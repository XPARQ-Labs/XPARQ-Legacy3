use std::{collections::BTreeMap, error::Error as StdError, fmt};

use borsh::{BorshDeserialize, BorshSerialize};
use crypto::Address;

use crate::monetary::coin::{CoinShare, Zeno};

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct CoinUtxo {
    pub amount: Zeno,
    pub owner: Address,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct UtxoSet {
    coins: BTreeMap<CoinShare, CoinUtxo>,
}

impl UtxoSet {
    pub fn coin(&self, id: &CoinShare) -> Option<&CoinUtxo> {
        self.coins.get(id)
    }

    pub fn insert_coin(&mut self, id: CoinShare, coin: CoinUtxo) -> Result<(), Error> {
        if self.coins.contains_key(&id) {
            return Err(Error::CoinCollision);
        }
        self.coins.insert(id, coin);
        Ok(())
    }

    pub fn consume_coin(&mut self, id: &CoinShare) -> Result<CoinUtxo, Error> {
        self.coins.remove(id).ok_or(Error::NotFound)
    }

    pub fn coins(&self) -> impl Iterator<Item = (CoinShare, &CoinUtxo)> + '_ {
        self.coins.iter().map(|(&id, coin)| (id, coin))
    }

    pub fn len(&self) -> usize {
        self.coins.len()
    }

    pub fn is_empty(&self) -> bool {
        self.coins.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotFound,
    CoinCollision,
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("UTXO was not found"),
            Self::CoinCollision => f.write_str("coin UTXO ID already exists"),
        }
    }
}

impl StdError for Error {}
