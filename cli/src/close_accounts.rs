use std::{path::PathBuf, str::FromStr};

use anchor_lang::prelude::Pubkey;
use clap::Args;
use error_stack::ResultExt;
use rpc::{
    rpc::create_rpc_client,
    spl::{
        close::{CloseableTokenAccount, close_token_accounts},
        tokens::list_wallet_token_accounts,
        utils::is_native_sol_mint,
    },
};
use solana_keypair::{Keypair, Signer};

use crate::{
    config::Config,
    error::{CliError, CliResult},
};

/// Arguments for the `close-accounts` subcommand.
// Derives are used so clap can parse CLI flags and debug-print them on verbose logging.
#[derive(Args, Debug)]
pub struct CloseAccountsArgs {
    /// Path to the TOML config file that holds the mint whitelist.
    #[arg(long, default_value = "config.toml")]
    config: PathBuf,

    /// RPC endpoint to use.
    #[arg(long, default_value = "https://api.mainnet-beta.solana.com")]
    rpc_url: String,

    /// Destination pubkey for the reclaimed rent lamports. Defaults to the signer.
    #[arg(long)]
    destination: Option<String>,

    /// List the accounts that would be closed without submitting any transaction.
    #[arg(long)]
    dry_run: bool,
}

/// Entrypoint of the `close-accounts` subcommand.
///
/// Discovers every token account owned by the wallet behind `SOLANA_PRIVATE_KEY`,
/// filters out whitelist-protected mints, and closes the remaining zero-balance
/// accounts in batched transactions.
pub async fn run(args: CloseAccountsArgs) -> CliResult<()> {
    let config = Config::load(&args.config)?;
    let whitelist = config.close_accounts.whitelist_pubkeys()?;
    tracing::info!(
        "Loaded {} whitelisted mint(s) from {}",
        whitelist.len(),
        args.config.display()
    );

    let private_key = std::env::var("SOLANA_PRIVATE_KEY")
        .change_context(CliError::Input)
        .attach("SOLANA_PRIVATE_KEY env var is required")?;
    let authority = Keypair::from_base58_string(&private_key);
    let owner = authority.pubkey();
    tracing::info!("Authority: {}", owner);

    let client = create_rpc_client(Some(args.rpc_url));

    // Listing doesn't require resolving mint metadata, so accounts for dead/burned mints —
    // the main candidates for cleanup — are preserved.
    let all_accounts = list_wallet_token_accounts(&client, &owner)
        .await
        .change_context(CliError::Rpc)?;
    tracing::info!("Discovered {} token account(s)", all_accounts.len());

    let candidates: Vec<CloseableTokenAccount> = all_accounts
        .into_iter()
        .filter(|account| !whitelist.contains(&account.mint))
        .collect();
    tracing::info!(
        "{} account(s) remain after applying whitelist",
        candidates.len()
    );

    if args.dry_run {
        // Mirror the filter applied inside `close_token_accounts` so the preview matches
        // exactly what would be submitted on a real run.
        let closeable: Vec<&CloseableTokenAccount> = candidates
            .iter()
            .filter(|account| account.amount_raw == 0 && !is_native_sol_mint(&account.mint))
            .collect();
        tracing::info!("[dry-run] {} account(s) would be closed:", closeable.len());
        for account in &closeable {
            tracing::info!(
                "  token_account={} mint={} program={}",
                account.token_account,
                account.mint,
                account.program_id
            );
        }
        return Ok(());
    }

    let destination = match args.destination.as_deref() {
        Some(raw) => Pubkey::from_str(raw)
            .change_context(CliError::Input)
            .attach(format!("Invalid --destination pubkey {}", raw))?,
        None => owner,
    };

    let signatures = close_token_accounts(&client, &authority, candidates, destination)
        .await
        .change_context(CliError::Rpc)?;

    tracing::info!("Submitted {} batch transaction(s)", signatures.len());
    for (index, signature) in signatures.iter().enumerate() {
        tracing::info!("  batch {}: {}", index + 1, signature);
    }

    Ok(())
}
