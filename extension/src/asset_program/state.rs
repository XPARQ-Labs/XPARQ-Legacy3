use std::collections::{BTreeMap, BTreeSet};

use borsh::{BorshDeserialize, BorshSerialize};
use crypto::{Address, HASH_SIZE};

use super::{
    asset::{AssetContract, AssetError, AssetShare, Metadata, Share, Unit},
    type_::AssetCall,
};

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetRecord {
    pub metadata: Metadata,
    pub supply: Unit,
    pub total_minted: Unit,
    pub mint_nonce: u64,
    pub total_burned: Unit,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetState {
    pub records: BTreeMap<AssetContract, AssetRecord>,
    pub shares: BTreeMap<Share, AssetShare>,
}

/// The caller must authenticate `signer` and bind `commitment` to this call.
#[derive(Debug, Clone, Copy)]
pub struct ExecutionContext {
    pub signer: Address,
    pub commitment: [u8; HASH_SIZE],
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AssetJournal {
    records: Vec<(AssetContract, Option<AssetRecord>)>,
    shares: Vec<(Share, Option<AssetShare>)>,
}

impl AssetState {
    pub fn apply(
        &mut self,
        call: &AssetCall,
        context: ExecutionContext,
    ) -> Result<AssetJournal, AssetError> {
        use super::opcode::AssetOpcode;
        let (opcode, payload) = match call {
            AssetCall::Register(value) => (AssetOpcode::Register, borsh::to_vec(value)),
            AssetCall::Mint(value) => (AssetOpcode::Mint, borsh::to_vec(value)),
            AssetCall::Transfer(value) => (AssetOpcode::Transfer, borsh::to_vec(value)),
            AssetCall::Burn(value) => (AssetOpcode::Burn, borsh::to_vec(value)),
        };
        let payload = payload.map_err(|_| AssetError::Encoding)?;
        super::decode(opcode as u8, &payload).map_err(|_| AssetError::InvalidProgram)?;
        let mut next = self.clone();
        next.execute(call, context)?;
        let record_keys: BTreeSet<_> = self
            .records
            .keys()
            .chain(next.records.keys())
            .copied()
            .collect();
        let share_keys: BTreeSet<_> = self
            .shares
            .keys()
            .chain(next.shares.keys())
            .copied()
            .collect();
        let journal = AssetJournal {
            records: record_keys
                .into_iter()
                .filter(|key| self.records.get(key) != next.records.get(key))
                .map(|key| (key, self.records.get(&key).cloned()))
                .collect(),
            shares: share_keys
                .into_iter()
                .filter(|key| self.shares.get(key) != next.shares.get(key))
                .map(|key| (key, self.shares.get(&key).cloned()))
                .collect(),
        };
        *self = next;
        Ok(journal)
    }

    pub fn rollback(&mut self, journal: AssetJournal) {
        for (key, previous) in journal.records {
            match previous {
                Some(record) => {
                    self.records.insert(key, record);
                }
                None => {
                    self.records.remove(&key);
                }
            }
        }
        for (key, previous) in journal.shares {
            match previous {
                Some(share) => {
                    self.shares.insert(key, share);
                }
                None => {
                    self.shares.remove(&key);
                }
            }
        }
    }

    fn input_total(
        &self,
        asset: AssetContract,
        inputs: &[Share],
        signer: Address,
    ) -> Result<Unit, AssetError> {
        let mut total = Unit::ZERO;
        for input in inputs {
            let share = self.shares.get(input).ok_or(AssetError::UnknownObject)?;
            if share.asset != asset {
                return Err(AssetError::AssetMismatch);
            }
            if share.owner != signer {
                return Err(AssetError::Unauthorized);
            }
            total = total
                .checked_add(share.amount)
                .ok_or(AssetError::BalanceOverflow)?;
        }
        Ok(total)
    }

    fn insert_share(
        &mut self,
        asset: AssetContract,
        commitment: [u8; HASH_SIZE],
        index: usize,
        amount: Unit,
        owner: Address,
    ) -> Result<(), AssetError> {
        let index = u32::try_from(index).map_err(|_| AssetError::InvalidProgram)?;
        let id = Share::derive(asset, commitment, index);
        if self
            .shares
            .insert(id, AssetShare::new(asset, amount, owner))
            .is_some()
        {
            return Err(AssetError::ShareAlreadyExists);
        }
        Ok(())
    }

    fn execute(&mut self, call: &AssetCall, context: ExecutionContext) -> Result<(), AssetError> {
        match call {
            AssetCall::Register(call) => {
                let metadata = Metadata::new(
                    call.name.clone(),
                    call.max_supply,
                    context.signer,
                    call.mint_authority,
                )?;
                if call.initial_mint.is_zero() || call.initial_mint > call.max_supply {
                    return Err(AssetError::InvalidAmount);
                }
                let asset = AssetContract::derive(&metadata, call.nonce)?;
                if self.records.contains_key(&asset) {
                    return Err(AssetError::AssetAlreadyExists);
                }
                self.insert_share(
                    asset,
                    context.commitment,
                    0,
                    call.initial_mint,
                    context.signer,
                )?;
                self.records.insert(
                    asset,
                    AssetRecord {
                        metadata,
                        supply: call.initial_mint,
                        total_minted: call.initial_mint,
                        mint_nonce: 0,
                        total_burned: Unit::ZERO,
                    },
                );
            }
            AssetCall::Mint(call) => {
                if call.amount.is_zero() {
                    return Err(AssetError::InvalidAmount);
                }
                let record = self
                    .records
                    .get(&call.asset)
                    .ok_or(AssetError::UnknownAsset)?;
                if record.metadata.mint_authority != context.signer {
                    return Err(AssetError::Unauthorized);
                }
                if call.nonce
                    != record
                        .mint_nonce
                        .checked_add(1)
                        .ok_or(AssetError::InvalidMintNonce)?
                {
                    return Err(AssetError::InvalidMintNonce);
                }
                let supply = record
                    .supply
                    .checked_add(call.amount)
                    .ok_or(AssetError::SupplyOverflow)?;
                if supply > record.metadata.max_supply {
                    return Err(AssetError::SupplyOverflow);
                }
                let total_minted = record
                    .total_minted
                    .checked_add(call.amount)
                    .ok_or(AssetError::SupplyOverflow)?;
                self.insert_share(
                    call.asset,
                    context.commitment,
                    0,
                    call.amount,
                    call.recipient,
                )?;
                let record = self
                    .records
                    .get_mut(&call.asset)
                    .ok_or(AssetError::UnknownAsset)?;
                record.supply = supply;
                record.total_minted = total_minted;
                record.mint_nonce = call.nonce;
            }
            AssetCall::Transfer(call) => {
                if !self.records.contains_key(&call.asset) {
                    return Err(AssetError::UnknownAsset);
                }
                if call.inputs.is_empty() || call.outputs.is_empty() {
                    return Err(AssetError::InvalidProgram);
                }
                super::asset::ensure_unique_asset_inputs(&call.inputs)?;
                let input_total = self.input_total(call.asset, &call.inputs, context.signer)?;
                let mut output_total = Unit::ZERO;
                for output in &call.outputs {
                    if output.amount.is_zero() {
                        return Err(AssetError::InvalidAmount);
                    }
                    output_total = output_total
                        .checked_add(output.amount)
                        .ok_or(AssetError::BalanceOverflow)?;
                }
                if input_total != output_total {
                    return Err(AssetError::InvalidAmount);
                }
                for input in &call.inputs {
                    self.shares.remove(input);
                }
                for (index, output) in call.outputs.iter().enumerate() {
                    self.insert_share(
                        call.asset,
                        context.commitment,
                        index,
                        output.amount,
                        output.recipient,
                    )?;
                }
            }
            AssetCall::Burn(call) => {
                if call.inputs.is_empty() || call.amount.is_zero() {
                    return Err(AssetError::InvalidAmount);
                }
                super::asset::ensure_unique_asset_inputs(&call.inputs)?;
                let input_total = self.input_total(call.asset, &call.inputs, context.signer)?;
                let expected = call
                    .amount
                    .checked_add(call.output)
                    .ok_or(AssetError::BalanceOverflow)?;
                if input_total != expected {
                    return Err(AssetError::InvalidAmount);
                }
                let record = self
                    .records
                    .get(&call.asset)
                    .ok_or(AssetError::UnknownAsset)?;
                let supply = record
                    .supply
                    .checked_sub(call.amount)
                    .ok_or(AssetError::SupplyOverflow)?;
                let total_burned = record
                    .total_burned
                    .checked_add(call.amount)
                    .ok_or(AssetError::SupplyOverflow)?;
                for input in &call.inputs {
                    self.shares.remove(input);
                }
                if !call.output.is_zero() {
                    self.insert_share(
                        call.asset,
                        context.commitment,
                        0,
                        call.output,
                        context.signer,
                    )?;
                }
                let record = self
                    .records
                    .get_mut(&call.asset)
                    .ok_or(AssetError::UnknownAsset)?;
                record.supply = supply;
                record.total_burned = total_burned;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::asset_program::{
        asset::AssetOutput,
        type_::{Burn, Mint, Register, Transfer},
    };

    #[test]
    fn lifecycle_authorization_conservation_and_rollback() {
        let owner = Address::from_bytes([1; crypto::ADDRESS_SIZE]);
        let receiver = Address::from_bytes([2; crypto::ADDRESS_SIZE]);
        let context = |signer, byte| ExecutionContext {
            signer,
            commitment: [byte; HASH_SIZE],
        };
        let mut state = AssetState::default();
        let register = Register {
            name: "TEST".into(),
            max_supply: Unit::from_units(100),
            initial_mint: Unit::from_units(40),
            mint_authority: owner,
            nonce: 7,
        };
        let register_journal = state
            .apply(&AssetCall::Register(register.clone()), context(owner, 1))
            .unwrap();
        let metadata = Metadata::new(register.name, register.max_supply, owner, owner).unwrap();
        let asset = AssetContract::derive(&metadata, 7).unwrap();
        let first = Share::derive(asset, [1; HASH_SIZE], 0);
        let mint = AssetCall::Mint(Mint {
            asset,
            nonce: 1,
            recipient: owner,
            amount: Unit::from_units(20),
        });
        let before = state.clone();
        assert_eq!(
            state.apply(&mint, context(receiver, 2)),
            Err(AssetError::Unauthorized)
        );
        assert_eq!(state, before);
        let mint_journal = state.apply(&mint, context(owner, 2)).unwrap();
        let second = Share::derive(asset, [2; HASH_SIZE], 0);
        let transfer = AssetCall::Transfer(Transfer {
            asset,
            inputs: vec![first, second],
            outputs: vec![AssetOutput::new(receiver, Unit::from_units(60))],
        });
        let before = state.clone();
        assert_eq!(
            state.apply(&transfer, context(receiver, 3)),
            Err(AssetError::Unauthorized)
        );
        assert_eq!(state, before);
        let journal = state.apply(&transfer, context(owner, 3)).unwrap();
        let received = Share::derive(asset, [3; HASH_SIZE], 0);
        assert_eq!(state.shares.get(&received).unwrap().owner, receiver);
        state.rollback(journal);
        assert_eq!(state, before);
        let transfer_journal = state.apply(&transfer, context(owner, 3)).unwrap();
        let burn = AssetCall::Burn(Burn {
            asset,
            inputs: vec![received],
            amount: Unit::from_units(10),
            output: Unit::from_units(50),
        });
        assert_eq!(
            state.apply(&burn, context(owner, 4)),
            Err(AssetError::Unauthorized)
        );
        let burn_journal = state.apply(&burn, context(receiver, 4)).unwrap();
        assert_eq!(state.records[&asset].supply, Unit::from_units(50));
        assert_eq!(state.records[&asset].total_burned, Unit::from_units(10));
        for journal in [
            burn_journal,
            transfer_journal,
            mint_journal,
            register_journal,
        ] {
            let encoded = borsh::to_vec(&journal).unwrap();
            state.rollback(AssetJournal::try_from_slice(&encoded).unwrap());
        }
        assert_eq!(state, AssetState::default());
    }
}
