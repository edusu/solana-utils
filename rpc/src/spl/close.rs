use anchor_client::{Instruction, Signer, Transaction};
use anchor_lang::prelude::Pubkey;
use anchor_spl::{
    token::{self, spl_token},
    token_2022::{self, spl_token_2022},
};
use error_stack::{Report, ResultExt};
use futures::future::try_join_all;
use solana_keypair::{Keypair, Signature};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;

use crate::{
    error::{RpcError, RpcResult},
    spl::utils::is_native_sol_mint,
};

/// Token account eligible for closing.
///
/// Carrying `program_id`, `mint` and `amount_raw` lets the closer build the right
/// `close_account` instruction and pre-filter non-closeable accounts without any
/// extra RPC calls.
// Derives are used so caller code can build these from existing query results with plain copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CloseableTokenAccount {
    pub token_account: Pubkey,
    pub program_id: Pubkey,
    pub mint: Pubkey,
    pub amount_raw: u64,
    /// `true` when the token account has been frozen by the mint's freeze authority.
    ///
    /// A frozen account rejects Burn/Transfer, so cleanup flows that depend on
    /// reaching a zero balance (our burn+close path) must skip it.
    pub is_frozen: bool,
}

/// Upper bound for `close_account` instructions per transaction.
const MAX_CLOSE_IXS_PER_TX: usize = 20;

/// Closes a batch of token accounts, reclaiming their rent-exempt lamports into `destination`.
///
/// # Parameters
///
/// * `client` - RPC client used to fetch a recent blockhash and submit the close transactions.
/// * `authority` - Signer that is the `owner` (or `close_authority`) of every account being closed.
/// * `accounts` - Token accounts to close along with the metadata required to build and filter them.
/// * `destination` - Pubkey that receives the reclaimed lamports (typically `authority.pubkey()`).
///
/// # Returns
///
/// One `Signature` per submitted batch transaction, in submission order.
///
/// # Assumptions
///
/// Accounts with a non-zero `amount_raw` and WSOL accounts are skipped with a warn log.
/// The on-chain `close_account` program already rejects non-zero balances, so filtering
/// them here avoids wasting whole-batch transactions on a single bad entry.
pub async fn close_token_accounts(
    client: &RpcClient,
    authority: &Keypair,
    accounts: Vec<CloseableTokenAccount>,
    destination: Pubkey,
) -> RpcResult<Vec<Signature>> {
    let closeable: Vec<CloseableTokenAccount> = accounts
        .into_iter()
        .filter(|account| is_closeable(account))
        .collect();

    if closeable.is_empty() {
        return Ok(Vec::new());
    }

    let authority_pubkey = authority.pubkey();
    let instructions = closeable
        .iter()
        .map(|account| build_close_instruction(account, &authority_pubkey, &destination))
        .collect::<RpcResult<Vec<_>>>()?;

    // One blockhash shared across all batches. A blockhash is valid for ~60 seconds,
    // more than enough to sign + submit 10-20 parallel transactions.
    let recent_blockhash = client
        .get_latest_blockhash()
        .await
        .change_context(RpcError::Provider)
        .attach("Failed to fetch recent blockhash")?;

    let batches: Vec<Transaction> = instructions
        .chunks(MAX_CLOSE_IXS_PER_TX)
        .map(|batch| {
            Transaction::new_signed_with_payer(
                batch,
                Some(&authority_pubkey),
                &[authority],
                recent_blockhash,
            )
        })
        .collect();

    let total_batches = batches.len();
    tracing::info!(
        "Submitting {} close-accounts batch transaction(s) in parallel",
        total_batches
    );

    // Submit + await confirmation of all batches concurrently. `send_and_confirm_transaction`
    // polls for confirmation internally; running them in parallel avoids stacking those polls
    // end-to-end and cuts wall-clock time from ~N*confirmation_time to ~1*confirmation_time.
    let submissions = batches.iter().enumerate().map(|(index, tx)| async move {
        let signature = client
            .send_and_confirm_transaction(tx)
            .await
            .change_context(RpcError::Provider)
            .attach(format!(
                "Failed to confirm close-accounts batch {}",
                index + 1
            ))?;
        tracing::info!(
            "Batch {}/{} confirmed: {}",
            index + 1,
            total_batches,
            signature
        );
        Ok::<Signature, error_stack::Report<RpcError>>(signature)
    });

    try_join_all(submissions).await
}

/// Decides whether a token account can be closed by the program without failing.
fn is_closeable(account: &CloseableTokenAccount) -> bool {
    if account.amount_raw != 0 {
        tracing::warn!(
            "Skipping close for token account {}: non-zero balance {}",
            account.token_account,
            account.amount_raw
        );
        return false;
    }

    // Closing a WSOL account unwraps it into native SOL; callers who want that behavior
    // should invoke it explicitly through a dedicated path rather than as a side effect.
    if is_native_sol_mint(&account.mint) {
        tracing::warn!(
            "Skipping close for WSOL token account {} (use the unwrap flow instead)",
            account.token_account
        );
        return false;
    }

    true
}

/// Builds the `close_account` instruction for the token program that owns the account.
pub(crate) fn build_close_instruction(
    account: &CloseableTokenAccount,
    authority: &Pubkey,
    destination: &Pubkey,
) -> RpcResult<Instruction> {
    if account.program_id == token::ID {
        spl_token::instruction::close_account(
            &token::ID,
            &account.token_account,
            destination,
            authority,
            &[],
        )
        .change_context(RpcError::Internal)
        .attach(format!(
            "Failed to build SPL token close_account instruction for {}",
            account.token_account
        ))
    } else if account.program_id == token_2022::ID {
        spl_token_2022::instruction::close_account(
            &token_2022::ID,
            &account.token_account,
            destination,
            authority,
            &[],
        )
        .change_context(RpcError::Internal)
        .attach(format!(
            "Failed to build Token-2022 close_account instruction for {}",
            account.token_account
        ))
    } else {
        Err(Report::new(RpcError::Parse).attach(format!(
            "Unsupported token program {} for token account {}",
            account.program_id, account.token_account
        )))
    }
}
