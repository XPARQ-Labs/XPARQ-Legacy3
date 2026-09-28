use borsh::{BorshDeserialize, BorshSerialize};
use std::io::{Error, ErrorKind, Read};

use crypto::{Address, HASH_SIZE, HashDomain, canonical_bytes, domain};

use super::{MAX_TRANSACTION_ITEMS, deserialize_bounded_vec};
use crate::monetary::asset::{
    ASSET_NAME_MAX_LEN, AssetContract, AssetError, AssetShare, Metadata, Share, Unit,
    ensure_nonzero_asset_amount, ensure_unique_asset_inputs,
};

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize)]
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

impl BorshDeserialize for AssetInstruction {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        match u8::deserialize_reader(reader)? {
            0 => {
                let length = u32::deserialize_reader(reader)? as usize;
                if length > ASSET_NAME_MAX_LEN {
                    return Err(Error::new(
                        ErrorKind::InvalidData,
                        "asset name exceeds limit",
                    ));
                }
                let mut bytes = vec![0; length];
                reader.read_exact(&mut bytes)?;
                Ok(Self::Register {
                    name: String::from_utf8(bytes)
                        .map_err(|_| Error::new(ErrorKind::InvalidData, "invalid asset name"))?,
                    max_supply: Unit::deserialize_reader(reader)?,
                    initial_mint: Unit::deserialize_reader(reader)?,
                    mint_authority: Address::deserialize_reader(reader)?,
                    nonce: u64::deserialize_reader(reader)?,
                })
            }
            1 => Ok(Self::Mint {
                asset: AssetContract::deserialize_reader(reader)?,
                nonce: u64::deserialize_reader(reader)?,
                recipient: Address::deserialize_reader(reader)?,
                amount: Unit::deserialize_reader(reader)?,
            }),
            2 => Ok(Self::Burn {
                asset: AssetContract::deserialize_reader(reader)?,
                inputs: deserialize_bounded_vec(reader, MAX_TRANSACTION_ITEMS)?,
                amount: Unit::deserialize_reader(reader)?,
                output: Unit::deserialize_reader(reader)?,
            }),
            _ => Err(Error::new(
                ErrorKind::InvalidData,
                "invalid asset instruction",
            )),
        }
    }
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
                if inputs.len() > MAX_TRANSACTION_ITEMS {
                    return Err(AssetError::InvalidProgram);
                }
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
