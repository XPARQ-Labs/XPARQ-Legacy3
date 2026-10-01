use std::{
    collections::BTreeSet,
    io::{Error, ErrorKind, Read},
};

use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{Address, HASH_SIZE, HashDomain, canonical_bytes, domain};

use crate::common::ChainContext;

use crate::{
    monetary::coin::{CoinOutput, CoinShare, Zeno},
    transaction::{IntentError, MAX_TRANSACTION_ITEMS, deserialize_bounded_vec},
};

/// Canonical semantic commitment for a spend intent.
///
/// Account signatures should use `AccountIntent::authorization_commitment`
/// from the authorization layer. That commitment additionally binds the
/// authorization role.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct SpendIntentCommitment([u8; HASH_SIZE]);

impl SpendIntentCommitment {
    pub const fn from_bytes(bytes: [u8; HASH_SIZE]) -> Self {
        Self(bytes)
    }

    pub const fn as_bytes(&self) -> &[u8; HASH_SIZE] {
        &self.0
    }

    pub const fn into_bytes(self) -> [u8; HASH_SIZE] {
        self.0
    }
}

/// An account-authorized transfer.
///
/// Native XPQ spends are separate from extension Program calls.
#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub enum Spend {
    Coin {
        inputs: Vec<CoinShare>,
        outputs: Vec<CoinOutput>,
    },
}

impl BorshDeserialize for Spend {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => Ok(Self::Coin {
                inputs: deserialize_bounded_vec(reader, MAX_TRANSACTION_ITEMS)?,
                outputs: deserialize_bounded_vec(reader, MAX_TRANSACTION_ITEMS)?,
            }),
            _ => Err(Error::new(ErrorKind::InvalidData, "invalid spend variant")),
        }
    }
}

/// Explicit miner payment. The required protocol burn is derived by consensus.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SpendCharges {
    pub miner_fee: Zeno,
}

impl SpendCharges {
    pub const fn new(miner_fee: Zeno) -> Self {
        Self { miner_fee }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct SpendIntent {
    pub signer: Address,
    pub spend: Spend,
    pub charges: SpendCharges,
}

impl SpendIntent {
    pub fn coin(
        signer: Address,
        inputs: Vec<CoinShare>,
        outputs: Vec<CoinOutput>,
    ) -> Result<Self, IntentError> {
        Self::coin_with_charges(signer, inputs, outputs, SpendCharges::default())
    }

    pub fn coin_with_charges(
        signer: Address,
        inputs: Vec<CoinShare>,
        outputs: Vec<CoinOutput>,
        charges: SpendCharges,
    ) -> Result<Self, IntentError> {
        let intent = Self {
            signer,
            spend: Spend::Coin { inputs, outputs },
            charges,
        };
        intent.validate()?;
        Ok(intent)
    }

    pub fn with_charges(mut self, charges: SpendCharges) -> Result<Self, IntentError> {
        self.charges = charges;
        self.validate()?;
        Ok(self)
    }

    pub fn validate(&self) -> Result<(), IntentError> {
        let Spend::Coin { inputs, outputs } = &self.spend;
        if inputs.len() > MAX_TRANSACTION_ITEMS || outputs.len() > MAX_TRANSACTION_ITEMS {
            return Err(IntentError::TooManyItems);
        }
        if inputs.is_empty() {
            return Err(IntentError::EmptyInputs);
        }
        if outputs.is_empty() && self.charges.miner_fee.is_zero() {
            return Err(IntentError::EmptyOutputs);
        }
        let mut unique = BTreeSet::new();
        if inputs.iter().any(|id| !unique.insert(*id)) {
            return Err(IntentError::DuplicateInput);
        }
        if outputs.iter().any(|output| output.amount.is_zero()) {
            return Err(IntentError::ZeroAmount);
        }
        Ok(())
    }

    /// Canonical bytes of the unsigned SpendIntent semantics.
    pub fn semantic_bytes(&self, chain: ChainContext) -> Result<Vec<u8>, IntentError> {
        self.validate()?;
        canonical_bytes(&(chain.genesis_hash, self)).map_err(|_| IntentError::Encoding)
    }

    /// Semantic SpendIntent commitment.
    ///
    /// This identifies the unsigned spend semantics. Account signatures must
    /// use the role-bound `AuthorizationCommitment` instead.
    pub fn semantic_commitment(
        &self,
        chain: ChainContext,
    ) -> Result<SpendIntentCommitment, IntentError> {
        let bytes = self.semantic_bytes(chain)?;
        Ok(SpendIntentCommitment::from_bytes(
            domain(HashDomain::SpendIntent, &bytes).into_bytes(),
        ))
    }

    pub fn coin_parts(&self) -> Option<(&[CoinShare], &[CoinOutput])> {
        match &self.spend {
            Spend::Coin { inputs, outputs } => Some((inputs, outputs)),
        }
    }
}

#[cfg(test)]
mod conservation_tests {
    use super::*;
    use crate::monetary::coin::{CoinOutput, CoinShare, Zeno};
    use crypto::HASH16_SIZE;

    fn address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
    }

    #[test]
    fn duplicate_coin_inputs_are_rejected_structurally() {
        let input = CoinShare::from_bytes([0x11; HASH16_SIZE]);

        let result = SpendIntent::coin(
            address(1),
            vec![input, input],
            vec![CoinOutput::new(address(2), Zeno::from_zeno(1))],
        );

        assert!(matches!(result, Err(IntentError::DuplicateInput)));
    }

    #[test]
    fn zero_value_coin_output_is_rejected_structurally() {
        let input = CoinShare::from_bytes([0x44; HASH16_SIZE]);

        let result = SpendIntent::coin(
            address(1),
            vec![input],
            vec![CoinOutput::new(address(2), Zeno::ZERO)],
        );

        assert!(matches!(result, Err(IntentError::ZeroAmount)));
    }
}

#[cfg(test)]
mod removed_legacy_tests {
    use super::*;

    #[test]
    fn removed_combined_spend_tag_is_rejected() {
        assert!(Spend::try_from_slice(&[1]).is_err());
    }
}
