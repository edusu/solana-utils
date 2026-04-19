use std::{collections::HashMap, path::PathBuf, str::FromStr, time::Duration};

use anchor_lang::prelude::Pubkey;
use clap::Args;
use error_stack::ResultExt;
use integrations::jupiter::{JupiterClient, OrderRequest, fetch_signed_swap_transaction};
use rpc::{
    rpc::create_rpc_client,
    spl::{
        burn::burn_and_close_token_accounts,
        close::{CloseableTokenAccount, close_token_accounts},
        tokens::list_wallet_token_accounts,
        utils::is_native_sol_mint,
    },
};
use solana_keypair::{Keypair, Signer};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::{
    config::Config,
    error::{CliError, CliResult},
};

/// Seconds to wait before each Jupiter `/order` call.
///
/// Applied on every iteration (including the first), so a burst of parse/route
/// failures cannot bypass the Jupiter rate limit.
const PRE_JUPITER_DELAY_SECS: u64 = 1;

/// Seconds to wait after each successfully confirmed swap transaction.
///
/// Adds a second layer of throttling on the happy path without penalising
/// iterations that failed before submitting a transaction.
const POST_TX_DELAY_SECS: u64 = 1;

/// A candidate Jupiter deemed routeable and worth the USD threshold.
#[derive(Debug, Clone, Copy)]
struct SwappableCandidate {
    account: CloseableTokenAccount,
    value_usd: f64,
}

/// A candidate that will be burned and closed instead of swapped.
///
/// `value_usd` is `None` when Jupiter has no price for the mint (dead pool,
/// frozen, unknown) and `Some(v)` when the mint is priced but below the USD
/// threshold — kept as extra context for the dry-run output.
#[derive(Debug, Clone, Copy)]
struct BurnableCandidate {
    account: CloseableTokenAccount,
    value_usd: Option<f64>,
}

/// Output of [`classify_candidates`].
#[derive(Debug, Default)]
struct Classification {
    swappable: Vec<SwappableCandidate>,
    burnable: Vec<BurnableCandidate>,
}

/// Output of [`verify_drained_accounts`]: accounts that reached zero vs. those
/// that still hold residual dust and therefore need burn+close.
#[derive(Debug, Default)]
struct DrainVerification {
    empty: Vec<CloseableTokenAccount>,
    dust: Vec<CloseableTokenAccount>,
}

/// Arguments for the `swap-all` subcommand.
#[derive(Args, Debug)]
pub struct SwapAllArgs {
    /// Path to the TOML config file that holds the swap whitelist.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,

    /// RPC endpoint to use.
    #[arg(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// Target mint to swap every other token into.
    #[arg(long)]
    target: String,

    /// Slippage tolerance (basis points) forwarded to Jupiter.
    #[arg(long, default_value_t = 50)]
    slippage_bps: u16,

    /// Minimum USD value to attempt a swap. Holdings worth less — and any mint
    /// for which Jupiter has no price — are burned and closed instead.
    #[arg(long, default_value_t = 0.01)]
    min_usd_value: f64,

    /// Destination pubkey for the reclaimed rent lamports. Defaults to the signer.
    #[arg(long)]
    destination: Option<String>,

    /// Skip both the close-accounts and burn+close steps.
    #[arg(long)]
    no_close: bool,

    /// Preview swaps and closes without submitting any transaction.
    #[arg(long)]
    dry_run: bool,
}

/// Entrypoint of the `swap-all` subcommand.
///
/// Discovers every token account owned by the wallet behind `SOLANA_PRIVATE_KEY`,
/// filters out native SOL, the target mint, and whitelisted mints, then swaps each
/// remaining balance to `--target` via Jupiter before reclaiming the drained
/// accounts' rent through `close_token_accounts`.
pub async fn run(args: SwapAllArgs) -> CliResult<()> {
    let config = Config::load(&args.config)?;
    let whitelist = config.swap_all.whitelist_pubkeys()?;
    tracing::info!(
        "Loaded {} whitelisted mint(s) from {}",
        whitelist.len(),
        args.config.display()
    );

    let target = Pubkey::from_str(&args.target)
        .change_context(CliError::Input)
        .attach(format!("Invalid --target mint {}", args.target))?;

    let private_key = std::env::var("SOLANA_PRIVATE_KEY")
        .change_context(CliError::Input)
        .attach("SOLANA_PRIVATE_KEY env var is required")?;
    let authority = Keypair::from_base58_string(&private_key);
    let owner = authority.pubkey();
    tracing::info!("Authority: {} | Target: {}", owner, target);

    let jup_api_key = std::env::var("JUP_API_KEY")
        .change_context(CliError::Input)
        .attach("JUP_API_KEY env var is required")?;
    let jupiter = JupiterClient::new(jup_api_key);

    let rpc = create_rpc_client(Some(args.rpc_url));

    let all_accounts = list_wallet_token_accounts(&rpc, &owner)
        .await
        .change_context(CliError::Rpc)?;
    tracing::info!("Discovered {} token account(s)", all_accounts.len());

    let candidates = select_swap_candidates(all_accounts, &whitelist, &target);
    tracing::info!("{} mint(s) to process", candidates.len());

    let classification = classify_candidates(candidates, &jupiter, args.min_usd_value).await?;

    tracing::info!(
        "Classification: {} swappable | {} to burn+close",
        classification.swappable.len(),
        classification.burnable.len()
    );

    if args.dry_run {
        for candidate in &classification.swappable {
            tracing::info!(
                "[dry-run] SWAP  mint={} amount={} value≈${:.6}",
                candidate.account.mint,
                candidate.account.amount_raw,
                candidate.value_usd,
            );
        }
        for candidate in &classification.burnable {
            match candidate.value_usd {
                Some(v) => tracing::info!(
                    "[dry-run] BURN  mint={} amount={} value≈${:.6} (below --min-usd-value={})",
                    candidate.account.mint,
                    candidate.account.amount_raw,
                    v,
                    args.min_usd_value,
                ),
                None => tracing::info!(
                    "[dry-run] BURN  mint={} amount={} (no Jupiter price — likely dead/frozen)",
                    candidate.account.mint,
                    candidate.account.amount_raw,
                ),
            }
        }
        return Ok(());
    }

    let destination = match args.destination.as_deref() {
        Some(raw) => Pubkey::from_str(raw)
            .change_context(CliError::Input)
            .attach(format!("Invalid --destination pubkey {}", raw))?,
        None => owner,
    };

    let swappable_accounts: Vec<CloseableTokenAccount> = classification
        .swappable
        .iter()
        .map(|candidate| candidate.account)
        .collect();
    let swapped = run_swap_loop(
        &jupiter,
        &rpc,
        &authority,
        owner,
        target,
        args.slippage_bps,
        &swappable_accounts,
    )
    .await;

    tracing::info!(
        "{}/{} swap(s) confirmed",
        swapped.len(),
        swappable_accounts.len()
    );

    if args.no_close {
        return Ok(());
    }

    // A successful swap TX can still leave dust behind (Token-2022 transfer
    // fees, route rounding, mid-swap incoming transfers). Trusting our stale
    // `amount_raw = 0` would crash `close_account` with error 0xb, so re-read
    // on-chain balances and split residual-dust accounts off to burn+close.
    let drain = verify_drained_accounts(&rpc, &owner, &swapped).await?;

    if !drain.empty.is_empty() {
        let signatures = close_token_accounts(&rpc, &authority, drain.empty, destination)
            .await
            .change_context(CliError::Rpc)?;
        tracing::info!("Submitted {} close batch transaction(s)", signatures.len());
        for (index, signature) in signatures.iter().enumerate() {
            tracing::info!("  close batch {}: {}", index + 1, signature);
        }
    }

    let mut all_burnable: Vec<CloseableTokenAccount> = classification
        .burnable
        .into_iter()
        .map(|candidate| candidate.account)
        .collect();
    all_burnable.extend(drain.dust);

    if !all_burnable.is_empty() {
        let signatures = burn_and_close_token_accounts(&rpc, &authority, all_burnable, destination)
            .await
            .change_context(CliError::Rpc)?;
        tracing::info!(
            "Submitted {} burn+close batch transaction(s)",
            signatures.len()
        );
        for (index, signature) in signatures.iter().enumerate() {
            tracing::info!("  burn+close batch {}: {}", index + 1, signature);
        }
    }

    Ok(())
}

/// Re-reads on-chain balances for the accounts we intended to drain and returns
/// `(empty, dust)` — accounts that are truly at zero vs. those that still hold a
/// residual balance and must therefore go through burn+close instead of a plain
/// `close_account`.
///
/// Accounts that have disappeared from the wallet (already closed, e.g. by the
/// swap program itself) are dropped silently: there is nothing left to close.
async fn verify_drained_accounts(
    rpc: &RpcClient,
    owner: &Pubkey,
    intended: &[CloseableTokenAccount],
) -> CliResult<DrainVerification> {
    if intended.is_empty() {
        return Ok(DrainVerification::default());
    }

    let fresh = list_wallet_token_accounts(rpc, owner)
        .await
        .change_context(CliError::Rpc)?;
    let fresh_by_ta: HashMap<Pubkey, CloseableTokenAccount> = fresh
        .into_iter()
        .map(|account| (account.token_account, account))
        .collect();

    let mut verification = DrainVerification {
        empty: Vec::with_capacity(intended.len()),
        dust: Vec::new(),
    };

    for account in intended {
        match fresh_by_ta.get(&account.token_account) {
            Some(current) if current.amount_raw == 0 => verification.empty.push(*current),
            Some(current) => {
                tracing::warn!(
                    "Account {} for mint {} still holds {} raw units after swap; routing to burn+close",
                    current.token_account,
                    current.mint,
                    current.amount_raw
                );
                verification.dust.push(*current);
            }
            None => {
                tracing::info!(
                    "Account {} no longer exists after swap — already closed",
                    account.token_account
                );
            }
        }
    }

    Ok(verification)
}

/// Partitions candidates into those worth swapping vs those to burn and close.
///
/// A single call to [`JupiterClient::prices`] supplies both USD price and
/// decimals for every known mint, so we do not need a separate RPC round-trip
/// to the mint accounts. A candidate is `swappable` when Jupiter returns both
/// fields **and** the computed USD value meets `min_usd_value`; everything else
/// is routed to burn+close with the optional computed value attached for
/// observability in the dry-run log.
async fn classify_candidates(
    candidates: Vec<CloseableTokenAccount>,
    jupiter: &JupiterClient,
    min_usd_value: f64,
) -> CliResult<Classification> {
    if candidates.is_empty() {
        return Ok(Classification::default());
    }

    let mints: Vec<Pubkey> = candidates.iter().map(|account| account.mint).collect();

    let prices = jupiter
        .prices(&mints)
        .await
        .change_context(CliError::Integration)?;

    let mut classification = Classification::default();

    for account in candidates {
        let Some(price_info) = prices.get(&account.mint).copied() else {
            classification.burnable.push(BurnableCandidate {
                account,
                value_usd: None,
            });
            continue;
        };

        let value_usd = (account.amount_raw as f64 / 10f64.powi(price_info.decimals as i32))
            * price_info.usd_price;

        if value_usd >= min_usd_value {
            classification
                .swappable
                .push(SwappableCandidate { account, value_usd });
        } else {
            classification.burnable.push(BurnableCandidate {
                account,
                value_usd: Some(value_usd),
            });
        }
    }

    Ok(classification)
}

/// Selects at most one `CloseableTokenAccount` per swappable mint.
///
/// Filters out native SOL, the target mint, whitelisted mints, and zero-balance
/// accounts. When the wallet holds multiple accounts for the same mint, keeps the
/// one with the highest balance — Jupiter operates on the taker's ATA, so legacy
/// accounts for the same mint would not be drained and are logged as a warning.
fn select_swap_candidates(
    accounts: Vec<CloseableTokenAccount>,
    whitelist: &std::collections::HashSet<Pubkey>,
    target: &Pubkey,
) -> Vec<CloseableTokenAccount> {
    let mut by_mint: HashMap<Pubkey, CloseableTokenAccount> = HashMap::new();

    for account in accounts {
        if account.is_frozen {
            // Frozen accounts cannot be burned, transferred or swapped — leave them
            // alone entirely. `freeze_authority` is off-limits to the token holder.
            tracing::warn!(
                "Skipping frozen account {} (mint {})",
                account.token_account,
                account.mint
            );
            continue;
        }
        if account.amount_raw == 0
            || account.mint == *target
            || is_native_sol_mint(&account.mint)
            || whitelist.contains(&account.mint)
        {
            continue;
        }

        match by_mint.get_mut(&account.mint) {
            Some(existing) => {
                tracing::warn!(
                    "Multiple token accounts hold mint {}: keeping {} ({}), ignoring {} ({})",
                    account.mint,
                    existing.token_account,
                    existing.amount_raw,
                    account.token_account,
                    account.amount_raw,
                );
                if account.amount_raw > existing.amount_raw {
                    *existing = account;
                }
            }
            None => {
                by_mint.insert(account.mint, account);
            }
        }
    }

    by_mint.into_values().collect()
}

/// Runs the swap loop sequentially, returning the accounts successfully drained.
///
/// Each iteration waits [`TX_DELAY_SECS`] seconds before submitting the signed
/// transaction — that pause also spaces out successive `/order` calls to Jupiter
/// since the next iteration cannot start until the previous one confirms.
/// Individual failures are logged and do not abort the run; the failing mint is
/// simply omitted from the returned set (and therefore not closed afterwards).
async fn run_swap_loop(
    jupiter: &JupiterClient,
    rpc: &RpcClient,
    authority: &Keypair,
    owner: Pubkey,
    target: Pubkey,
    slippage_bps: u16,
    candidates: &[CloseableTokenAccount],
) -> Vec<CloseableTokenAccount> {
    let mut swapped = Vec::with_capacity(candidates.len());

    for account in candidates {
        // Throttle before EVERY Jupiter call — including failure paths — so a
        // burst of parse/routing errors cannot hammer the rate limit.
        tokio::time::sleep(Duration::from_secs(PRE_JUPITER_DELAY_SECS)).await;

        let mut request = OrderRequest::new(
            account.mint.to_string(),
            target.to_string(),
            account.amount_raw,
            owner.to_string(),
        );
        request.slippage_bps = Some(slippage_bps);

        let (signed_tx, order) =
            match fetch_signed_swap_transaction(jupiter, &request, authority).await {
                Ok(result) => result,
                Err(err) => {
                    tracing::warn!(
                        "Skipping {}: Jupiter order/sign failed: {:?}",
                        account.mint,
                        err
                    );
                    continue;
                }
            };

        match rpc.send_and_confirm_transaction(&signed_tx).await {
            Ok(signature) => {
                tracing::info!(
                    "Swapped {} → {}: out≈{} sig={}",
                    account.mint,
                    target,
                    order.out_amount,
                    signature
                );
                swapped.push(CloseableTokenAccount {
                    amount_raw: 0,
                    ..*account
                });
                tokio::time::sleep(Duration::from_secs(POST_TX_DELAY_SECS)).await;
            }
            Err(err) => {
                tracing::warn!("RPC submit failed for {}: {:?}", account.mint, err);
            }
        }
    }

    swapped
}
