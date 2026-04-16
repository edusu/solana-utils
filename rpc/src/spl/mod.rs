use std::collections::HashMap;

use anchor_lang::prelude::Pubkey;
use error_stack::ResultExt;
use futures::future::try_join_all;
use solana_account::Account;
use solana_keypair::Address;
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::error::{RpcError, RpcResult};

pub mod close;
pub mod tokens;
pub mod utils;

/// Loads many accounts through `getMultipleAccounts` while preserving input order.
async fn fetch_multiple_accounts_chunked(
    client: &RpcClient,
    account_pubkeys: &[Pubkey],
) -> RpcResult<HashMap<Pubkey, Option<Account>>> {
    let fetches = account_pubkeys.chunks(100).map(|accounts_chunk| {
        let account_pubkeys = accounts_chunk.to_vec();
        let account_addresses = account_pubkeys
            .iter()
            .map(|account| Address::from(account.to_bytes()))
            .collect::<Vec<_>>();

        async move {
            let accounts_info = client
                .get_multiple_accounts(&account_addresses)
                .await
                .change_context(RpcError::Provider)?;

            Ok::<_, error_stack::Report<RpcError>>((account_pubkeys, accounts_info))
        }
    });

    let mut accounts_by_pubkey = HashMap::with_capacity(account_pubkeys.len());
    for (account_pubkeys, accounts_info) in try_join_all(fetches).await? {
        for (account_pubkey, account_info) in account_pubkeys.into_iter().zip(accounts_info) {
            accounts_by_pubkey.insert(account_pubkey, account_info);
        }
    }

    Ok(accounts_by_pubkey)
}
