mod close_accounts;
mod config;
mod error;
mod swap_all;

use clap::{Parser, Subcommand};
use tracing_subscriber::EnvFilter;

use crate::error::CliResult;

#[derive(Parser)]
#[command(name = "solana-utils", version, about = "Solana wallet utilities")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Close all token accounts with zero balance, respecting the whitelist.
    CloseAccounts(close_accounts::CloseAccountsArgs),
    /// Swap every token balance (except SOL and whitelisted mints) into a target mint via Jupiter, then close the drained accounts.
    SwapAll(swap_all::SwapAllArgs),
}

#[tokio::main]
async fn main() -> CliResult<()> {
    // `.ok()` — a missing `.env` is expected in non-dev environments, not an error.
    dotenvy::dotenv().ok();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::CloseAccounts(args) => close_accounts::run(args).await,
        Command::SwapAll(args) => swap_all::run(args).await,
    }
}
