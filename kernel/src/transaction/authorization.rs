use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{
    AccountSignature, Address, HASH_SIZE, HashDomain, PublicKey, address_from_public_key,
    canonical_bytes, domain, verify,
};

use crate::common::ChainContext;
use crate::transaction::{AssetIntent, IntentError, Spend, SpendIntent, TransactionEncodingError};

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
const INTENT_KIND_COMBINED_SPEND: u8 = 2;
const INTENT_KIND_ASSET_CALL: u8 = 3;

/// Something that can be authorized by an account.
///
/// Coin and asset spends both use the same Principal role.
/// Their canonical encodings remain different because Spend is an enum.
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

impl AccountIntent for AssetIntent {
    fn sender(&self) -> Address {
        self.signer
    }

    fn principal_commitment(
        &self,
        chain: ChainContext,
    ) -> Result<AuthorizationCommitment, IntentError> {
        self.validate_structure()
            .map_err(|_| IntentError::InvalidAssetCall)?;

        let bytes = canonical_bytes(&(chain.genesis_hash, AuthorizationRole::Principal, self))
            .map_err(|_| IntentError::Encoding)?;

        Ok(AuthorizationCommitment::from_bytes(
            domain(HashDomain::AssetIntent, &bytes).into_bytes(),
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
pub struct AuthorizedAssetTransaction {
    pub call: AssetIntent,
    pub payment: SpendIntent,
    pub authorization: AccountAuthorization,
}

impl AuthorizedAssetTransaction {
    pub fn verify_authorizations(
        &self,
        chain: ChainContext,
        height: u64,
    ) -> Result<bool, IntentError> {
        let commitment = asset_call_commitment(&self.call, &self.payment, chain)?;
        Ok(self
            .authorization
            .verify_commitment(self.call.signer, &commitment, height))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum AuthorizedTransaction {
    Spend(Box<AuthorizedAccountIntent<SpendIntent>>),
    Asset(Box<AuthorizedAssetTransaction>),
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

                Spend::Combined { .. } => canonical_bytes(&(
                    TRANSACTION_INTENT_ID_TAG,
                    INTENT_KIND_COMBINED_SPEND,
                    &tx.intent,
                )),
            },

            Self::Asset(tx) => canonical_bytes(&(
                TRANSACTION_INTENT_ID_TAG,
                INTENT_KIND_ASSET_CALL,
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

            Self::Asset(tx) => asset_call_structure(&tx.call, &tx.payment),
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

            Self::Asset(tx) => tx.verify_authorizations(chain, height),
        }
    }
}

fn asset_call_structure(call: &AssetIntent, payment: &SpendIntent) -> Result<(), IntentError> {
    call.validate_structure()
        .map_err(|_| IntentError::InvalidAssetCall)?;
    payment.validate()?;
    if call.signer != payment.signer || !matches!(&payment.spend, Spend::Coin { .. }) {
        return Err(IntentError::InvalidAssetCall);
    }

    Ok(())
}

/// One signature authorizes the asset operation and its XPQ payment together.
pub fn asset_call_commitment(
    call: &AssetIntent,
    payment: &SpendIntent,
    chain: ChainContext,
) -> Result<AuthorizationCommitment, IntentError> {
    asset_call_structure(call, payment)?;

    let bytes = canonical_bytes(&(
        chain.genesis_hash,
        AuthorizationRole::Principal,
        b"xparq:asset-call-authorization:v1",
        call,
        payment,
    ))
    .map_err(|_| IntentError::Encoding)?;

    Ok(AuthorizationCommitment::from_bytes(
        domain(HashDomain::AssetIntent, &bytes).into_bytes(),
    ))
}
