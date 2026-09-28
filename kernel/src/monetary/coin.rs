use borsh::{BorshDeserialize, BorshSerialize};
use crypto::{
    Address, HASH_SIZE, HASH16_SIZE, Hash, Hash16, HashDomain, HashParseError, domain, domain16,
};
use std::{fmt, str::FromStr};

pub const DECIMALS: u8 = 8;

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
pub struct Zeno(u64);

impl Zeno {
    pub const ZERO: Self = Self(0);
    pub const ONE: Self = Self(1);

    pub const fn from_zeno(zeno: u64) -> Self {
        Self(zeno)
    }

    pub const fn as_zeno(self) -> u64 {
        self.0
    }

    pub const fn is_zero(self) -> bool {
        self.0 == 0
    }

    pub const fn checked_add(self, rhs: Self) -> Option<Self> {
        match self.0.checked_add(rhs.0) {
            Some(zeno) => Some(Self(zeno)),
            None => None,
        }
    }

    pub const fn checked_sub(self, rhs: Self) -> Option<Self> {
        match self.0.checked_sub(rhs.0) {
            Some(zeno) => Some(Self(zeno)),
            None => None,
        }
    }
}

/// Global identity of the native XPQ contract.
///
/// Unlike asset contracts, this contract is unique and protocol-defined.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct CoinContract(Hash);

impl CoinContract {
    pub const SIZE: usize = HASH_SIZE;

    pub fn derive() -> Self {
        Self(domain(HashDomain::XPQState, b"XPARQ_NATIVE_COIN_V1"))
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

impl From<Hash> for CoinContract {
    fn from(hash: Hash) -> Self {
        Self(hash)
    }
}

impl From<CoinContract> for Hash {
    fn from(contract: CoinContract) -> Self {
        contract.0
    }
}

impl fmt::Display for CoinContract {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        crypto::hash::format("", &self.0, formatter)
    }
}

impl FromStr for CoinContract {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        crypto::hash::parse("", value).map(Self)
    }
}

/// Unique identifier of one concrete XPQ share / UTXO.
///
/// Internally derived using SHA3-256 and truncated to 128 bits.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, BorshSerialize, BorshDeserialize,
)]
pub struct CoinShare(Hash16);

impl CoinShare {
    pub const SIZE: usize = HASH16_SIZE;
    pub const ZENO_PER_COIN: u64 = 10u64.pow(DECIMALS as u32);

    /// Derive an XPQ share created by protocol emission.
    ///
    /// CoinShare = H16(
    ///     XPQ CoinContract
    ///     || emission origin
    /// )
    pub fn from_emission_origin(origin: &[u8; HASH_SIZE]) -> Self {
        let contract = CoinContract::derive();

        let mut bytes = [0_u8; HASH_SIZE + HASH_SIZE];

        bytes[..HASH_SIZE].copy_from_slice(contract.as_bytes());
        bytes[HASH_SIZE..].copy_from_slice(origin);

        Self(domain16(HashDomain::Emission, &bytes))
    }

    /// Derive an XPQ share created by a normal transaction output.
    ///
    /// CoinShare = H16(
    ///     XPQ CoinContract
    ///     || SpendIntentCommitment
    ///     || output index
    /// )
    pub fn from_output(commitment: &[u8; HASH_SIZE], index: u32) -> Self {
        let contract = CoinContract::derive();

        let mut bytes = [0_u8; HASH_SIZE + HASH_SIZE + 4];

        bytes[..HASH_SIZE].copy_from_slice(contract.as_bytes());

        bytes[HASH_SIZE..HASH_SIZE + HASH_SIZE].copy_from_slice(commitment);

        bytes[HASH_SIZE + HASH_SIZE..].copy_from_slice(&index.to_le_bytes());

        Self(domain16(HashDomain::Output, &bytes))
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

impl From<Hash16> for CoinShare {
    fn from(hash: Hash16) -> Self {
        Self(hash)
    }
}

impl From<CoinShare> for Hash16 {
    fn from(share: CoinShare) -> Self {
        share.0
    }
}

impl fmt::Display for CoinShare {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(formatter)
    }
}

impl FromStr for CoinShare {
    type Err = HashParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value.parse::<Hash16>().map(Self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coin_contract_is_32_bytes() {
        let contract = CoinContract::derive();

        assert_eq!(contract.as_bytes().len(), HASH_SIZE);
        assert_eq!(contract.to_string().len(), HASH_SIZE * 2);
    }

    #[test]
    fn coin_share_is_16_bytes() {
        let share = CoinShare::from_bytes([0xab; HASH16_SIZE]);
        let encoded = "ab".repeat(HASH16_SIZE);

        assert_eq!(share.to_string(), encoded);
        assert_eq!(encoded.parse::<CoinShare>(), Ok(share));
        assert!(format!("XPQ:{encoded}").parse::<CoinShare>().is_err());
    }
}

#[test]
fn coin_share_derivation_is_deterministic() {
    let origin = [0x11; HASH_SIZE];

    let a = CoinShare::from_emission_origin(&origin);
    let b = CoinShare::from_emission_origin(&origin);

    assert_eq!(a, b);
}

#[test]
fn different_emission_origins_create_different_shares() {
    let a = CoinShare::from_emission_origin(&[0x11; HASH_SIZE]);
    let b = CoinShare::from_emission_origin(&[0x22; HASH_SIZE]);

    assert_ne!(a, b);
}

#[test]
fn different_output_indexes_create_different_shares() {
    let commitment = [0x33; HASH_SIZE];

    let a = CoinShare::from_output(&commitment, 0);
    let b = CoinShare::from_output(&commitment, 1);

    assert_ne!(a, b);
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct CoinOutput {
    pub output: Address,
    pub amount: Zeno,
}

impl CoinOutput {
    pub const fn new(recipient: Address, amount: Zeno) -> Self {
        Self {
            output: recipient,
            amount,
        }
    }
}
