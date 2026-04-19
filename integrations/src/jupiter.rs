use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use error_stack::{Report, ResultExt};
use rpc::tx::sign_versioned_transaction;
use serde::{Deserialize, Serialize};
use solana_keypair::{Keypair, Signature};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_transaction::versioned::VersionedTransaction;

use crate::error::{IntegrationError, IntegrationResult};

/// Default host for the production Jupiter Swap v2 API.
pub const DEFAULT_BASE_URL: &str = "https://api.jup.ag/swap/v2";

/// Default host for the production Jupiter Price v3 API.
pub const DEFAULT_PRICE_BASE_URL: &str = "https://api.jup.ag/price/v3";

/// Maximum number of mints accepted by Jupiter's Price v3 API per request.
const PRICE_BATCH_LIMIT: usize = 50;

/// HTTP client for the Jupiter Swap v2 `/order` and Price v2 endpoints.
pub struct JupiterClient {
    http: reqwest::Client,
    base_url: String,
    price_base_url: String,
    api_key: String,
}

impl JupiterClient {
    /// Builds a client targeting [`DEFAULT_BASE_URL`] + [`DEFAULT_PRICE_BASE_URL`]
    /// with the given API key.
    ///
    /// # Parameters
    ///
    /// * `api_key` - Value forwarded as the `x-api-key` header on every request.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            http: reqwest::Client::new(),
            base_url: DEFAULT_BASE_URL.to_string(),
            price_base_url: DEFAULT_PRICE_BASE_URL.to_string(),
            api_key: api_key.into(),
        }
    }

    /// Overrides the swap base URL (e.g. to point at `lite-api.jup.ag` or a mock server).
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = base_url.into();
        self
    }

    /// Overrides the price base URL.
    pub fn with_price_base_url(mut self, price_base_url: impl Into<String>) -> Self {
        self.price_base_url = price_base_url.into();
        self
    }

    /// Calls `GET /order` and returns the parsed response, including the base64
    /// transaction blob to be signed by the taker.
    ///
    /// # Parameters
    ///
    /// * `request` - Swap parameters (input/output mint, amount, taker, ...).
    ///
    /// # Returns
    ///
    /// The deserialized [`OrderResponse`] as delivered by Jupiter.
    pub async fn order(&self, request: &OrderRequest) -> IntegrationResult<OrderResponse> {
        let url = format!("{}/order", self.base_url);

        let response = self
            .http
            .get(&url)
            .header("x-api-key", &self.api_key)
            .query(request)
            .send()
            .await
            .change_context(IntegrationError::Network)
            .attach(format!("Failed to call Jupiter {}", url))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .change_context(IntegrationError::Network)
            .attach("Failed to read Jupiter /order response body")?;

        if !status.is_success() {
            return Err(Report::new(IntegrationError::Provider)
                .attach(format!("Jupiter /order returned HTTP {}: {}", status, body)));
        }

        serde_json::from_str::<OrderResponse>(&body)
            .change_context(IntegrationError::Parse)
            .attach(format!("Failed to parse Jupiter /order response: {}", body))
    }

    /// Fetches USD prices and decimals for a batch of mints via Jupiter's Price v3 API.
    ///
    /// # Parameters
    ///
    /// * `mints` - Mint addresses to quote. Duplicates are de-duplicated by the API.
    ///
    /// # Returns
    ///
    /// A map from mint to [`PriceInfo`]. Empty on empty input.
    pub async fn prices(&self, mints: &[Pubkey]) -> IntegrationResult<HashMap<Pubkey, PriceInfo>> {
        if mints.is_empty() {
            return Ok(HashMap::new());
        }

        let mut out: HashMap<Pubkey, PriceInfo> = HashMap::with_capacity(mints.len());

        for chunk in mints.chunks(PRICE_BATCH_LIMIT) {
            let ids = chunk
                .iter()
                .map(|mint| mint.to_string())
                .collect::<Vec<_>>()
                .join(",");

            let response = self
                .http
                .get(&self.price_base_url)
                .header("x-api-key", &self.api_key)
                .query(&[("ids", ids.as_str())])
                .send()
                .await
                .change_context(IntegrationError::Network)
                .attach(format!("Failed to call Jupiter {}", self.price_base_url))?;

            let status = response.status();
            let body = response
                .text()
                .await
                .change_context(IntegrationError::Network)
                .attach("Failed to read Jupiter /price response body")?;

            if !status.is_success() {
                return Err(Report::new(IntegrationError::Provider)
                    .attach(format!("Jupiter /price returned HTTP {}: {}", status, body)));
            }

            // v3 returns a flat map `{ "<mint>": { "usdPrice": 1.23, ... }, ... }`.
            // The entry itself is present even for mints Jupiter knows but cannot
            // price — in that case `usdPrice` is absent — so treat "no `usdPrice`"
            // as "no price", identical to the route-failure path downstream.
            let parsed: HashMap<String, PriceEntry> = serde_json::from_str(&body)
                .change_context(IntegrationError::Parse)
                .attach(format!("Failed to parse Jupiter /price response: {}", body))?;

            for (mint_str, entry) in parsed {
                let (Some(usd_price), Some(decimals)) = (entry.usd_price, entry.decimals) else {
                    continue;
                };
                let Ok(mint) = mint_str.parse::<Pubkey>() else {
                    tracing::warn!("Jupiter /price returned unparseable mint {}", mint_str);
                    continue;
                };
                out.insert(
                    mint,
                    PriceInfo {
                        usd_price,
                        decimals,
                    },
                );
            }
        }

        Ok(out)
    }
}

/// USD price + decimals for one mint, as returned by [`JupiterClient::prices`].
///
/// `decimals` comes from Jupiter's indexed metadata so callers computing USD
/// values don't need a separate on-chain round-trip to the mint.
// Derives are used so callers can inspect and clone the value without extra plumbing.
#[derive(Debug, Clone, Copy)]
pub struct PriceInfo {
    pub usd_price: f64,
    pub decimals: u8,
}

/// Price v3 response entry. Fields other than `usdPrice` and `decimals` are
/// ignored by this client.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PriceEntry {
    usd_price: Option<f64>,
    decimals: Option<u8>,
}

/// Parameters accepted by `GET /order`.
///
/// Optional fields set to `None` are skipped from the query string, which keeps
/// Jupiter in its default "ultra" routing mode.
// Derives are used so requests can be cloned and compared in tests, and serialized
// straight into the query string by `reqwest::RequestBuilder::query`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderRequest {
    /// Mint of the token being sold (base58 address).
    pub input_mint: String,
    /// Mint of the token being bought (base58 address).
    pub output_mint: String,
    /// Amount of `input_mint` in its smallest unit (lamports for SOL, raw units for SPL).
    pub amount: u64,
    /// Wallet that will sign and receive the transaction.
    pub taker: String,
    /// Optional alternative recipient of the output tokens.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub receiver: Option<String>,
    /// Optional referral account for fee collection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referral_account: Option<String>,
    /// Optional referral fee in basis points (50..=255).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub referral_fee: Option<u16>,
    /// Optional integrator paying gas for the taker.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payer: Option<String>,
    /// Optional slippage tolerance in basis points.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub slippage_bps: Option<u16>,
    /// Optional comma-separated list of router names to exclude (e.g. `"jupiterz,dflow"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exclude_routers: Option<String>,
}

impl OrderRequest {
    /// Convenience constructor covering the required set of parameters.
    pub fn new(
        input_mint: impl Into<String>,
        output_mint: impl Into<String>,
        amount: u64,
        taker: impl Into<String>,
    ) -> Self {
        Self {
            input_mint: input_mint.into(),
            output_mint: output_mint.into(),
            amount,
            taker: taker.into(),
            receiver: None,
            referral_account: None,
            referral_fee: None,
            payer: None,
            slippage_bps: None,
            exclude_routers: None,
        }
    }
}

/// Response payload from `GET /order`.
// Derives are used so responses can be logged and cloned between pipeline stages.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrderResponse {
    /// Base64-encoded `VersionedTransaction` to be signed. `None` if `taker` was omitted.
    pub transaction: Option<String>,
    /// Opaque identifier returned by Jupiter (carried for logging/observability).
    pub request_id: String,
    /// Expected output amount in the smallest unit of `outputMint`.
    ///
    /// Kept as a string to match Jupiter's wire format and avoid losing precision on
    /// u64 values that exceed JavaScript's safe-integer range.
    pub out_amount: String,
    /// Router that produced the quote (e.g. `"iris"`, `"jupiterz"`, ...).
    pub router: Option<String>,
    /// Routing mode, either `"ultra"` or `"manual"`.
    pub mode: Option<String>,
    /// Integrator fee applied by Jupiter itself, in basis points.
    pub fee_bps: Option<u16>,
    /// Mint of the token in which the Jupiter fee is charged.
    pub fee_mint: Option<String>,
    /// Optional platform/referral fee breakdown.
    pub platform_fee: Option<PlatformFee>,
}

/// Platform/referral fee breakdown returned inside [`OrderResponse`].
///
/// `amount` is optional because some routers (notably OKX) omit it and only send
/// `fee_bps` + `fee_mint`.
// Derives are used so the nested fee block can be inspected and cloned like the parent response.
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlatformFee {
    pub amount: Option<String>,
    pub fee_bps: u16,
    pub fee_mint: String,
}

/// Decodes the base64 `transaction` field from an `/order` response into a `VersionedTransaction`.
///
/// # Parameters
///
/// * `transaction_b64` - The `OrderResponse::transaction` value as returned by Jupiter.
pub fn decode_transaction(transaction_b64: &str) -> IntegrationResult<VersionedTransaction> {
    let bytes = BASE64
        .decode(transaction_b64)
        .change_context(IntegrationError::Parse)
        .attach("Failed to base64-decode Jupiter transaction")?;

    bincode::deserialize::<VersionedTransaction>(&bytes)
        .change_context(IntegrationError::Parse)
        .attach("Failed to bincode-deserialize Jupiter transaction")
}

/// Fetches a Jupiter order and signs its transaction, returning the ready-to-submit
/// `VersionedTransaction`.
///
/// This is the building block for submitting the swap through your own RPC: the
/// resulting value can be passed straight to
/// [`RpcClient::send_and_confirm_transaction`] or to any other sender that
/// accepts `SerializableTransaction`.
///
/// # Parameters
///
/// * `client` - Configured [`JupiterClient`].
/// * `request` - Swap parameters. `request.taker` must match `signer.pubkey()`.
/// * `signer` - Keypair used to sign the assembled transaction.
///
/// # Returns
///
/// The signed [`VersionedTransaction`] alongside the original `/order` response.
pub async fn fetch_signed_swap_transaction(
    client: &JupiterClient,
    request: &OrderRequest,
    signer: &Keypair,
) -> IntegrationResult<(VersionedTransaction, OrderResponse)> {
    let order = client.order(request).await?;

    let transaction_b64 = order.transaction.as_deref().ok_or_else(|| {
        Report::new(IntegrationError::Jupiter)
            .attach("Jupiter /order did not return an assembled transaction; was `taker` provided?")
    })?;

    let decoded = decode_transaction(transaction_b64)?;
    // Signing lives in the `rpc` crate so it can be reused by non-Jupiter pipelines;
    // any RpcError it surfaces is re-tagged into IntegrationError at the boundary.
    let signed = sign_versioned_transaction(decoded, signer)
        .change_context(IntegrationError::Internal)
        .attach("Failed to sign Jupiter transaction")?;

    Ok((signed, order))
}

/// One-shot helper that fetches an order, signs it, and submits the result through
/// the caller's own RPC endpoint.
///
/// Submission uses `send_and_confirm_transaction`, which polls for confirmation
/// internally; for fire-and-forget behavior reach for
/// [`fetch_signed_swap_transaction`] and call `send_transaction` yourself.
///
/// # Parameters
///
/// * `jupiter` - Configured [`JupiterClient`] for the `/order` call.
/// * `rpc` - Solana RPC client used to land the signed transaction.
/// * `request` - Swap parameters. `request.taker` must match `signer.pubkey()`.
/// * `signer` - Keypair that signs the assembled transaction.
///
/// # Returns
///
/// The confirmed transaction signature and the `/order` response (handy for
/// logging `out_amount`, `request_id` or the chosen `router`).
pub async fn swap_via_rpc(
    jupiter: &JupiterClient,
    rpc: &RpcClient,
    request: &OrderRequest,
    signer: &Keypair,
) -> IntegrationResult<(Signature, OrderResponse)> {
    let (signed_tx, order) = fetch_signed_swap_transaction(jupiter, request, signer).await?;

    let signature = rpc
        .send_and_confirm_transaction(&signed_tx)
        .await
        .change_context(IntegrationError::Provider)
        .attach("Failed to confirm Jupiter swap transaction via RPC")?;

    Ok((signature, order))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rpc::spl::utils::SOLANA_WRAPPED_MINT as WSOL_MINT;
    use solana_keypair::Signer;
    use std::sync::OnceLock;

    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    // Jupiter does not validate that the taker actually holds a balance to produce
    // an /order response, so any valid pubkey works as a placeholder.
    const TAKER_PLACEHOLDER: &str = "JUPyiwrYJFskUPiHa7hkeR8VUtAeFoSYbKedZNsDvCN";

    /// Runs the real `reqwest` query serializer over `request` and returns the
    /// resulting `key -> value` map — lets tests assert on the wire format
    /// without duplicating serialization logic.
    fn query_pairs(request: &OrderRequest) -> HashMap<String, String> {
        // `reqwest::Client::new()` spins up a TLS config + DNS resolver on each call,
        // so share one across tests.
        static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
        let built = CLIENT
            .get_or_init(reqwest::Client::new)
            .get("https://example.invalid/order")
            .query(request)
            .build()
            .unwrap();
        built.url().query_pairs().into_owned().collect()
    }

    #[test]
    fn order_request_serializes_required_fields_as_camel_case() {
        let req = OrderRequest::new(WSOL_MINT, USDC_MINT, 1_000_000, TAKER_PLACEHOLDER);
        let q = query_pairs(&req);

        assert_eq!(q.get("inputMint").map(String::as_str), Some(WSOL_MINT));
        assert_eq!(q.get("outputMint").map(String::as_str), Some(USDC_MINT));
        assert_eq!(q.get("amount").map(String::as_str), Some("1000000"));
        assert_eq!(q.get("taker").map(String::as_str), Some(TAKER_PLACEHOLDER));
    }

    #[test]
    fn order_request_omits_none_optional_fields() {
        let req = OrderRequest::new(WSOL_MINT, USDC_MINT, 1, TAKER_PLACEHOLDER);
        let q = query_pairs(&req);

        for key in [
            "slippageBps",
            "receiver",
            "referralAccount",
            "referralFee",
            "payer",
            "excludeRouters",
        ] {
            assert!(!q.contains_key(key), "unexpected key {key} in query string");
        }
    }

    #[test]
    fn order_request_includes_optional_fields_when_set() {
        let mut req = OrderRequest::new(WSOL_MINT, USDC_MINT, 1, TAKER_PLACEHOLDER);
        req.slippage_bps = Some(150);
        req.exclude_routers = Some("okx,dflow".into());
        req.referral_fee = Some(50);
        let q = query_pairs(&req);

        assert_eq!(q.get("slippageBps").map(String::as_str), Some("150"));
        assert_eq!(
            q.get("excludeRouters").map(String::as_str),
            Some("okx,dflow")
        );
        assert_eq!(q.get("referralFee").map(String::as_str), Some("50"));
    }

    #[test]
    fn order_response_parses_iris_payload_with_platform_fee_amount() {
        let body = r#"{
            "transaction": "AQID",
            "requestId": "req-iris-1",
            "outAmount": "987654321",
            "router": "iris",
            "mode": "ultra",
            "feeBps": 5,
            "feeMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",
            "platformFee": {
                "amount": "1234",
                "feeBps": 10,
                "feeMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            }
        }"#;
        let parsed: OrderResponse = serde_json::from_str(body).unwrap();

        assert_eq!(parsed.request_id, "req-iris-1");
        assert_eq!(parsed.out_amount, "987654321");
        assert_eq!(parsed.router.as_deref(), Some("iris"));
        assert_eq!(parsed.transaction.as_deref(), Some("AQID"));

        let fee = parsed.platform_fee.unwrap();
        assert_eq!(fee.amount.as_deref(), Some("1234"));
        assert_eq!(fee.fee_bps, 10);
    }

    #[test]
    fn order_response_parses_okx_payload_without_platform_fee_amount() {
        let body = r#"{
            "transaction": null,
            "requestId": "req-okx-1",
            "outAmount": "42",
            "router": "okx",
            "platformFee": {
                "feeBps": 10,
                "feeMint": "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v"
            }
        }"#;
        let parsed: OrderResponse = serde_json::from_str(body).unwrap();

        assert!(parsed.transaction.is_none());
        let fee = parsed.platform_fee.unwrap();
        assert!(fee.amount.is_none(), "amount must stay optional");
        assert_eq!(fee.fee_bps, 10);
    }

    #[test]
    fn price_entry_accepts_entry_with_price_and_decimals() {
        let entry: PriceEntry =
            serde_json::from_str(r#"{ "usdPrice": 1.23, "decimals": 6 }"#).unwrap();
        assert_eq!(entry.usd_price, Some(1.23));
        assert_eq!(entry.decimals, Some(6));
    }

    // Price v3 returns metadata-only entries for known mints with no reliable
    // price (e.g. just `createdAt` + `decimals`). Those must parse cleanly and
    // be filtered out downstream, not crash the client.
    #[test]
    fn price_entry_accepts_entry_without_usd_price() {
        let entry: PriceEntry =
            serde_json::from_str(r#"{ "decimals": 9, "createdAt": "2024-01-01" }"#).unwrap();
        assert!(entry.usd_price.is_none());
        assert_eq!(entry.decimals, Some(9));
    }

    #[test]
    fn price_entry_accepts_entry_without_decimals() {
        let entry: PriceEntry = serde_json::from_str(r#"{ "usdPrice": 0.5 }"#).unwrap();
        assert_eq!(entry.usd_price, Some(0.5));
        assert!(entry.decimals.is_none());
    }

    // `prices([])` must short-circuit before issuing any HTTP request; if it
    // didn't, this test would hang or error trying to resolve `127.0.0.1:1`.
    #[tokio::test]
    async fn prices_empty_input_skips_http_call() {
        let client = JupiterClient::new("").with_price_base_url("http://127.0.0.1:1");
        let out = client
            .prices(&[])
            .await
            .expect("empty input should return Ok(empty map) without network IO");
        assert!(out.is_empty());
    }

    #[test]
    fn decode_transaction_rejects_invalid_base64() {
        let Err(err) = decode_transaction("not base64!!!") else {
            panic!("invalid base64 should fail");
        };
        assert!(matches!(err.current_context(), IntegrationError::Parse));
    }

    #[test]
    fn decode_transaction_rejects_non_transaction_bytes() {
        let Err(err) = decode_transaction("AQID") else {
            panic!("non-transaction bytes should fail");
        };
        assert!(matches!(err.current_context(), IntegrationError::Parse));
    }

    // Live integration tests hit the real Jupiter API; run with:
    //     JUP_API_KEY=... cargo test -p integrations -- --ignored --nocapture
    // When `JUP_API_KEY` is unset they return early instead of failing, so
    // `cargo test -- --ignored` stays green on machines without a key.

    fn live_client() -> Option<JupiterClient> {
        std::env::var("JUP_API_KEY").ok().map(JupiterClient::new)
    }

    #[tokio::test]
    #[ignore = "hits real Jupiter API"]
    async fn live_order_quote_sol_to_usdc() {
        let Some(client) = live_client() else {
            eprintln!("JUP_API_KEY not set — skipping live test");
            return;
        };

        // A freshly generated keypair is enough: Jupiter only checks that `taker`
        // is a valid base58 pubkey to produce the quote. 0.001 SOL input keeps
        // the quote small and predictable.
        let taker = Keypair::new();
        let request = OrderRequest {
            slippage_bps: Some(50),
            ..OrderRequest::new(WSOL_MINT, USDC_MINT, 1_000_000, taker.pubkey().to_string())
        };

        let response = client
            .order(&request)
            .await
            .expect("live /order should return a quote for SOL → USDC");

        assert!(
            !response.request_id.is_empty(),
            "requestId must be populated"
        );
        assert!(
            response.transaction.is_some(),
            "transaction must be present when taker is provided"
        );
        let out: u64 = response
            .out_amount
            .parse()
            .expect("outAmount must be a numeric string");
        assert!(out > 0, "out_amount must be > 0 for a liquid pair");
    }

    #[tokio::test]
    #[ignore = "hits real Jupiter API"]
    async fn live_prices_returns_usdc_and_sol() {
        let Some(client) = live_client() else {
            eprintln!("JUP_API_KEY not set — skipping live test");
            return;
        };

        let wsol: Pubkey = WSOL_MINT.parse().unwrap();
        let usdc: Pubkey = USDC_MINT.parse().unwrap();
        let prices = client
            .prices(&[wsol, usdc])
            .await
            .expect("live /price/v3 should return USD prices for WSOL + USDC");

        let sol = prices.get(&wsol).expect("WSOL must be priced by Jupiter");
        let stable = prices.get(&usdc).expect("USDC must be priced by Jupiter");
        assert!(sol.usd_price > 0.0 && sol.decimals == 9);
        // USDC peg can drift a few bps; sanity bound rather than exact equality.
        assert!(
            (0.9..=1.1).contains(&stable.usd_price),
            "USDC usd_price should stay within ±10% of the peg, got {}",
            stable.usd_price
        );
        assert_eq!(stable.decimals, 6);
    }
}
