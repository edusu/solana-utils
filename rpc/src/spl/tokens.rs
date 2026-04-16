use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use crate::{
    error::{RpcError, RpcResult},
    spl::{
        fetch_multiple_accounts_chunked,
        utils::{hardcoded_token_data_for_mint, unique_pubkeys},
    },
};
use anchor_lang::prelude::Pubkey;
use anchor_spl::{token, token_2022};
use error_stack::{Report, ResultExt};
use solana_account::Account;
use solana_keypair::Address;
use solana_rpc_client::{api::response::RpcKeyedAccount, nonblocking::rpc_client::RpcClient};
use solana_rpc_client_api::request::TokenAccountsFilter;

/// Describes one concrete token account balance owned by a wallet.
// Derives are used so parsed account balances can be compared in tests and cloned into responses.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolanaTokenAccountBalance {
    pub token_account: Pubkey,
    pub amount_raw: u64,
}

/// Aggregates all balances a wallet owns for one mint.
// Derives are used so wallet balance aggregates can be copied between query stages and tests.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SolanaWalletTokenBalance {
    pub token_data: SolanaTokenData,
    pub total_amount_raw: u64,
    pub accounts: Vec<SolanaTokenAccountBalance>,
}

/// Holds the parsed fields we need from one RPC token account response.
struct ParsedRpcTokenAccount {
    token_account: Pubkey,
    mint: Pubkey,
    amount_raw: u64,
}

/// Describes the mint metadata needed to work with Solana token accounts.
// Derives are used so parsed mint metadata can be copied into wallet balance aggregations.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct SolanaTokenData {
    pub program_id: Pubkey,
    pub decimals: u8,
    /// Required account space for initializing new token account
    pub account_len: usize,
}

pub async fn get_token_accounts_by_owner(client: &RpcClient, owner: &Pubkey) -> RpcResult<()> {
    let wallet_address = Address::from(owner.to_bytes());

    // Collect every mint currently owned by the wallet across both token programs.
    let mut mints = HashSet::new();
    for program_id in [token::ID, token_2022::ID] {
        let keyed_accounts = client
            .get_token_accounts_by_owner(
                &wallet_address,
                TokenAccountsFilter::ProgramId(Address::from(program_id.to_bytes())),
            )
            .await
            .expect("live RPC token accounts should load");

        for keyed_account in keyed_accounts {
            let parsed_account =
                parse_rpc_token_account(keyed_account).expect("live token account should parse");
            mints.insert(parsed_account.mint);
        }
    }

    todo!()
}

/// Parses the JSON payload returned by `getTokenAccountsByOwner` into raw balance fields.
fn parse_rpc_token_account(keyed_account: RpcKeyedAccount) -> RpcResult<ParsedRpcTokenAccount> {
    let token_account = Pubkey::from_str(&keyed_account.pubkey)
        .change_context(RpcError::Parse)
        .attach(format!(
            "Failed to parse token account pubkey {}",
            keyed_account.pubkey
        ))?;

    let solana_rpc_client_api::response::UiAccountData::Json(parsed_account) =
        keyed_account.account.data
    else {
        return Err(Report::new(RpcError::Parse).attach(format!(
            "Unexpected token account encoding for {}",
            token_account
        )));
    };

    let account_type = parsed_account
        .parsed
        .get("type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Report::new(RpcError::Parse)
                .attach(format!("Missing token account type for {}", token_account))
        })?;
    if account_type != "account" {
        return Err(Report::new(RpcError::Parse).attach(format!(
            "Unexpected non-token-account payload for {}",
            token_account
        )));
    }

    let info = parsed_account.parsed.get("info").ok_or_else(|| {
        Report::new(RpcError::Parse).attach(format!(
            "Missing token account info payload for {}",
            token_account
        ))
    })?;

    let mint_str = info
        .get("mint")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Report::new(RpcError::Parse).attach(format!(
                "Missing mint in token account info for {}",
                token_account
            ))
        })?;
    let mint = Pubkey::from_str(mint_str)
        .change_context(RpcError::Parse)
        .attach(format!(
            "Failed to parse mint {} for token account {}",
            mint_str, token_account
        ))?;

    let amount_str = info
        .get("tokenAmount")
        .and_then(|token_amount| token_amount.get("amount"))
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            Report::new(RpcError::Parse).attach(format!(
                "Missing raw amount in token account info for {}",
                token_account
            ))
        })?;
    let amount_raw = amount_str
        .parse::<u64>()
        .change_context(RpcError::Parse)
        .attach(format!(
            "Failed to parse raw amount {} for token account {}",
            amount_str, token_account
        ))?;

    Ok(ParsedRpcTokenAccount {
        token_account,
        mint,
        amount_raw,
    })
}

/// Fetches metadata for multiple Solana token mints.
///
/// # Parameters
///
/// * `client` - RPC client used to fetch mint accounts.
/// * `token_mints` - Mint addresses to resolve.
///
/// # Returns
///
/// A map keyed by mint address with the parsed token metadata.
pub async fn get_tokens_data(
    client: &RpcClient,
    token_mints: Vec<Pubkey>,
) -> RpcResult<HashMap<Pubkey, SolanaTokenData>> {
    let mut unique_token_mints = unique_pubkeys(token_mints);
    let mut hardcoded_tokens_by_mint = HashMap::new();
    unique_token_mints.retain(|token_mint| {
        if let Some(token_data) = hardcoded_token_data_for_mint(token_mint) {
            hardcoded_tokens_by_mint.insert(*token_mint, token_data);
            false
        } else {
            true
        }
    });
    if unique_token_mints.is_empty() {
        return Ok(hardcoded_tokens_by_mint);
    }

    let mint_accounts_by_pubkey =
        fetch_multiple_accounts_chunked(client, &unique_token_mints).await?;

    hardcoded_tokens_by_mint.extend(
        collect_token_data_from_accounts(&unique_token_mints, &mint_accounts_by_pubkey).await,
    );

    Ok(hardcoded_tokens_by_mint)
}

/// Collects valid token metadata from preloaded mint accounts while skipping unsupported entries.
///
/// # Parameters
///
/// * `token_mints` - Requested mint addresses in their original deduplicated order.
/// * `mint_accounts_by_pubkey` - Preloaded mint accounts keyed by mint address.
///
/// # Returns
///
/// A map containing only the mints whose metadata could be parsed successfully.
///
/// # Assumptions
///
/// Missing accounts, unsupported token programs, or malformed mint data are treated as
/// token-local failures and therefore omitted from the output instead of failing the whole batch.
async fn collect_token_data_from_accounts(
    token_mints: &[Pubkey],
    mint_accounts_by_pubkey: &HashMap<Pubkey, Option<Account>>,
) -> HashMap<Pubkey, SolanaTokenData> {
    let mut data = HashMap::with_capacity(token_mints.len());

    for token_mint in token_mints {
        let Some(account_info) = mint_accounts_by_pubkey.get(token_mint).cloned().flatten() else {
            tracing::warn!("Skipping token metadata for missing mint {}", token_mint);
            continue;
        };

        match check_token_data(token_mint, account_info).await {
            Ok(checked_data) => {
                data.insert(*token_mint, checked_data);
            }
            Err(err) => {
                tracing::warn!(
                    "Skipping token metadata for mint {} because it could not be parsed: {:?}",
                    token_mint,
                    err
                );
            }
        }
    }

    data
}

/// Validates a mint account and extracts the token metadata required by wallet and pricing flows.
///
/// # Parameters
///
/// * `token_mint` - Mint address being parsed. Used for context-rich errors.
/// * `account_info` - On-chain mint account data returned by RPC.
///
/// # Returns
///
/// The parsed token metadata for the provided mint account.
async fn check_token_data(
    token_mint: &Pubkey,
    account_info: Account,
) -> RpcResult<SolanaTokenData> {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    // #[tokio::test]
    // async fn test_get_token_accounts_by_owner() {}
}
