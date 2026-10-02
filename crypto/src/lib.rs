pub mod address;
pub mod agility;
pub mod argon2;
pub mod codec;
mod error;
pub mod hash;
pub mod signature;

pub mod crypto {
    pub use crate::*;
}

pub use address::*;
pub use agility::*;
pub use argon2::*;
pub use codec::{
    CANONICAL_ENCODING_PROFILE, CodecError, canonical_bytes, canonical_decode,
    canonical_deserialize,
};
pub use error::CryptoError;
pub use hash::*;
pub use signature::*;

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, borsh::BorshSerialize, borsh::BorshDeserialize,
)]
pub struct ChainContext {
    pub genesis_hash: [u8; crate::HASH_SIZE],
}

impl ChainContext {
    pub const fn new(genesis_hash: [u8; crate::HASH_SIZE]) -> Self {
        Self { genesis_hash }
    }
}
