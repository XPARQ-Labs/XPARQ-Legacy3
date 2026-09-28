use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{Address, HASH_SIZE, HashDomain, canonical_bytes, domain};

use crate::monetary::asset::{
    AssetContract, AssetError, AssetShare, Metadata, Share, Unit, ensure_nonzero_asset_amount,
    ensure_unique_asset_inputs,
};

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub enum AssetInstruction {
    Register {
        name: String,
        max_supply: Unit,
        initial_mint: Unit,
        mint_authority: Address,
        nonce: u64,
    },
    Mint {
        asset: AssetContract,
        nonce: u64,
        recipient: Address,
        amount: Unit,
    },
    Burn {
        asset: AssetContract,
        inputs: Vec<Share>,
        amount: Unit,
        output: Unit,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetIntent {
    pub instruction: AssetInstruction,
    pub signer: Address,
}

impl AssetIntent {
    pub const fn new(instruction: AssetInstruction, signer: Address) -> Self {
        Self {
            instruction,
            signer,
        }
    }

    pub fn asset(&self) -> Result<AssetContract, AssetError> {
        match &self.instruction {
            AssetInstruction::Register {
                name,
                max_supply,
                mint_authority,
                nonce,
                ..
            } => {
                let metadata =
                    Metadata::new(name.clone(), *max_supply, self.signer, *mint_authority)?;
                AssetContract::derive(&metadata, *nonce)
            }
            AssetInstruction::Mint { asset, .. } | AssetInstruction::Burn { asset, .. } => {
                Ok(*asset)
            }
        }
    }

    /// Semantic AssetIntent commitment.
    ///
    /// This identifies the unsigned asset-call semantics. Account signatures
    /// must use `AccountIntent::authorization_commitment`, which additionally
    /// binds the `AssetCall` authorization role.
    pub fn semantic_commitment(
        &self,
        genesis_hash: [u8; HASH_SIZE],
    ) -> Result<[u8; HASH_SIZE], AssetError> {
        self.validate_structure()?;
        let bytes = canonical_bytes(&(genesis_hash, self)).map_err(|_| AssetError::Encoding)?;
        Ok(domain(HashDomain::AssetIntent, &bytes).into_bytes())
    }

    /// Compatibility accessor. New code should use `semantic_commitment()`.
    pub fn commitment(&self, genesis_hash: [u8; HASH_SIZE]) -> Result<[u8; HASH_SIZE], AssetError> {
        self.semantic_commitment(genesis_hash)
    }

    pub fn validate_structure(&self) -> Result<(), AssetError> {
        match &self.instruction {
            AssetInstruction::Register {
                name,
                max_supply,
                initial_mint,
                mint_authority,
                ..
            } => {
                Metadata::new(name.clone(), *max_supply, self.signer, *mint_authority)?;

                ensure_nonzero_asset_amount(*initial_mint)?;
                if *initial_mint > *max_supply {
                    return Err(AssetError::InvalidAmount);
                }
            }
            AssetInstruction::Mint { amount, .. } => {
                ensure_nonzero_asset_amount(*amount)?;
            }
            AssetInstruction::Burn { inputs, amount, .. } => {
                if inputs.is_empty() {
                    return Err(AssetError::InvalidProgram);
                }

                ensure_unique_asset_inputs(inputs)?;
                ensure_nonzero_asset_amount(*amount)?;
            }
        }

        Ok(())
    }

    /// Newly-created canonical state weight. Consumed UTXOs are not counted.
    pub fn created_state_weight(&self) -> Result<u64, AssetError> {
        let mut weight = 0_u64;

        match &self.instruction {
            AssetInstruction::Register {
                name,
                max_supply,
                initial_mint,
                mint_authority,
                nonce,
            } => {
                let metadata =
                    Metadata::new(name.clone(), *max_supply, self.signer, *mint_authority)?;
                let asset = AssetContract::derive(&metadata, *nonce)?;

                weight = checked_entry_weight(
                    weight,
                    HASH_SIZE,
                    &(&metadata, initial_mint, initial_mint, 0_u64, Unit::ZERO),
                )?;
                weight = checked_entry_weight(
                    weight,
                    HASH_SIZE,
                    &AssetShare {
                        asset,
                        amount: *initial_mint,
                        owner: self.signer,
                    },
                )?;
            }
            AssetInstruction::Mint {
                asset,
                recipient,
                amount,
                ..
            } => {
                weight = checked_entry_weight(
                    weight,
                    HASH_SIZE,
                    &AssetShare {
                        asset: *asset,
                        amount: *amount,
                        owner: *recipient,
                    },
                )?;
            }
            AssetInstruction::Burn { asset, output, .. } => {
                if !output.is_zero() {
                    weight = checked_entry_weight(
                        weight,
                        HASH_SIZE,
                        &AssetShare {
                            asset: *asset,
                            amount: *output,
                            owner: self.signer,
                        },
                    )?;
                }
            }
        }

        Ok(weight)
    }
}

fn checked_entry_weight<T: BorshSerialize>(
    current: u64,
    key_len: usize,
    value: &T,
) -> Result<u64, AssetError> {
    let value_len = borsh::to_vec(value)
        .map_err(|_| AssetError::Encoding)?
        .len();
    let entry = key_len.checked_add(value_len).ok_or(AssetError::Encoding)?;
    let entry = u64::try_from(entry).map_err(|_| AssetError::Encoding)?;
    current.checked_add(entry).ok_or(AssetError::Encoding)
}
