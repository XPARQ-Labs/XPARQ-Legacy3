use borsh::{BorshDeserialize, BorshSerialize};
use serde::{Deserialize, Serialize};

use crypto::Address;

use crate::monetary::coin::Zeno;

pub use crypto::ChainContext;

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct Height(pub u64);

#[derive(
    BorshSerialize,
    BorshDeserialize,
    Serialize,
    Deserialize,
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
)]
pub struct Nonce(pub u64);

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Recipient {
    Address(Address),
    BlockMiner,
}

impl Recipient {
    pub const fn resolve(self, block_miner: Address) -> Address {
        match self {
            Self::Address(address) => address,
            Self::BlockMiner => block_miner,
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum Value {
    Coin(Zeno),
}

impl Value {
    pub const fn coin(self) -> Option<Zeno> {
        match self {
            Self::Coin(amount) => Some(amount),
        }
    }

    pub const fn is_zero(self) -> bool {
        match self {
            Self::Coin(amount) => amount.is_zero(),
        }
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct Output {
    pub to: Recipient,
    pub value: Value,
}

impl Output {
    pub const fn new(to: Address, value: Value) -> Self {
        Self {
            to: Recipient::Address(to),
            value,
        }
    }

    pub const fn block_miner(value: Value) -> Self {
        Self {
            to: Recipient::BlockMiner,
            value,
        }
    }

    pub const fn recipient(&self) -> Recipient {
        self.to
    }

    pub const fn value(&self) -> Value {
        self.value
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recipient_resolves_block_miner_at_application_time() {
        let miner = Address([9; crypto::ADDRESS_SIZE]);

        assert_eq!(Recipient::BlockMiner.resolve(miner), miner);
        assert_eq!(
            Recipient::Address(Address::ZERO).resolve(miner),
            Address::ZERO
        );
    }
}
