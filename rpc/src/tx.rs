use error_stack::ResultExt;
use solana_keypair::Keypair;
use solana_transaction::versioned::VersionedTransaction;

use crate::error::{RpcError, RpcResult};

/// Signs the message of a pre-assembled `VersionedTransaction` with the taker keypair.
///
/// # Parameters
///
/// * `transaction` - A decoded transaction whose header matches the single-signer shape.
/// * `signer` - Keypair authorized by the transaction's message (the `taker` wallet).
///
/// # Returns
///
/// A [`VersionedTransaction`] ready to be submitted via `send_and_confirm_transaction`
/// or any other sender accepting `SerializableTransaction`.
pub fn sign_versioned_transaction(
    transaction: VersionedTransaction,
    signer: &Keypair,
) -> RpcResult<VersionedTransaction> {
    VersionedTransaction::try_new(transaction.message, &[signer])
        .change_context(RpcError::Internal)
        .attach("Failed to sign versioned transaction")
}
