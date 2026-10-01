mod authorization;
mod spend;

use borsh::BorshDeserialize;
use std::io::{Error, ErrorKind, Read};

/// Maximum canonical transaction size and per-list cardinality in consensus.
pub const MAX_TRANSACTION_SIZE: usize = 256 * 1024;
pub const MAX_TRANSACTION_ITEMS: usize = 4096;

fn deserialize_bounded_vec<T: BorshDeserialize, R: Read>(
    reader: &mut R,
    maximum: usize,
) -> std::io::Result<Vec<T>> {
    let length = u32::deserialize_reader(reader)? as usize;
    if length > maximum {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "transaction list exceeds limit",
        ));
    }
    let mut items = Vec::new();
    for _ in 0..length {
        items.push(T::deserialize_reader(reader)?);
    }
    Ok(items)
}

pub use crate::error::{IntentError, TransactionEncodingError};
pub use authorization::{
    AccountAuthorization, AccountIntent, AuthorizationCommitment, AuthorizationRole,
    AuthorizedAccountIntent, AuthorizedProgramTransaction, AuthorizedTransaction, IntentId,
    TransactionId, program_transaction_commitment,
};
pub use spend::{Spend, SpendCharges, SpendIntent, SpendIntentCommitment};

pub type Transaction = AuthorizedTransaction;

#[cfg(test)]
mod phase3_bounds_tests {
    use super::*;
    use crate::monetary::coin::CoinShare;
    use crypto::Address;

    #[test]
    fn oversized_spend_list_prefix_is_rejected_before_elements() {
        let mut bytes = vec![0_u8];
        bytes.extend_from_slice(&((MAX_TRANSACTION_ITEMS + 1) as u32).to_le_bytes());
        assert!(Spend::try_from_slice(&bytes).is_err());
    }

    #[test]
    fn in_memory_oversized_spend_is_rejected() {
        let intent = SpendIntent {
            signer: Address::ZERO,
            spend: Spend::Coin {
                inputs: vec![
                    CoinShare::from_bytes([1; crypto::HASH16_SIZE]);
                    MAX_TRANSACTION_ITEMS + 1
                ],
                outputs: vec![],
            },
            charges: SpendCharges::default(),
        };
        assert_eq!(intent.validate(), Err(IntentError::TooManyItems));
    }
}
