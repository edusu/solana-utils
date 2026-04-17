use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use error_stack::{Report, ResultExt};
use serde::{Deserialize, Serialize};
use solana_keypair::{Keypair, Signature};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_transaction::versioned::VersionedTransaction;

use crate::error::{RpcError, RpcResult};

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
    pub async fn order(&self, request: &OrderRequest) -> RpcResult<OrderResponse> {
        let url = format!("{}/order", self.base_url);

        let response = self
            .http
            .get(&url)
            .header("x-api-key", &self.api_key)
            .query(request)
            .send()
            .await
            .change_context(RpcError::Network)
            .attach(format!("Failed to call Jupiter {}", url))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .change_context(RpcError::Network)
            .attach("Failed to read Jupiter /order response body")?;

        if !status.is_success() {
            return Err(Report::new(RpcError::Provider)
                .attach(format!("Jupiter /order returned HTTP {}: {}", status, body)));
        }

        serde_json::from_str::<OrderResponse>(&body)
            .change_context(RpcError::Parse)
            .attach(format!("Failed to parse Jupiter /order response: {}", body))
    }

    /// Fetches USD prices and decimals for a batch of mints via Jupiter's Price v3 API.
    ///
    /// Mints that Jupiter cannot quote (no liquidity, dead pool, frozen, ...) are
    /// simply absent from the returned map, so callers can use "present" as a
    /// signal for "routeable" and "missing" for "likely dead".
    ///
    /// Entries are also skipped when Jupiter returns metadata but no `usdPrice`
    /// (known token without a reliable price) or is missing `decimals` — either
    /// case leaves the caller unable to compute a USD value and should be treated
    /// the same as "no price".
    ///
    /// Large batches are automatically split to respect the API's 50-mint limit.
    ///
    /// # Parameters
    ///
    /// * `mints` - Mint addresses to quote. Duplicates are de-duplicated by the API.
    ///
    /// # Returns
    ///
    /// A map from mint to [`PriceInfo`]. Empty on empty input.
    pub async fn prices(&self, mints: &[Pubkey]) -> RpcResult<HashMap<Pubkey, PriceInfo>> {
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
                .change_context(RpcError::Network)
                .attach(format!("Failed to call Jupiter {}", self.price_base_url))?;

            let status = response.status();
            let body = response
                .text()
                .await
                .change_context(RpcError::Network)
                .attach("Failed to read Jupiter /price response body")?;

            if !status.is_success() {
                return Err(Report::new(RpcError::Provider).attach(format!(
                    "Jupiter /price returned HTTP {}: {}",
                    status, body
                )));
            }

            // v3 returns a flat map `{ "<mint>": { "usdPrice": 1.23, ... }, ... }`.
            // The entry itself is present even for mints Jupiter knows but cannot
            // price — in that case `usdPrice` is absent — so treat "no `usdPrice`"
            // as "no price", identical to the route-failure path downstream.
            let parsed: HashMap<String, PriceEntry> = serde_json::from_str(&body)
                .change_context(RpcError::Parse)
                .attach(format!("Failed to parse Jupiter /price response: {}", body))?;

            for (mint_str, entry) in parsed {
                let (Some(usd_price), Some(decimals)) = (entry.usd_price, entry.decimals) else {
                    continue;
                };
                let Ok(mint) = mint_str.parse::<Pubkey>() else {
                    tracing::warn!("Jupiter /price returned unparseable mint {}", mint_str);
                    continue;
                };
                out.insert(mint, PriceInfo { usd_price, decimals });
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
pub fn decode_transaction(transaction_b64: &str) -> RpcResult<VersionedTransaction> {
    let bytes = BASE64
        .decode(transaction_b64)
        .change_context(RpcError::Parse)
        .attach("Failed to base64-decode Jupiter transaction")?;

    bincode::deserialize::<VersionedTransaction>(&bytes)
        .change_context(RpcError::Parse)
        .attach("Failed to bincode-deserialize Jupiter transaction")
}

/// Signs the message of a Jupiter-assembled `VersionedTransaction` with the taker keypair.
///
/// Jupiter returns the transaction with placeholder signatures matching the message
/// header, so rebuilding it through `VersionedTransaction::try_new` replaces them with
/// real signatures produced by `signer`.
///
/// # Parameters
///
/// * `transaction` - The decoded transaction returned by [`decode_transaction`].
/// * `signer` - Keypair that owns the `taker` wallet passed in the order request.
pub fn sign_transaction(
    transaction: VersionedTransaction,
    signer: &Keypair,
) -> RpcResult<VersionedTransaction> {
    VersionedTransaction::try_new(transaction.message, &[signer])
        .change_context(RpcError::Internal)
        .attach("Failed to sign Jupiter transaction")
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
) -> RpcResult<(VersionedTransaction, OrderResponse)> {
    let order = client.order(request).await?;

    let transaction_b64 = order.transaction.as_deref().ok_or_else(|| {
        Report::new(RpcError::Parse).attach(
            "Jupiter /order did not return an assembled transaction; was `taker` provided?",
        )
    })?;

    let decoded = decode_transaction(transaction_b64)?;
    let signed = sign_transaction(decoded, signer)?;

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
) -> RpcResult<(Signature, OrderResponse)> {
    let (signed_tx, order) = fetch_signed_swap_transaction(jupiter, request, signer).await?;

    let signature = rpc
        .send_and_confirm_transaction(&signed_tx)
        .await
        .change_context(RpcError::Provider)
        .attach("Failed to confirm Jupiter swap transaction via RPC")?;

    Ok((signature, order))
}
