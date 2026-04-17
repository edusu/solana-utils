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
    spl::{
        close::{CloseableTokenAccount, build_close_instruction},
        utils::is_native_sol_mint,
    },
};

/// Upper bound for `burn + close_account` instruction pairs per transaction.
///
/// Each token account contributes two instructions (burn + close), so the pair
/// count is half the legacy close-only limit to stay well under Solana's 1232-byte
/// transaction limit.
const MAX_BURN_CLOSE_PAIRS_PER_TX: usize = 10;

/// Burns the remaining balance of every token account in the batch and then closes
/// each account in the same transaction, reclaiming the rent to `destination`.
///
/// This is the cleanup path for *unroutable* tokens — mints that cannot be sold via
/// an aggregator (no liquidity, frozen, dust) and therefore have to be discarded
/// to reclaim the rent. WSOL is skipped with a warning because unwrapping is the
/// correct disposal path for native SOL.
///
/// # Parameters
///
/// * `client` - RPC client used to fetch a recent blockhash and submit the batches.
/// * `authority` - Signer that owns the token accounts (also becomes the burn + close authority).
/// * `accounts` - Token accounts whose balances should be burned before closing.
/// * `destination` - Pubkey that receives the reclaimed rent lamports.
///
/// # Returns
///
/// One `Signature` per submitted batch transaction, in submission order.
pub async fn burn_and_close_token_accounts(
    client: &RpcClient,
    authority: &Keypair,
    accounts: Vec<CloseableTokenAccount>,
    destination: Pubkey,
) -> RpcResult<Vec<Signature>> {
    let processable: Vec<CloseableTokenAccount> =
        accounts.into_iter().filter(is_processable).collect();

    if processable.is_empty() {
        return Ok(Vec::new());
    }

    let authority_pubkey = authority.pubkey();

    // Build a single flat instruction stream: [burn_0, close_0, burn_1, close_1, ...].
    // Chunking by `2 * PAIRS_PER_TX` later keeps each TX at a whole number of pairs.
    let instructions: Vec<Instruction> = processable
        .iter()
        .map(|account| build_burn_and_close_instructions(account, &authority_pubkey, &destination))
        .collect::<RpcResult<Vec<[Instruction; 2]>>>()?
        .into_iter()
        .flatten()
        .collect();

    let recent_blockhash = client
        .get_latest_blockhash()
        .await
        .change_context(RpcError::Provider)
        .attach("Failed to fetch recent blockhash")?;

    let batches: Vec<Transaction> = instructions
        .chunks(MAX_BURN_CLOSE_PAIRS_PER_TX * 2)
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
        "Submitting {} burn+close batch transaction(s) in parallel",
        total_batches
    );

    let submissions = batches.iter().enumerate().map(|(index, tx)| async move {
        let signature = client
            .send_and_confirm_transaction(tx)
            .await
            .change_context(RpcError::Provider)
            .attach(format!("Failed to confirm burn+close batch {}", index + 1))?;
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

/// Filters out accounts that should not participate in the burn+close flow.
fn is_processable(account: &CloseableTokenAccount) -> bool {
    if account.is_frozen {
        // A frozen account cannot be Burn'ed (SPL Token error 0x11 AccountFrozen)
        // — there is no owner-side path to reach zero balance, so we leave it as-is.
        tracing::warn!(
            "Skipping burn+close for frozen account {} (mint {}): owner cannot unfreeze",
            account.token_account,
            account.mint
        );
        return false;
    }
    if is_native_sol_mint(&account.mint) {
        tracing::warn!(
            "Skipping burn+close for WSOL account {} (unwrap it instead)",
            account.token_account
        );
        return false;
    }
    true
}

/// Builds the `[burn, close_account]` instruction pair for a single token account.
///
/// The `close_account` half delegates to [`build_close_instruction`] so the
/// SPL Token vs Token-2022 dispatch lives in exactly one place.
fn build_burn_and_close_instructions(
    account: &CloseableTokenAccount,
    authority: &Pubkey,
    destination: &Pubkey,
) -> RpcResult<[Instruction; 2]> {
    let burn_ix = build_burn_instruction(account, authority)?;
    let close_ix = build_close_instruction(account, authority, destination)?;
    Ok([burn_ix, close_ix])
}

/// Builds the `burn` instruction for the token program that owns the account.
fn build_burn_instruction(
    account: &CloseableTokenAccount,
    authority: &Pubkey,
) -> RpcResult<Instruction> {
    if account.program_id == token::ID {
        spl_token::instruction::burn(
            &token::ID,
            &account.token_account,
            &account.mint,
            authority,
            &[],
            account.amount_raw,
        )
        .change_context(RpcError::Internal)
        .attach(format!(
            "Failed to build SPL token burn instruction for {}",
            account.token_account
        ))
    } else if account.program_id == token_2022::ID {
        spl_token_2022::instruction::burn(
            &token_2022::ID,
            &account.token_account,
            &account.mint,
            authority,
            &[],
            account.amount_raw,
        )
        .change_context(RpcError::Internal)
        .attach(format!(
            "Failed to build Token-2022 burn instruction for {}",
            account.token_account
        ))
    } else {
        Err(Report::new(RpcError::Parse).attach(format!(
            "Unsupported token program {} for token account {}",
            account.program_id, account.token_account
        )))
    }
}
