use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
};

use crate::{
    error::{RpcError, RpcResult},
    spl::{
        close::CloseableTokenAccount,
        fetch_multiple_accounts_chunked,
        utils::{hardcoded_token_data_for_mint, unique_pubkeys},
    },
};
use anchor_lang::prelude::Pubkey;
use anchor_lang::solana_program::program_pack::Pack;
use anchor_spl::{
    token::{self, spl_token},
    token_2022::{
        self,
        spl_token_2022::{
            extension::{BaseStateWithExtensions, ExtensionType, StateWithExtensionsOwned},
            state::{Account as Token2022Account, Mint as Token2022Mint},
        },
    },
};
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
    is_frozen: bool,
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

/// Fetches every token account owned by a wallet across both SPL token programs and
/// aggregates their balances per mint.
///
/// # Parameters
///
/// * `client` - RPC client used to query token accounts and mint metadata.
/// * `owner` - Wallet whose token accounts should be listed and aggregated.
///
/// # Returns
///
/// A map keyed by mint with the resolved mint metadata and the wallet's summed balance.
pub async fn get_token_accounts_by_owner(
    client: &RpcClient,
    owner: &Pubkey,
) -> RpcResult<HashMap<Pubkey, SolanaWalletTokenBalance>> {
    let wallet_address = Address::from(owner.to_bytes());

    // Each `get_token_accounts_by_owner` response already carries the raw amount, so we
    // parse once up front and keep the parsed accounts to avoid re-querying the same data.
    let mut parsed_accounts = Vec::new();
    let mut mints = HashSet::new();
    for program_id in [token::ID, token_2022::ID] {
        let keyed_accounts = client
            .get_token_accounts_by_owner(
                &wallet_address,
                TokenAccountsFilter::ProgramId(Address::from(program_id.to_bytes())),
            )
            .await
            .change_context(RpcError::Provider)
            .attach(format!(
                "Failed to list token accounts for wallet {} under program {}",
                owner, program_id
            ))?;

        for keyed_account in keyed_accounts {
            let parsed = parse_rpc_token_account(keyed_account)?;
            mints.insert(parsed.mint);
            parsed_accounts.push(parsed);
        }
    }

    let tokens_by_mint = get_tokens_data(client, mints.into_iter().collect()).await?;

    let mut balances: HashMap<Pubkey, SolanaWalletTokenBalance> = tokens_by_mint
        .into_iter()
        .map(|(mint, token_data)| {
            (
                mint,
                SolanaWalletTokenBalance {
                    token_data,
                    total_amount_raw: 0,
                    accounts: Vec::new(),
                },
            )
        })
        .collect();

    for parsed in parsed_accounts {
        // Mints whose metadata could not be resolved are silently dropped; the metadata
        // loader already logs each skip with context at warn level.
        let Some(balance) = balances.get_mut(&parsed.mint) else {
            continue;
        };

        balance.total_amount_raw = balance
            .total_amount_raw
            .checked_add(parsed.amount_raw)
            .ok_or_else(|| {
                Report::new(RpcError::Internal).attach(format!(
                    "Overflow while summing wallet balances for mint {}",
                    parsed.mint
                ))
            })?;
        balance.accounts.push(SolanaTokenAccountBalance {
            token_account: parsed.token_account,
            amount_raw: parsed.amount_raw,
        });
    }

    Ok(balances)
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

    // `state` is emitted by jsonParsed as a lowercase string. Anything other than
    // `"frozen"` is treated as usable; a missing field (some old encodings) also
    // implies "not frozen".
    let is_frozen = info
        .get("state")
        .and_then(serde_json::Value::as_str)
        .map(|state| state.eq_ignore_ascii_case("frozen"))
        .unwrap_or(false);

    Ok(ParsedRpcTokenAccount {
        token_account,
        mint,
        amount_raw,
        is_frozen,
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

    hardcoded_tokens_by_mint.extend(collect_token_data_from_accounts(
        &unique_token_mints,
        &mint_accounts_by_pubkey,
    ));

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
fn collect_token_data_from_accounts(
    token_mints: &[Pubkey],
    mint_accounts_by_pubkey: &HashMap<Pubkey, Option<Account>>,
) -> HashMap<Pubkey, SolanaTokenData> {
    let mut data = HashMap::with_capacity(token_mints.len());

    for token_mint in token_mints {
        let Some(account_info) = mint_accounts_by_pubkey.get(token_mint).cloned().flatten() else {
            tracing::warn!("Skipping token metadata for missing mint {}", token_mint);
            continue;
        };

        match check_token_data(token_mint, account_info) {
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
fn check_token_data(token_mint: &Pubkey, account_info: Account) -> RpcResult<SolanaTokenData> {
    if account_info.owner == token::ID {
        let mint = spl_token::state::Mint::unpack(&account_info.data)
            .change_context(RpcError::Parse)
            .attach(format!("Failed to unpack SPL token mint for {}", token_mint))?;

        Ok(SolanaTokenData {
            program_id: token::ID,
            decimals: mint.decimals,
            account_len: token::TokenAccount::LEN,
        })
    } else if account_info.owner == token_2022::ID {
        // Token-2022 token account size depends on the mint's TLV extensions, so we must
        // walk the mint's extension list and derive the matching account-side extensions.
        let mint_state = StateWithExtensionsOwned::<Token2022Mint>::unpack(account_info.data)
            .map_err(|err| {
                Report::new(RpcError::Parse).attach(format!(
                    "Failed to unpack Token-2022 mint for {}: {}",
                    token_mint, err
                ))
            })?;

        let mint_extension_types = mint_state.get_extension_types().map_err(|err| {
            Report::new(RpcError::Parse).attach(format!(
                "Failed to read Token-2022 mint extensions for {}: {}",
                token_mint, err
            ))
        })?;

        let account_extension_types =
            ExtensionType::get_required_init_account_extensions(&mint_extension_types);

        let account_len =
            ExtensionType::try_calculate_account_len::<Token2022Account>(&account_extension_types)
                .map_err(|err| {
                    Report::new(RpcError::Parse).attach(format!(
                        "Failed to calculate Token-2022 account length for mint {}: {}",
                        token_mint, err
                    ))
                })?;

        Ok(SolanaTokenData {
            program_id: token_2022::ID,
            decimals: mint_state.base.decimals,
            account_len,
        })
    } else {
        Err(Report::new(RpcError::Parse).attach(format!(
            "Unsupported token program {} owns mint {}",
            account_info.owner, token_mint
        )))
    }
}

/// Lists every token account owned by `wallet`, annotated with the program that owns each.
///
/// # Parameters
///
/// * `client` - RPC client used to list wallet-owned token accounts.
/// * `wallet` - Wallet whose token accounts should be returned.
///
/// # Returns
///
/// One record per token account across both SPL token programs. `program_id` is taken
/// from the per-program query, so no additional lookups are needed to dispatch the
/// matching close instruction later.
///
/// # Assumptions
///
/// Unlike [`get_token_accounts_by_owner`], this function does NOT resolve mint metadata —
/// accounts whose mint has been closed, burned, or is otherwise unloadable are still
/// included. That makes it the right entry point for closing stale accounts, where
/// precisely those orphaned records are the ones worth reclaiming rent from.
pub async fn list_wallet_token_accounts(
    client: &RpcClient,
    wallet: &Pubkey,
) -> RpcResult<Vec<CloseableTokenAccount>> {
    let wallet_address = Address::from(wallet.to_bytes());
    let mut accounts = Vec::new();

    for program_id in [token::ID, token_2022::ID] {
        let keyed_accounts = client
            .get_token_accounts_by_owner(
                &wallet_address,
                TokenAccountsFilter::ProgramId(Address::from(program_id.to_bytes())),
            )
            .await
            .change_context(RpcError::Provider)
            .attach(format!(
                "Failed to list token accounts for wallet {} under program {}",
                wallet, program_id
            ))?;

        for keyed_account in keyed_accounts {
            let parsed = parse_rpc_token_account(keyed_account)?;
            accounts.push(CloseableTokenAccount {
                token_account: parsed.token_account,
                program_id,
                mint: parsed.mint,
                amount_raw: parsed.amount_raw,
                is_frozen: parsed.is_frozen,
            });
        }
    }

    Ok(accounts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rpc::create_rpc_client;
    use anchor_lang::solana_program::{program_option::COption, program_pack::Pack};
    use anchor_spl::token::spl_token::state::Mint as SplMint;
    use serde_json::{Value, json};

    const TOKEN_ACCOUNT_PUBKEY: &str = "5Q544fKrFoe6tsEbD7S8EmxGTJYAKtTVhAW5Q5pge4j1";
    const USDC_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";
    const OWNER_PUBKEY: &str = "9WzDXwBbmkg8ZTbNMqUxvQRAyrZzDsGYdLVL9zYtAWWM";

    /// Builds a `jsonParsed`-style `RpcKeyedAccount` fixture from an `info` object.
    ///
    /// The outer envelope (`pubkey`, `account.lamports`, encoding, etc.) is fixed —
    /// individual tests only override the inner `info` payload, which is where every
    /// field `parse_rpc_token_account` looks at lives.
    fn token_account_fixture(pubkey: &str, info: Value) -> RpcKeyedAccount {
        let body = json!({
            "pubkey": pubkey,
            "account": {
                "lamports": 2_039_280,
                "data": {
                    "program": "spl-token",
                    "parsed": { "type": "account", "info": info },
                    "space": 165,
                },
                "owner": token::ID.to_string(),
                "executable": false,
                "rentEpoch": 0,
                "space": 165,
            }
        });
        serde_json::from_value(body).unwrap()
    }

    /// Default `info` payload matching what Solana's RPC emits for an initialized SPL token account.
    fn default_info(mint: &str) -> Value {
        json!({
            "isNative": false,
            "mint": mint,
            "owner": OWNER_PUBKEY,
            "state": "initialized",
            "tokenAmount": {
                "amount": "1000",
                "decimals": 6,
                "uiAmount": 0.001_f64,
                "uiAmountString": "0.001",
            },
        })
    }

    #[test]
    fn parse_rpc_token_account_accepts_valid_initialized_account() {
        let fixture = token_account_fixture(TOKEN_ACCOUNT_PUBKEY, default_info(USDC_MINT));
        let parsed = parse_rpc_token_account(fixture).unwrap();

        assert_eq!(
            parsed.token_account,
            TOKEN_ACCOUNT_PUBKEY.parse::<Pubkey>().unwrap()
        );
        assert_eq!(parsed.mint, USDC_MINT.parse::<Pubkey>().unwrap());
        assert_eq!(parsed.amount_raw, 1000);
        assert!(!parsed.is_frozen);
    }

    #[test]
    fn parse_rpc_token_account_flags_frozen_state() {
        let mut info = default_info(USDC_MINT);
        info["state"] = json!("frozen");
        let parsed =
            parse_rpc_token_account(token_account_fixture(TOKEN_ACCOUNT_PUBKEY, info)).unwrap();
        assert!(parsed.is_frozen);
    }

    // Guards against RPC implementations that upper-case the state field. The parser
    // is case-insensitive on purpose.
    #[test]
    fn parse_rpc_token_account_flags_frozen_is_case_insensitive() {
        let mut info = default_info(USDC_MINT);
        info["state"] = json!("FROZEN");
        let parsed =
            parse_rpc_token_account(token_account_fixture(TOKEN_ACCOUNT_PUBKEY, info)).unwrap();
        assert!(parsed.is_frozen);
    }

    // Older `jsonParsed` encodings omit the `state` field; treat absence as "not frozen"
    // rather than erroring out.
    #[test]
    fn parse_rpc_token_account_treats_missing_state_as_not_frozen() {
        let mut info = default_info(USDC_MINT);
        info.as_object_mut().unwrap().remove("state");
        let parsed =
            parse_rpc_token_account(token_account_fixture(TOKEN_ACCOUNT_PUBKEY, info)).unwrap();
        assert!(!parsed.is_frozen);
    }

    /// Asserts that parsing a fixture built from `info` produces an `RpcError::Parse`.
    fn assert_parse_error(info: Value) {
        let Err(err) = parse_rpc_token_account(token_account_fixture(TOKEN_ACCOUNT_PUBKEY, info))
        else {
            panic!("expected Parse error");
        };
        assert!(matches!(err.current_context(), RpcError::Parse));
    }

    /// Asserts that parsing `fixture` (used when the test needs a non-default envelope)
    /// produces an `RpcError::Parse`.
    fn assert_parse_error_fixture(fixture: RpcKeyedAccount) {
        let Err(err) = parse_rpc_token_account(fixture) else {
            panic!("expected Parse error");
        };
        assert!(matches!(err.current_context(), RpcError::Parse));
    }

    #[test]
    fn parse_rpc_token_account_rejects_non_json_encoding() {
        // Base64 encoding instead of jsonParsed: the parser has no structured
        // fields to extract, so it must fail rather than silently returning zeros.
        let body = json!({
            "pubkey": TOKEN_ACCOUNT_PUBKEY,
            "account": {
                "lamports": 2_039_280,
                "data": ["ZGVhZGJlZWY=", "base64"],
                "owner": token::ID.to_string(),
                "executable": false,
                "rentEpoch": 0,
                "space": 165,
            }
        });
        assert_parse_error_fixture(serde_json::from_value(body).unwrap());
    }

    #[test]
    fn parse_rpc_token_account_rejects_non_account_type() {
        let body = json!({
            "pubkey": TOKEN_ACCOUNT_PUBKEY,
            "account": {
                "lamports": 2_039_280,
                "data": {
                    "program": "spl-token",
                    "parsed": { "type": "mint", "info": {} },
                    "space": 82,
                },
                "owner": token::ID.to_string(),
                "executable": false,
                "rentEpoch": 0,
                "space": 82,
            }
        });
        assert_parse_error_fixture(serde_json::from_value(body).unwrap());
    }

    #[test]
    fn parse_rpc_token_account_rejects_missing_mint_field() {
        let mut info = default_info(USDC_MINT);
        info.as_object_mut().unwrap().remove("mint");
        assert_parse_error(info);
    }

    #[test]
    fn parse_rpc_token_account_rejects_missing_amount_field() {
        let mut info = default_info(USDC_MINT);
        info["tokenAmount"]
            .as_object_mut()
            .unwrap()
            .remove("amount");
        assert_parse_error(info);
    }

    #[test]
    fn parse_rpc_token_account_rejects_non_numeric_amount() {
        let mut info = default_info(USDC_MINT);
        info["tokenAmount"]["amount"] = json!("not-a-number");
        assert_parse_error(info);
    }

    /// Builds a valid legacy SPL Token Mint account packed into the 82-byte layout.
    fn legacy_mint_account(decimals: u8) -> Account {
        let mint = SplMint {
            mint_authority: COption::None,
            supply: 1,
            decimals,
            is_initialized: true,
            freeze_authority: COption::None,
        };
        let mut data = vec![0u8; SplMint::LEN];
        SplMint::pack(mint, &mut data).expect("packing a valid mint must succeed");
        Account {
            lamports: 1_000_000,
            data,
            owner: token::ID,
            executable: false,
            rent_epoch: 0,
        }
    }

    #[test]
    fn collect_token_data_returns_metadata_for_valid_legacy_mint() {
        let mint_pk: Pubkey = USDC_MINT.parse().unwrap();
        let mut accounts = HashMap::new();
        accounts.insert(mint_pk, Some(legacy_mint_account(6)));

        let out = collect_token_data_from_accounts(&[mint_pk], &accounts);

        assert_eq!(out.len(), 1);
        let data = out.get(&mint_pk).expect("legacy mint must be included");
        assert_eq!(data.program_id, token::ID);
        assert_eq!(data.decimals, 6);
        assert_eq!(data.account_len, token::TokenAccount::LEN);
    }

    // Closed / stale mints come back as `None` from `getMultipleAccounts`. We must
    // skip them rather than fail the whole batch.
    #[test]
    fn collect_token_data_skips_missing_account() {
        let mint_pk: Pubkey = USDC_MINT.parse().unwrap();
        let mut accounts = HashMap::new();
        accounts.insert(mint_pk, None);

        let out = collect_token_data_from_accounts(&[mint_pk], &accounts);
        assert!(out.is_empty());
    }

    // A mint owned by a program that is not SPL-Token / Token-2022 has no readable
    // decimals; skip rather than guess.
    #[test]
    fn collect_token_data_skips_unsupported_owner() {
        let mint_pk: Pubkey = USDC_MINT.parse().unwrap();
        let mut bogus = legacy_mint_account(6);
        bogus.owner = Pubkey::new_unique();
        let mut accounts = HashMap::new();
        accounts.insert(mint_pk, Some(bogus));

        let out = collect_token_data_from_accounts(&[mint_pk], &accounts);
        assert!(out.is_empty());
    }

    // SPL-Token owner but garbage bytes → `SplMint::unpack` fails → skipped.
    #[test]
    fn collect_token_data_skips_corrupted_legacy_mint_bytes() {
        let mint_pk: Pubkey = USDC_MINT.parse().unwrap();
        let corrupted = Account {
            lamports: 1,
            data: vec![0xFF; 10],
            owner: token::ID,
            executable: false,
            rent_epoch: 0,
        };
        let mut accounts = HashMap::new();
        accounts.insert(mint_pk, Some(corrupted));

        let out = collect_token_data_from_accounts(&[mint_pk], &accounts);
        assert!(out.is_empty());
    }

    // Live integration test hits the public mainnet RPC; run with:
    //     cargo test -p rpc -- --ignored --nocapture

    #[tokio::test]
    #[ignore = "hits public mainnet RPC"]
    async fn live_get_token_accounts_by_owner() {
        let wallet = Pubkey::from_str("7kDXEH3xPS5TvScR1czWvSCJMaeHHB9693mWTrdTRQVB").unwrap();
        let client = create_rpc_client(None);

        let balances = get_token_accounts_by_owner(&client, &wallet)
            .await
            .expect("mainnet RPC should return token balances");

        for (mint, balance) in &balances {
            let pid = balance.token_data.program_id;
            assert!(
                pid == token::ID || pid == token_2022::ID,
                "unexpected program_id {} for mint {}",
                pid,
                mint
            );

            let per_account_sum: u64 = balance.accounts.iter().map(|a| a.amount_raw).sum();
            assert_eq!(
                balance.total_amount_raw, per_account_sum,
                "total_amount_raw must equal the sum of per-account balances for mint {}",
                mint
            );
        }
    }
}
