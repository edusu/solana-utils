use anchor_lang::prelude::Pubkey;
use anchor_spl::token::TokenAccount;
use std::{collections::HashSet, str::FromStr};

use crate::spl::tokens::SolanaTokenData;

/// Number of decimal places used by native SOL balances.
pub const SOLANA_NATIVE_TOKEN_DECIMALS: u8 = 9;
/// Wrapped-SOL mint alias that some Solana integrations still emit for native SOL swaps.
pub const SOLANA_NATIVE_WRAPPED_MINT_ALIAS: &str = "So11111111111111111111111111111111111111111";

pub const SOLANA_WRAPPED_MINT: &str = "So11111111111111111111111111111111111111112";

/// Returns the hardcoded mint address used by integrations that alias native SOL as wrapped SOL.
fn solana_native_wrapped_mint_alias() -> Pubkey {
    Pubkey::from_str(SOLANA_NATIVE_WRAPPED_MINT_ALIAS)
        .expect("Hardcoded native wrapped mint alias must be a valid pubkey")
}

/// Returns the canonical WSOL mint address from chain metadata.
fn solana_wsol_mint() -> Pubkey {
    Pubkey::from_str(SOLANA_WRAPPED_MINT)
        .expect("Solana native token address must be a valid pubkey")
}

/// Deduplicates pubkeys while keeping their first-seen order.
pub fn unique_pubkeys(values: Vec<Pubkey>) -> Vec<Pubkey> {
    let mut unique_values = Vec::with_capacity(values.len());
    let mut seen = HashSet::with_capacity(values.len());
    for value in values {
        if seen.insert(value) {
            unique_values.push(value);
        }
    }

    unique_values
}

/// Returns hardcoded lightweight token metadata for native SOL mint aliases.
///
/// # Parameters
///
/// * `token_mint` - Mint address being requested by the caller.
///
/// # Returns
///
/// Lightweight token metadata when the mint is a known native-SOL alias, otherwise `None`.
pub fn hardcoded_token_data_for_mint(token_mint: &Pubkey) -> Option<SolanaTokenData> {
    if !is_native_sol_mint(token_mint) {
        return None;
    }

    Some(SolanaTokenData {
        program_id: anchor_spl::token::spl_token::ID,
        decimals: SOLANA_NATIVE_TOKEN_DECIMALS,
        account_len: TokenAccount::LEN,
    })
}

/// Checks whether a mint address is a known native SOL representation (WSOL or the SOL alias).
pub fn is_native_sol_mint(mint: &Pubkey) -> bool {
    *mint == solana_wsol_mint() || *mint == solana_native_wrapped_mint_alias()
}
