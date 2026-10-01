use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{
    AccountSignature, Address, HASH_SIZE, HashDomain, PublicKey, address_from_public_key,
    canonical_bytes, domain, verify,
};
use extension::script::{call::ProgramCall, execute::decode_program};

use crate::common::ChainContext;
use crate::transaction::{IntentError, Spend, SpendIntent, TransactionEncodingError};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
#[repr(u8)]
#[borsh(use_discriminant = true)]
pub enum AuthorizationRole {
    Principal = 1,
    Payment = 2,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct AuthorizationCommitment([u8; HASH_SIZE]);

impl AuthorizationCommitment {
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

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct IntentId([u8; HASH_SIZE]);

impl IntentId {
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

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct TransactionId([u8; HASH_SIZE]);

impl TransactionId {
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

const TRANSACTION_INTENT_ID_TAG: [u8; 27] = *b"xparq:transaction-intent:v1";

const INTENT_KIND_COIN_SPEND: u8 = 1;
const INTENT_KIND_PROGRAM_CALL: u8 = 4;

/// Something that can be authorized by an account.
///
/// Native coin spends use the Principal authorization role.
pub trait AccountIntent {
    fn sender(&self) -> Address;

    fn principal_commitment(
        &self,
        chain: ChainContext,
    ) -> Result<AuthorizationCommitment, IntentError>;
}

impl AccountIntent for SpendIntent {
    fn sender(&self) -> Address {
        self.signer
    }

    fn principal_commitment(
        &self,
        chain: ChainContext,
    ) -> Result<AuthorizationCommitment, IntentError> {
        self.validate()?;

        let bytes = canonical_bytes(&(chain.genesis_hash, AuthorizationRole::Principal, self))
            .map_err(|_| IntentError::Encoding)?;

        Ok(AuthorizationCommitment::from_bytes(
            domain(HashDomain::SpendIntent, &bytes).into_bytes(),
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AccountAuthorization {
    pub public_key: PublicKey,
    pub signature: AccountSignature,
}

impl AccountAuthorization {
    pub fn has_matching_scheme(&self) -> bool {
        self.public_key.scheme() == self.signature.scheme()
    }

    pub fn active_at_height(&self, _height: u64) -> bool {
        self.has_matching_scheme() && self.public_key.scheme().supported()
    }

    pub fn verify_commitment(
        &self,
        sender: Address,
        commitment: &AuthorizationCommitment,
        height: u64,
    ) -> bool {
        if !self.active_at_height(height) {
            return false;
        }

        if address_from_public_key(&self.public_key) != sender {
            return false;
        }

        verify(&self.public_key, commitment.as_bytes(), &self.signature)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AuthorizedAccountIntent<T> {
    pub intent: T,
    pub authorization: AccountAuthorization,
}

impl<T: AccountIntent> AuthorizedAccountIntent<T> {
    pub fn verify_principal(&self, chain: ChainContext, height: u64) -> Result<bool, IntentError> {
        let commitment = self.intent.principal_commitment(chain)?;

        Ok(self
            .authorization
            .verify_commitment(self.intent.sender(), &commitment, height))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AuthorizedProgramTransaction {
    pub signer: Address,
    pub call: ProgramCall,
    pub payment: SpendIntent,
    pub authorization: AccountAuthorization,
}

impl AuthorizedProgramTransaction {
    pub fn validate_structure(&self) -> Result<(), IntentError> {
        decode_program(&self.call).map_err(|_| IntentError::InvalidAssetCall)?;
        self.payment.validate()?;
        if self.signer != self.payment.signer || !matches!(&self.payment.spend, Spend::Coin { .. })
        {
            return Err(IntentError::InvalidAssetCall);
        }
        Ok(())
    }

    pub fn verify_authorizations(
        &self,
        chain: ChainContext,
        height: u64,
    ) -> Result<bool, IntentError> {
        let commitment =
            program_transaction_commitment(self.signer, &self.call, &self.payment, chain)?;
        Ok(self
            .authorization
            .verify_commitment(self.signer, &commitment, height))
    }
}

/// One signature binds the program call and its XPQ payment to this chain.
pub fn program_transaction_commitment(
    signer: Address,
    call: &ProgramCall,
    payment: &SpendIntent,
    chain: ChainContext,
) -> Result<AuthorizationCommitment, IntentError> {
    decode_program(call).map_err(|_| IntentError::InvalidAssetCall)?;
    payment.validate()?;
    if signer != payment.signer || !matches!(&payment.spend, Spend::Coin { .. }) {
        return Err(IntentError::InvalidAssetCall);
    }
    let bytes = canonical_bytes(&(
        chain.genesis_hash,
        AuthorizationRole::Principal,
        b"xparq:program-transaction:v1",
        signer,
        call,
        payment,
    ))
    .map_err(|_| IntentError::Encoding)?;
    Ok(AuthorizationCommitment::from_bytes(
        domain(HashDomain::AssetIntent, &bytes).into_bytes(),
    ))
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum AuthorizedTransaction {
    Spend(Box<AuthorizedAccountIntent<SpendIntent>>),
    Program(Box<AuthorizedProgramTransaction>),
}

impl AuthorizedTransaction {
    pub fn transaction_id(&self) -> Result<TransactionId, TransactionEncodingError> {
        let bytes = canonical_bytes(self).map_err(|_| TransactionEncodingError::Encoding)?;

        Ok(TransactionId::from_bytes(
            domain(HashDomain::Transaction, &bytes).into_bytes(),
        ))
    }

    pub fn intent_id(&self) -> Result<IntentId, TransactionEncodingError> {
        self.validate_structure()
            .map_err(|_| TransactionEncodingError::Encoding)?;

        let bytes = match self {
            Self::Spend(tx) => match &tx.intent.spend {
                Spend::Coin { .. } => canonical_bytes(&(
                    TRANSACTION_INTENT_ID_TAG,
                    INTENT_KIND_COIN_SPEND,
                    &tx.intent,
                )),
            },

            Self::Program(tx) => canonical_bytes(&(
                TRANSACTION_INTENT_ID_TAG,
                INTENT_KIND_PROGRAM_CALL,
                tx.signer,
                &tx.call,
                &tx.payment,
            )),
        }
        .map_err(|_| TransactionEncodingError::Encoding)?;

        Ok(IntentId::from_bytes(
            domain(HashDomain::Transaction, &bytes).into_bytes(),
        ))
    }

    pub fn id(&self) -> Result<[u8; HASH_SIZE], TransactionEncodingError> {
        Ok(self.transaction_id()?.into_bytes())
    }

    pub fn validate_structure(&self) -> Result<(), IntentError> {
        match self {
            Self::Spend(tx) => tx.intent.validate(),

            Self::Program(tx) => tx.validate_structure(),
        }
    }

    pub fn verify_authorizations(
        &self,
        chain: ChainContext,
        height: u64,
    ) -> Result<bool, IntentError> {
        self.validate_structure()?;

        match self {
            Self::Spend(tx) => tx.verify_principal(chain, height),

            Self::Program(tx) => tx.verify_authorizations(chain, height),
        }
    }
}

#[cfg(test)]
mod program_transaction_tests {
    use super::*;
    use crate::{
        consensus::{
            CoinInputState, TransactionConsensusError, TransactionStateView, validate_transaction,
        },
        monetary::coin::{CoinOutput, CoinShare, Zeno},
    };
    use crypto::{AccountSignatureScheme, SigningSeed};
    use extension::{
        asset_program::{asset::Unit, opcode::AssetOpcode, type_::Register},
        script::call::ProgramId,
    };

    struct EmptyState;
    impl TransactionStateView for EmptyState {
        fn coin(&self, _: CoinShare) -> Option<CoinInputState> {
            None
        }
    }

    #[test]
    fn program_envelope_binds_payment_and_requires_state_view() {
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([14; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let chain = ChainContext::new([8; HASH_SIZE]);
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: AssetOpcode::Register as u8,
            payload: borsh::to_vec(&Register {
                name: "PROGRAM".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: signer,
                nonce: 1,
            })
            .unwrap(),
        };
        let payment = SpendIntent::coin(
            signer,
            vec![CoinShare::from_bytes([1; crypto::HASH16_SIZE])],
            vec![CoinOutput::new(signer, Zeno::from_zeno(1))],
        )
        .unwrap();
        let commitment = program_transaction_commitment(signer, &call, &payment, chain).unwrap();
        let signed = AuthorizedProgramTransaction {
            signer,
            call,
            payment,
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        };
        assert!(signed.verify_authorizations(chain, 0).unwrap());
        assert!(
            !signed
                .verify_authorizations(ChainContext::new([9; HASH_SIZE]), 0)
                .unwrap()
        );
        let mut changed = signed.clone();
        changed.payment.charges.miner_fee = Zeno::from_zeno(1);
        assert!(!changed.verify_authorizations(chain, 0).unwrap());
        let transaction = AuthorizedTransaction::Program(Box::new(signed));
        let encoded = borsh::to_vec(&transaction).unwrap();
        assert_eq!(
            AuthorizedTransaction::try_from_slice(&encoded).unwrap(),
            transaction
        );
        assert!(matches!(
            validate_transaction(transaction, chain, 0, &EmptyState),
            Err(TransactionConsensusError::Intent(
                IntentError::InvalidAssetCall
            ))
        ));
    }
}
