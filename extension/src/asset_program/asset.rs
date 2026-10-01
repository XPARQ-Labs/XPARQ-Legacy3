use borsh::{BorshDeserialize, BorshSerialize};
use crypto::{
    Address, HASH_SIZE, HASH16_SIZE, Hash, Hash16, HashDomain, HashParseError, canonical_bytes,
    domain, domain16,
};

use std::{collections::BTreeSet, error::Error, fmt, str::FromStr};

pub const ASSET_NAME_MAX_LEN: usize = 64;
pub const ASSET_DECIMALS: u8 = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AssetError {
    InvalidAmount,
    InvalidProgram,
    AssetAlreadyExists,
    ShareAlreadyExists,
    UnknownAsset,
    UnknownObject,
    AssetMismatch,
    Unauthorized,
    InvalidMintNonce,
    SupplyOverflow,
    BalanceOverflow,
    InsufficientBalance,
    Encoding,
}

impl fmt::Display for AssetError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{self:?}")
    }
}

impl Error for AssetError {}

/// Raw unit amount of one native asset.
#[derive(
    Debug,
    Clone,
    Copy,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    BorshSerialize,
    BorshDeserialize,
)]
pub struct Unit(u128);

impl Unit {
    pub const ZERO: Self = Self(0);

    pub const fn from_units(units: u128) -> Self {
        Self(units)
    }

    pub const fn as_units(self) -> u128 {
        self.0
    }

    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(units) => Some(Self(units)),
            None => None,
        }
    }

    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(units) => Some(Self(units)),
            None => None,
        }
    }

    pub const fn saturating_add(self, rhs: Self) -> Self {
        Self(self.0.saturating_add(rhs.0))
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }
}

impl fmt::Display for Unit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

/// Immutable definition of one native asset.
///
/// The canonical Borsh encoding of this structure is committed
/// into the corresponding `Asset` identifier.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Metadata {
    // Rename to Metadata
    pub name: String,
    pub max_supply: Unit,
    pub creator: Address,
    pub mint_authority: Address,
}

impl Metadata {
    pub fn new(
        name: String,
        max_supply: Unit,
        creator: Address,
        mint_authority: Address,
    ) -> Result<Self, AssetError> {
        let metadata = Self {
            name,
            max_supply,
            creator,
            mint_authority,
        };

        metadata.validate()?;

        Ok(metadata)
    }

    pub fn validate(&self) -> Result<(), AssetError> {
        validate_asset_name(&self.name)?;

        if self.max_supply.is_zero() {
            return Err(AssetError::InvalidProgram);
        }

        Ok(())
    }

    pub const fn max_supply_amount(&self) -> Unit {
        self.max_supply
    }
}

pub fn validate_asset_name(name: &str) -> Result<(), AssetError> {
    if name.is_empty()
        || name.len() > ASSET_NAME_MAX_LEN
        || name.trim() != name
        || !name
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(AssetError::InvalidProgram);
    }

    Ok(())
}

/// Cryptographic identifier of one native asset.
///
/// `Asset = H(HashDomain::Asset || Borsh(Metadata))`
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct AssetContract(Hash);
impl AssetContract {
    pub fn derive(metadata: &Metadata, nonce: u64) -> Result<Self, AssetError> {
        let bytes = canonical_bytes(&(metadata, nonce)).map_err(|_| AssetError::Encoding)?;

        Ok(Self::from_hash(domain(HashDomain::Asset, &bytes)))
    }

    pub const fn from_hash(hash: Hash) -> Self {
        Self(hash)
    }

    pub const fn from_bytes(bytes: [u8; HASH_SIZE]) -> Self {
        Self(Hash::from_bytes(bytes))
    }

    pub const fn as_hash(&self) -> &Hash {
        &self.0
    }

    pub const fn into_hash(self) -> Hash {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; HASH_SIZE] {
        self.0.as_bytes()
    }

    pub const fn into_bytes(self) -> [u8; HASH_SIZE] {
        self.0.into_bytes()
    }
}

impl From<Hash> for AssetContract {
    fn from(hash: Hash) -> Self {
        Self(hash)
    }
}

impl From<AssetContract> for Hash {
    fn from(contract: AssetContract) -> Self {
        contract.0
    }
}

impl fmt::Display for AssetContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        crypto::hash::format("", &self.0, formatter)
    }
}

impl FromStr for AssetContract {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        crypto::hash::parse("", value).map(Self)
    }
}

/// Unique identifier of one concrete native AssetContract share/UTXO.
///
/// A share ID is derived from:
///
/// - asset AssetContract
/// - transaction/output commitment
/// - output index
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct Share(Hash16);

impl Share {
    pub fn derive(asset: AssetContract, commitment: [u8; HASH_SIZE], output_index: u32) -> Self {
        let mut bytes = [0_u8; HASH_SIZE + HASH_SIZE + 4];

        bytes[..HASH_SIZE].copy_from_slice(asset.as_bytes());

        bytes[HASH_SIZE..HASH_SIZE * 2].copy_from_slice(&commitment);

        bytes[HASH_SIZE * 2..].copy_from_slice(&output_index.to_le_bytes());

        Self(domain16(HashDomain::Share, &bytes))
    }

    pub const fn from_hash(hash: Hash16) -> Self {
        Self(hash)
    }

    pub const fn from_bytes(bytes: [u8; HASH16_SIZE]) -> Self {
        Self(Hash16::from_bytes(bytes))
    }

    pub const fn as_hash(&self) -> &Hash16 {
        &self.0
    }

    pub const fn into_hash(self) -> Hash16 {
        self.0
    }

    pub const fn as_bytes(&self) -> &[u8; HASH16_SIZE] {
        self.0.as_bytes()
    }

    pub const fn into_bytes(self) -> [u8; HASH16_SIZE] {
        self.0.into_bytes()
    }
}

impl From<Hash16> for Share {
    fn from(hash: Hash16) -> Self {
        Self(hash)
    }
}

impl From<Share> for Hash16 {
    fn from(share: Share) -> Self {
        share.0
    }
}

impl fmt::Display for Share {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for Share {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<Hash16>().map(Self)
    }
}

#[cfg(test)]
mod identifier_tests {
    use super::*;

    #[test]
    fn contract_and_share_text_are_unprefixed_hex() {
        let contract_encoded = "cd".repeat(HASH_SIZE);
        let share_encoded = "cd".repeat(HASH16_SIZE);

        let contract = AssetContract::from_bytes([0xcd; HASH_SIZE]);
        let share = Share::from_bytes([0xcd; HASH16_SIZE]);

        assert_eq!(contract.to_string(), contract_encoded);
        assert_eq!(share.to_string(), share_encoded);

        assert_eq!(contract_encoded.parse::<AssetContract>(), Ok(contract));

        assert_eq!(share_encoded.parse::<Share>(), Ok(share));

        assert!(
            format!("asset:{contract_encoded}")
                .parse::<AssetContract>()
                .is_err()
        );

        assert!(format!("share:{share_encoded}").parse::<Share>().is_err());
    }
}
/// One concrete live native asset share.
///
/// `Share` is its unique UTXO identifier, while `AssetShare`
/// contains the live state associated with that identifier.
#[derive(
    BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct AssetShare {
    pub asset: AssetContract,
    pub amount: Unit,
    pub owner: Address,
}

impl AssetShare {
    pub const fn new(asset: AssetContract, amount: Unit, owner: Address) -> Self {
        Self {
            asset,
            amount,
            owner,
        }
    }

    pub const fn is_zero(self) -> bool {
        self.amount.is_zero()
    }
}

/// Transaction output that creates a new native asset share.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct AssetOutput {
    pub recipient: Address,
    pub amount: Unit,
}

impl AssetOutput {
    pub const fn new(recipient: Address, amount: Unit) -> Self {
        Self { recipient, amount }
    }
}

pub fn ensure_nonzero_asset_amount(value: Unit) -> Result<(), AssetError> {
    if value.is_zero() {
        Err(AssetError::InvalidProgram)
    } else {
        Ok(())
    }
}

pub fn ensure_unique_asset_inputs(inputs: &[Share]) -> Result<(), AssetError> {
    let mut seen = BTreeSet::new();
    if inputs.iter().any(|share| !seen.insert(*share)) {
        Err(AssetError::InvalidProgram)
    } else {
        Ok(())
    }
}

pub fn checked_asset_entry_weight<T: BorshSerialize>(
    current: u64,
    key_len: usize,
    value: &T,
) -> Result<u64, AssetError> {
    let value_len = crypto::canonical_bytes(value)
        .map_err(|_| AssetError::Encoding)?
        .len();
    let entry = u64::try_from(key_len.checked_add(value_len).ok_or(AssetError::Encoding)?)
        .map_err(|_| AssetError::Encoding)?;

    current.checked_add(entry).ok_or(AssetError::Encoding)
}
