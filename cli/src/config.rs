use std::{collections::HashSet, path::Path, str::FromStr};

use anchor_lang::prelude::Pubkey;
use error_stack::ResultExt;
use serde::Deserialize;

use crate::error::{CliError, CliResult};

/// Root TOML document. Every section has its own `#[serde(default)]` so an empty or
/// missing file still deserialises into a valid (empty) configuration.
// Derives are used so the config can be deserialised from TOML and introspected in tests.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct Config {
    #[serde(default)]
    pub close_accounts: CloseAccountsConfig,
    #[serde(default)]
    pub swap_all: SwapAllConfig,
}

/// Configuration scoped to the `close-accounts` subcommand.
// Derives are used so the section can be deserialised from TOML independently.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct CloseAccountsConfig {
    /// Mint addresses whose token accounts must be skipped by the closer.
    #[serde(default)]
    pub whitelist_mints: Vec<String>,
}

/// Configuration scoped to the `swap-all` subcommand.
// Derives are used so the section can be deserialised from TOML independently.
#[derive(Deserialize, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub struct SwapAllConfig {
    /// Mint addresses that must never be swapped out of the wallet.
    #[serde(default)]
    pub whitelist_mints: Vec<String>,
}

impl Config {
    /// Loads and parses the TOML configuration at `path`.
    ///
    /// An empty file (or one containing only whitespace) yields `Config::default()` so
    /// users can run the CLI with an empty `config.toml` and opt in to options gradually.
    pub fn load(path: &Path) -> CliResult<Self> {
        let content = std::fs::read_to_string(path)
            .change_context(CliError::Config)
            .attach(format!("Failed to read config at {}", path.display()))?;

        if content.trim().is_empty() {
            return Ok(Self::default());
        }

        toml::from_str(&content)
            .change_context(CliError::Config)
            .attach(format!("Failed to parse TOML at {}", path.display()))
    }
}

impl CloseAccountsConfig {
    /// Parses the whitelist entries into `Pubkey`s, deduplicating along the way.
    pub fn whitelist_pubkeys(&self) -> CliResult<HashSet<Pubkey>> {
        self.whitelist_mints
            .iter()
            .map(|mint| {
                Pubkey::from_str(mint)
                    .change_context(CliError::Config)
                    .attach(format!("Invalid whitelist mint {}", mint))
            })
            .collect()
    }
}

impl SwapAllConfig {
    /// Parses the whitelist entries into `Pubkey`s, deduplicating along the way.
    pub fn whitelist_pubkeys(&self) -> CliResult<HashSet<Pubkey>> {
        self.whitelist_mints
            .iter()
            .map(|mint| {
                Pubkey::from_str(mint)
                    .change_context(CliError::Config)
                    .attach(format!("Invalid whitelist mint {}", mint))
            })
            .collect()
    }
}
