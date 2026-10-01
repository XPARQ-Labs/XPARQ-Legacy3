use borsh::{BorshDeserialize, BorshSerialize};
use crypto::Address;

use super::asset::{AssetContract, AssetOutput, Share, Unit};

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Register {
    pub name: String,
    pub max_supply: Unit,
    pub initial_mint: Unit,
    pub mint_authority: Address,
    pub nonce: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Mint {
    pub asset: AssetContract,
    pub nonce: u64,
    pub recipient: Address,
    pub amount: Unit,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Transfer {
    pub asset: AssetContract,
    pub inputs: Vec<Share>,
    pub outputs: Vec<AssetOutput>,
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct Burn {
    pub asset: AssetContract,
    pub inputs: Vec<Share>,
    pub amount: Unit,
    pub output: Unit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AssetCall {
    Register(Register),
    Mint(Mint),
    Transfer(Transfer),
    Burn(Burn),
}
