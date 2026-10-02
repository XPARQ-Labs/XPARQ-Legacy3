mod authorization;
mod coin_transition;

pub const MAX_TRANSACTION_SIZE: usize = 256 * 1024;
pub const MAX_TRANSACTION_ITEMS: usize = 4096;

pub use crate::error::{IntentError, TransactionEncodingError};
pub use authorization::{
    AccountAuthorization, AuthorizationCommitment, AuthorizationRole, AuthorizedProgramTransaction,
    AuthorizedTransaction, IntentId, TransactionId, program_transaction_commitment,
};
pub use coin_transition::{CoinCharges, CoinTransition, CoinTransitionCommitment};

pub type Transaction = AuthorizedTransaction;

#[cfg(test)]
mod phase3_bounds_tests {
    use super::*;
    use crate::monetary::coin::CoinShare;
    use borsh::BorshDeserialize;
    use crypto::Address;

    #[test]
    fn oversized_coin_list_prefix_is_rejected_before_elements() {
        let mut bytes = vec![0_u8; crypto::ADDRESS_SIZE];
        bytes.extend_from_slice(&((MAX_TRANSACTION_ITEMS + 1) as u32).to_le_bytes());
        assert!(CoinTransition::try_from_slice(&bytes).is_err());
    }

    #[test]
    fn in_memory_oversized_transition_is_rejected() {
        let intent = CoinTransition {
            signer: Address::ZERO,
            inputs: vec![
                CoinShare::from_bytes([1; crypto::HASH16_SIZE]);
                MAX_TRANSACTION_ITEMS + 1
            ],
            outputs: vec![],
            charges: CoinCharges::default(),
        };
        assert_eq!(intent.validate(), Err(IntentError::TooManyItems));
    }
}

use borsh::BorshDeserialize;
use std::io::{Error, ErrorKind, Read};

pub(crate) fn deserialize_bounded_vec<T: BorshDeserialize, R: Read>(
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
