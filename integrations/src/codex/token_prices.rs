use std::collections::HashMap;

use error_stack::ResultExt;
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::codex::client::CodexClient;
use crate::codex::gql::GqlRequest;
use crate::error::{IntegrationError, IntegrationResult};

/// Maximum number of inputs accepted by one `getTokenPrices` call.
///
/// Per the Codex docs, anything beyond this is silently truncated. Exposed as
/// a `const` so it can be bumped in a single place if the real cap changes.
pub const MAX_INPUTS_PER_QUERY: usize = 25;

/// Maximum number of aliased queries packed into a single GraphQL HTTP request.
///
/// With the default of 5, one HTTP round-trip carries up to
/// `MAX_INPUTS_PER_QUERY * MAX_ALIASES_PER_REQUEST` = 125 inputs.
pub const MAX_ALIASES_PER_REQUEST: usize = 5;

/// Input tuple accepted by `getTokenPrices`.
// Derives used so callers can build inputs off the async path, clone them for
// retries, and have serde render them as the `GetPriceInput` shape Codex expects.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPriceInput {
    /// Contract address of the token.
    pub address: String,
    /// Network ID as published by Codex (e.g. 1399811149 for Solana).
    pub network_id: i64,
    /// Optional Unix timestamp; when omitted, Codex returns the latest price.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timestamp: Option<i64>,
    /// Optional pool address to query a specific venue instead of the top pool.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pool_address: Option<String>,
}

impl TokenPriceInput {
    /// Convenience constructor covering the required set of fields.
    pub fn new(address: impl Into<String>, network_id: i64) -> Self {
        Self {
            address: address.into(),
            network_id,
            timestamp: None,
            pool_address: None,
        }
    }
}

/// One `Price` entry returned by `getTokenPrices`.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPrice {
    /// Contract address the price is for.
    pub address: String,
    /// Network ID the address is deployed on.
    pub network_id: i64,
    /// USD price at the (optional) timestamp.
    pub price_usd: f64,
    /// Unix timestamp of the quoted price.
    pub timestamp: Option<i64>,
}

impl CodexClient {
    /// Fetches USD prices for a batch of `(address, networkId)` tuples.
    ///
    /// Behaviour:
    ///
    /// * Empty input returns `Ok(vec![])` without issuing any HTTP request.
    /// * Inputs are split into chunks of up to [`MAX_INPUTS_PER_QUERY`]; up to
    ///   [`MAX_ALIASES_PER_REQUEST`] chunks are packed into a single GraphQL
    ///   HTTP request via aliased queries.
    /// * Entries for which Codex returns `null` (unknown / unpriced) are
    ///   dropped silently — consistent with how `JupiterClient::prices`
    ///   reports routable vs. unroutable mints via presence/absence.
    /// * Output is ordered: within each aliased response Codex preserves input
    ///   order, and aliases are flattened in ascending index order.
    pub async fn token_prices(
        &self,
        inputs: &[TokenPriceInput],
    ) -> IntegrationResult<Vec<TokenPrice>> {
        if inputs.is_empty() {
            return Ok(Vec::new());
        }

        let chunks = chunk_inputs(inputs, MAX_INPUTS_PER_QUERY);
        let mut out: Vec<TokenPrice> = Vec::with_capacity(inputs.len());

        for http_batch in chunks.chunks(MAX_ALIASES_PER_REQUEST) {
            let query = build_batch_query(http_batch.len());

            // Variables are a flat JSON object `{ "in0": [...], "in1": [...], ... }`.
            let mut variables = Map::with_capacity(http_batch.len());
            for (idx, chunk) in http_batch.iter().enumerate() {
                variables.insert(
                    format!("in{idx}"),
                    serde_json::to_value(chunk)
                        .change_context(IntegrationError::Parse)
                        .attach("Failed to serialize Codex TokenPriceInput batch")?,
                );
            }

            let request = GqlRequest {
                query,
                variables: Some(Value::Object(variables)),
                operation_name: Some("TokenPricesBatch".into()),
            };

            // Each alias deserialises into `Vec<Option<TokenPrice>>`: Codex's
            // `[Price]` return type allows null elements for unknown tokens.
            let data: HashMap<String, Vec<Option<TokenPrice>>> = self.post_gql(&request).await?;

            // Flatten in alias order to preserve overall input ordering.
            for idx in 0..http_batch.len() {
                let key = format!("q{idx}");
                let Some(entries) = data.get(&key) else {
                    tracing::warn!("Codex response missing alias {}", key);
                    continue;
                };
                out.extend(entries.iter().flatten().cloned());
            }
        }

        Ok(out)
    }
}

/// Splits `inputs` into consecutive slices of at most `chunk_size` elements.
///
/// Separated from the async path so it is trivial to unit-test. Returned
/// slices borrow from `inputs`; callers own the lifetime.
fn chunk_inputs(inputs: &[TokenPriceInput], chunk_size: usize) -> Vec<&[TokenPriceInput]> {
    inputs.chunks(chunk_size).collect()
}

/// Builds a GraphQL document with `alias_count` aliased `getTokenPrices`
/// selections sharing the operation `TokenPricesBatch`.
///
/// Pure function of `alias_count`, so unit tests can assert on the emitted
/// string without mocking HTTP.
fn build_batch_query(alias_count: usize) -> String {
    debug_assert!(alias_count > 0, "alias_count must be > 0");

    let mut declarations = String::new();
    let mut selections = String::new();

    for idx in 0..alias_count {
        if idx > 0 {
            declarations.push_str(", ");
        }
        declarations.push_str(&format!("$in{idx}: [GetPriceInput!]!"));

        selections.push_str(&format!(
            "  q{idx}: getTokenPrices(inputs: $in{idx}) {{ address networkId priceUsd timestamp }}\n"
        ));
    }

    format!(
        "query TokenPricesBatch({declarations}) {{\n{selections}}}",
        declarations = declarations,
        selections = selections,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const SOLANA_NETWORK_ID: i64 = 1399811149;
    const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

    fn make_inputs(count: usize) -> Vec<TokenPriceInput> {
        (0..count)
            .map(|i| TokenPriceInput::new(format!("addr{i}"), SOLANA_NETWORK_ID))
            .collect()
    }

    #[test]
    fn chunk_inputs_handles_boundary_sizes() {
        for (total, expected_lens) in [
            (0usize, vec![]),
            (1, vec![1]),
            (25, vec![25]),
            (26, vec![25, 1]),
            (30, vec![25, 5]),
            (125, vec![25, 25, 25, 25, 25]),
            (126, vec![25, 25, 25, 25, 25, 1]),
        ] {
            let inputs = make_inputs(total);
            let chunks = chunk_inputs(&inputs, MAX_INPUTS_PER_QUERY);
            let got: Vec<usize> = chunks.iter().map(|c| c.len()).collect();
            assert_eq!(got, expected_lens, "mismatch for total={total}");
            println!("chunk_inputs({total:>3}) -> {got:?}");
        }
    }

    #[test]
    fn build_batch_query_single_alias_has_only_q0() {
        let q = build_batch_query(1);
        assert!(q.contains("$in0: [GetPriceInput!]!"), "query: {q}");
        assert!(q.contains("q0: getTokenPrices(inputs: $in0)"));
        assert!(!q.contains("q1:"));
        assert!(!q.contains("$in1"));
        println!("build_batch_query(1):\n{q}");
    }

    #[test]
    fn build_batch_query_three_aliases_enumerates_all() {
        let q = build_batch_query(3);
        for idx in 0..3 {
            assert!(
                q.contains(&format!("$in{idx}: [GetPriceInput!]!")),
                "query: {q}"
            );
            assert!(q.contains(&format!("q{idx}: getTokenPrices(inputs: $in{idx})")));
        }
        assert!(!q.contains("q3:"));
        println!("build_batch_query(3):\n{q}");
    }

    // Pointing at 127.0.0.1:1 would refuse any connection; asserting `Ok` proves
    // no IO happened on empty input, matching `JupiterClient::prices`'s pattern.
    #[tokio::test]
    async fn token_prices_empty_input_skips_http_call() {
        let client = CodexClient::new("").with_endpoint("http://127.0.0.1:1");
        let out = client
            .token_prices(&[])
            .await
            .expect("empty input should return Ok(empty vec) without network IO");
        assert!(out.is_empty());
        println!(
            "token_prices(&[]) -> Ok(vec![]) len={} (no HTTP issued, endpoint was 127.0.0.1:1)",
            out.len()
        );
    }

    #[test]
    fn token_price_input_serializes_as_camel_case_and_omits_none() {
        let input = TokenPriceInput::new(WSOL_MINT, SOLANA_NETWORK_ID);
        let json = serde_json::to_value(&input).unwrap();

        assert_eq!(json["address"], WSOL_MINT);
        assert_eq!(json["networkId"], SOLANA_NETWORK_ID);
        assert!(json.get("timestamp").is_none(), "timestamp must be omitted");
        assert!(
            json.get("poolAddress").is_none(),
            "poolAddress must be omitted"
        );
        println!(
            "TokenPriceInput::new(..) -> {}",
            serde_json::to_string(&json).unwrap()
        );
    }

    #[test]
    fn token_price_input_serializes_optional_fields_when_set() {
        let input = TokenPriceInput {
            address: WSOL_MINT.into(),
            network_id: SOLANA_NETWORK_ID,
            timestamp: Some(1_700_000_000),
            pool_address: Some("poolXYZ".into()),
        };
        let json = serde_json::to_value(&input).unwrap();

        assert_eq!(json["timestamp"], 1_700_000_000);
        assert_eq!(json["poolAddress"], "poolXYZ");
        println!(
            "TokenPriceInput {{ all fields set }} -> {}",
            serde_json::to_string(&json).unwrap()
        );
    }

    #[test]
    fn token_price_deserializes_camel_case_payload() {
        let body = r#"{
            "address": "So11111111111111111111111111111111111111112",
            "networkId": 1399811149,
            "priceUsd": 87.91,
            "timestamp": 1776408345
        }"#;
        let price: TokenPrice = serde_json::from_str(body).unwrap();

        assert_eq!(price.address, WSOL_MINT);
        assert_eq!(price.network_id, SOLANA_NETWORK_ID);
        assert!((price.price_usd - 87.91).abs() < 1e-6);
        assert_eq!(price.timestamp, Some(1_776_408_345));
        println!(
            "TokenPrice parsed:\n{}",
            serde_json::to_string_pretty(&price).unwrap()
        );
    }

    #[test]
    fn token_price_deserializes_with_null_timestamp() {
        let body = r#"{
            "address": "A",
            "networkId": 1,
            "priceUsd": 1.0,
            "timestamp": null
        }"#;
        let price: TokenPrice = serde_json::from_str(body).unwrap();
        assert!(price.timestamp.is_none());
        println!(
            "TokenPrice with null timestamp:\n{}",
            serde_json::to_string_pretty(&price).unwrap()
        );
    }

    // Live integration tests hit the real Codex API; run with:
    //     CODEX_API_KEY=... cargo test -p integrations codex -- --ignored --nocapture
    // When `CODEX_API_KEY` is unset they return early so `cargo test -- --ignored`
    // stays green on machines without a key.

    fn live_client() -> Option<CodexClient> {
        std::env::var("CODEX_API_KEY").ok().map(CodexClient::new)
    }

    #[tokio::test]
    #[ignore = "hits real Codex API"]
    async fn live_token_prices_solana_wsol_and_usdc() {
        let Some(client) = live_client() else {
            eprintln!("CODEX_API_KEY not set — skipping live test");
            return;
        };

        let inputs = vec![
            TokenPriceInput::new(WSOL_MINT, SOLANA_NETWORK_ID),
            TokenPriceInput::new(USDC_MINT, SOLANA_NETWORK_ID),
        ];

        let prices = client
            .token_prices(&inputs)
            .await
            .expect("live getTokenPrices should return WSOL + USDC");

        assert_eq!(prices.len(), 2, "expected one price per input");

        let wsol = prices
            .iter()
            .find(|p| p.address == WSOL_MINT)
            .expect("WSOL must be priced by Codex");
        assert!(wsol.price_usd > 0.0, "WSOL price must be > 0");

        let usdc = prices
            .iter()
            .find(|p| p.address == USDC_MINT)
            .expect("USDC must be priced by Codex");
        // USDC peg can drift a few bps; sanity bound rather than exact equality.
        assert!(
            (0.9..=1.1).contains(&usdc.price_usd),
            "USDC priceUsd should stay within ±10% of the peg, got {}",
            usdc.price_usd
        );

        println!(
            "Codex getTokenPrices response ({} entries):\n{}",
            prices.len(),
            serde_json::to_string_pretty(&prices).unwrap()
        );
    }

    // 30 inputs force two chunks (25 + 5) in a single HTTP request via aliases,
    // exercising the batching path end-to-end.
    #[tokio::test]
    #[ignore = "hits real Codex API"]
    async fn live_token_prices_chunking_over_25() {
        let Some(client) = live_client() else {
            eprintln!("CODEX_API_KEY not set — skipping live test");
            return;
        };

        // Duplicate two known-priced tokens to avoid flakiness from illiquid
        // tokens Codex might legitimately return as `null`.
        let mut inputs = Vec::with_capacity(30);
        for i in 0..30 {
            let mint = if i % 2 == 0 { WSOL_MINT } else { USDC_MINT };
            inputs.push(TokenPriceInput::new(mint, SOLANA_NETWORK_ID));
        }

        let prices = client
            .token_prices(&inputs)
            .await
            .expect("live batched getTokenPrices should succeed");

        assert!(
            !prices.is_empty(),
            "chunking path must return at least some priced entries"
        );
        for price in &prices {
            assert!(price.price_usd > 0.0, "no priced entry should be 0");
        }

        println!(
            "Codex chunked getTokenPrices: {} inputs -> {} priced entries (2 aliases in 1 HTTP)",
            inputs.len(),
            prices.len()
        );
        println!("{}", serde_json::to_string_pretty(&prices).unwrap());
    }
}
