use std::collections::BTreeMap;

use borsh::BorshSerialize;

use crypto::{Address, HASH_SIZE};

use crate::{
    consensus::{AuthorizationValidated, ValidatedTransaction},
    ledger::{
        AssetRecord, AssetRollbackJournal, AssetState, CoinUtxo, LedgerState, SpendRollbackJournal,
        StateError, StateRollbackJournal, utxo,
    },
    monetary::{
        asset::{AssetContract, AssetError, AssetOutput, AssetShare, Metadata, Share, Unit},
        coin::{CoinShare, Zeno},
    },
    transaction::{AssetInstruction, AssetIntent, SpendIntent, SpendIntentCommitment},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionPoint {
    CoinInputConsumed,
    CoinOutputCreated,
    MinerFeeCreated,
    ProtocolBurnRecorded,
    AssetRecordUpdated,
    AssetInputConsumed,
    AssetOutputCreated,
}

//
// Apply validated transaction
//

impl LedgerState {
    pub fn apply_validated_transaction(
        &mut self,
        transaction: &ValidatedTransaction,
        block_miner: Address,
        chain: crate::common::ChainContext,
    ) -> Result<StateRollbackJournal, StateError> {
        match transaction {
            ValidatedTransaction::CoinSpend(transaction) => {
                let spend = self.apply_validated_onchain_spend(&transaction.spend, block_miner)?;
                Ok(StateRollbackJournal {
                    spend: Some(spend),
                    asset: None,
                })
            }
            ValidatedTransaction::CombinedSpend(transaction) => {
                let coin_journal =
                    self.apply_validated_onchain_spend(&transaction.spend, block_miner)?;
                let (asset, inputs, outputs) = transaction
                    .spend
                    .intent()
                    .asset_parts()
                    .ok_or(StateError::Asset(AssetError::InvalidProgram))?;
                let asset_journal = match self.assets.apply_account_transfer(
                    &mut self.utxos,
                    asset,
                    inputs,
                    outputs,
                    transaction.spend.commitment().into_bytes(),
                ) {
                    Ok(journal) => journal,
                    Err(error) => {
                        self.rollback_spend(coin_journal)?;
                        return Err(StateError::Asset(error));
                    }
                };
                Ok(StateRollbackJournal {
                    spend: Some(coin_journal),
                    asset: Some(asset_journal),
                })
            }
            ValidatedTransaction::AssetCall(transaction) => {
                let payment =
                    self.apply_validated_onchain_spend(&transaction.payment, block_miner)?;

                match self.assets.apply(
                    &mut self.utxos,
                    transaction.call.intent(),
                    chain.genesis_hash,
                ) {
                    Ok(asset) => Ok(StateRollbackJournal {
                        spend: Some(payment),
                        asset: Some(asset),
                    }),

                    Err(error) => {
                        self.rollback_spend(payment)?;
                        Err(StateError::Asset(error))
                    }
                }
            }
        }
    }
}

//
// Coin state transition
//

impl LedgerState {
    fn apply_validated_onchain_spend(
        &mut self,
        validated: &AuthorizationValidated<SpendIntent>,
        block_miner: Address,
    ) -> Result<SpendRollbackJournal, StateError> {
        self.apply_onchain_spend_with_commitment(
            validated.intent(),
            validated.commitment(),
            block_miner,
        )
    }

    fn apply_onchain_spend_with_commitment(
        &mut self,
        intent: &SpendIntent,
        commitment: SpendIntentCommitment,
        block_miner: Address,
    ) -> Result<SpendRollbackJournal, StateError> {
        self.apply_onchain_spend_with_checkpoint(intent, commitment, block_miner, |_| Ok(()))
    }

    fn apply_onchain_spend_with_checkpoint(
        &mut self,
        intent: &SpendIntent,
        commitment: SpendIntentCommitment,
        block_miner: Address,
        mut checkpoint: impl FnMut(TransitionPoint) -> Result<(), StateError>,
    ) -> Result<SpendRollbackJournal, StateError> {
        let mut journal = SpendRollbackJournal::default();

        let result = (|| {
            let (inputs, outputs) = intent.coin_parts().ok_or(StateError::InvalidTransaction)?;

            let input_total = inputs.iter().try_fold(Zeno::ZERO, |total, id| {
                total
                    .checked_add(
                        self.utxos
                            .coin(id)
                            .ok_or(StateError::InvalidTransaction)?
                            .amount,
                    )
                    .ok_or(StateError::AmountOverflow)
            })?;
            let output_total = outputs.iter().try_fold(Zeno::ZERO, |total, output| {
                total
                    .checked_add(output.amount)
                    .ok_or(StateError::AmountOverflow)
            })?;
            let burn = input_total
                .checked_sub(output_total)
                .and_then(|value| value.checked_sub(intent.charges.miner_fee))
                .ok_or(StateError::InvalidTransaction)?;

            //
            // Consume existing CoinShare objects.
            //
            for id in inputs {
                let coin = self.utxos.consume_coin(id)?;
                journal.consumed_coins.push((*id, coin));
                checkpoint(TransitionPoint::CoinInputConsumed)?;
            }

            //
            // Create new CoinShare objects.
            //
            for (index, output) in outputs.iter().enumerate() {
                let id = coin_output_id(commitment, index)?;

                //
                // Recipient is intentionally NOT stored
                // in the canonical UTXO state.
                //
                // Ownership is bound by the transaction
                // commitment and validated by consensus.
                //
                self.utxos.insert_coin(
                    id,
                    CoinUtxo {
                        amount: output.amount,
                        owner: output.output,
                    },
                )?;

                journal.created_coin_ids.push(id);
                checkpoint(TransitionPoint::CoinOutputCreated)?;
            }

            if !intent.charges.miner_fee.is_zero() {
                let id = coin_output_id(commitment, outputs.len())?;
                self.utxos.insert_coin(
                    id,
                    CoinUtxo {
                        amount: intent.charges.miner_fee,
                        owner: block_miner,
                    },
                )?;
                journal.created_coin_ids.push(id);
                checkpoint(TransitionPoint::MinerFeeCreated)?;
            }

            self.record_protocol_burn(burn, &mut journal)?;
            checkpoint(TransitionPoint::ProtocolBurnRecorded)?;

            Ok(())
        })();

        self.finish_spend_transition(journal, result)
    }
}

impl AssetState {
    pub fn apply(
        &mut self,
        utxos: &mut utxo::UtxoSet,
        call: &AssetIntent,
        genesis_hash: [u8; 32],
    ) -> Result<AssetRollbackJournal, AssetError> {
        self.apply_with_checkpoint(utxos, call, genesis_hash, |_| Ok(()))
    }

    fn apply_with_checkpoint(
        &mut self,
        utxos: &mut utxo::UtxoSet,
        call: &AssetIntent,
        genesis_hash: [u8; 32],
        mut checkpoint: impl FnMut(TransitionPoint) -> Result<(), AssetError>,
    ) -> Result<AssetRollbackJournal, AssetError> {
        call.validate_structure()?;

        self.validate_transition(utxos, call, genesis_hash)?;

        let commitment = call.commitment(genesis_hash)?;

        let created_share = match &call.instruction {
            AssetInstruction::Register { .. } => Some(Share::derive(call.asset()?, commitment, 0)),
            AssetInstruction::Mint { asset, .. } => Some(Share::derive(*asset, commitment, 0)),
            AssetInstruction::Burn { asset, output, .. } => {
                if output.is_zero() {
                    None
                } else {
                    Some(Share::derive(*asset, commitment, 0))
                }
            }
        };
        if created_share.is_some_and(|share| utxos.asset(&share).is_some()) {
            return Err(AssetError::ShareAlreadyExists);
        }

        let mut journal = AssetRollbackJournal::default();

        let result = (|| {
            match &call.instruction {
                AssetInstruction::Register {
                    name,
                    max_supply,
                    initial_mint,
                    mint_authority,
                    nonce,
                } => {
                    let metadata =
                        Metadata::new(name.clone(), *max_supply, call.signer, *mint_authority)?;

                    let asset = AssetContract::derive(&metadata, *nonce)?;

                    let share = Share::derive(asset, commitment, 0);

                    journal
                        .assets
                        .push((asset, self.assets.get(&asset).cloned()));

                    journal.utxos.push((share, utxos.asset(&share).copied()));
                    self.assets.insert(
                        asset,
                        AssetRecord {
                            metadata,
                            supply: *initial_mint,
                            total_minted: *initial_mint,
                            mint_nonce: 0,
                            total_burned: Unit::ZERO,
                        },
                    );
                    checkpoint(TransitionPoint::AssetRecordUpdated)?;

                    utxos
                        .insert_asset(
                            share,
                            AssetShare {
                                asset: asset,
                                amount: *initial_mint,
                                owner: call.signer,
                            },
                        )
                        .map_err(|_| AssetError::ShareAlreadyExists)?;
                    checkpoint(TransitionPoint::AssetOutputCreated)?;
                }

                AssetInstruction::Mint {
                    asset,
                    nonce,
                    recipient,
                    amount,
                } => {
                    let share = Share::derive(*asset, commitment, 0);
                    let previous = self.assets.get(asset).cloned();
                    let record = self.assets.get_mut(asset).ok_or(AssetError::UnknownAsset)?;
                    let supply = record
                        .supply
                        .checked_add(*amount)
                        .ok_or(AssetError::SupplyOverflow)?;
                    let total_minted = record
                        .total_minted
                        .checked_add(*amount)
                        .ok_or(AssetError::SupplyOverflow)?;
                    journal.assets.push((*asset, previous));
                    record.supply = supply;
                    record.total_minted = total_minted;
                    record.mint_nonce = *nonce;
                    checkpoint(TransitionPoint::AssetRecordUpdated)?;

                    journal.utxos.push((share, utxos.asset(&share).copied()));

                    utxos
                        .insert_asset(
                            share,
                            AssetShare {
                                asset: *asset,
                                amount: *amount,
                                owner: *recipient,
                            },
                        )
                        .map_err(|_| AssetError::ShareAlreadyExists)?;
                    checkpoint(TransitionPoint::AssetOutputCreated)?;
                }

                AssetInstruction::Burn {
                    asset,
                    inputs,
                    amount,
                    output,
                } => {
                    let total = self.validate_inputs(utxos, *asset, inputs)?;
                    let expected_total = amount
                        .checked_add(*output)
                        .ok_or(AssetError::BalanceOverflow)?;
                    if total != expected_total {
                        return Err(AssetError::InvalidAmount);
                    }
                    let previous = self.assets.get(asset).cloned();
                    let record = self.assets.get_mut(asset).ok_or(AssetError::UnknownAsset)?;
                    let supply = record
                        .supply
                        .checked_sub(*amount)
                        .ok_or(AssetError::SupplyOverflow)?;
                    let total_burned = record
                        .total_burned
                        .checked_add(*amount)
                        .ok_or(AssetError::SupplyOverflow)?;
                    journal.assets.push((*asset, previous));
                    record.supply = supply;
                    record.total_burned = total_burned;
                    checkpoint(TransitionPoint::AssetRecordUpdated)?;

                    for input in inputs {
                        journal.utxos.push((*input, utxos.asset(input).copied()));
                        utxos
                            .consume_asset(input)
                            .map_err(|_| AssetError::UnknownObject)?;
                        checkpoint(TransitionPoint::AssetInputConsumed)?;
                    }
                    if !output.is_zero() {
                        let share = Share::derive(*asset, commitment, 0);
                        journal.utxos.push((share, utxos.asset(&share).copied()));
                        utxos
                            .insert_asset(
                                share,
                                AssetShare {
                                    asset: *asset,
                                    amount: *output,
                                    owner: call.signer,
                                },
                            )
                            .map_err(|_| AssetError::ShareAlreadyExists)?;
                        checkpoint(TransitionPoint::AssetOutputCreated)?;
                    }
                }
            }

            Ok(())
        })();

        match result {
            Ok(()) => Ok(journal),
            Err(error) => {
                self.rollback(utxos, journal)
                    .map_err(|_| AssetError::InvalidProgram)?;
                Err(error)
            }
        }
    }

    pub fn apply_account_transfer(
        &mut self,
        utxos: &mut utxo::UtxoSet,
        asset: AssetContract,
        inputs: &[Share],
        outputs: &[AssetOutput],
        commitment: [u8; HASH_SIZE],
    ) -> Result<AssetRollbackJournal, AssetError> {
        self.apply_account_transfer_with_checkpoint(
            utxos,
            asset,
            inputs,
            outputs,
            commitment,
            |_| Ok(()),
        )
    }

    fn apply_account_transfer_with_checkpoint(
        &mut self,
        utxos: &mut utxo::UtxoSet,
        asset: AssetContract,
        inputs: &[Share],
        outputs: &[AssetOutput],
        commitment: [u8; HASH_SIZE],
        mut checkpoint: impl FnMut(TransitionPoint) -> Result<(), AssetError>,
    ) -> Result<AssetRollbackJournal, AssetError> {
        self.metadata(asset).ok_or(AssetError::UnknownAsset)?;

        let input_total = self.validate_inputs(utxos, asset, inputs)?;

        let output_total = outputs_total(outputs)?;

        if input_total != output_total {
            return Err(AssetError::InvalidAmount);
        }

        for index in 0..outputs.len() {
            let index = u32::try_from(index).map_err(|_| AssetError::InvalidProgram)?;

            let id = Share::derive(asset, commitment, index);

            if utxos.asset(&id).is_some() {
                return Err(AssetError::ShareAlreadyExists);
            }
        }

        let mut journal = AssetRollbackJournal::default();

        let result = (|| {
            for input in inputs {
                journal.utxos.push((*input, utxos.asset(input).copied()));
                utxos
                    .consume_asset(input)
                    .map_err(|_| AssetError::UnknownObject)?;
                checkpoint(TransitionPoint::AssetInputConsumed)?;
            }

            for (index, output) in outputs.iter().enumerate() {
                let index = u32::try_from(index).map_err(|_| AssetError::InvalidProgram)?;

                let id = Share::derive(asset, commitment, index);

                journal.utxos.push((id, utxos.asset(&id).copied()));

                utxos
                    .insert_asset(
                        id,
                        AssetShare {
                            asset: asset,
                            amount: output.amount,
                            owner: output.recipient,
                        },
                    )
                    .map_err(|_| AssetError::ShareAlreadyExists)?;
                checkpoint(TransitionPoint::AssetOutputCreated)?;
            }

            Ok(())
        })();

        match result {
            Ok(()) => Ok(journal),
            Err(error) => {
                self.rollback(utxos, journal)
                    .map_err(|_| AssetError::InvalidProgram)?;
                Err(error)
            }
        }
    }
}

impl AssetState {
    pub(crate) fn validate_transition(
        &self,
        utxos: &utxo::UtxoSet,
        call: &AssetIntent,
        _genesis_hash: [u8; 32],
    ) -> Result<(), AssetError> {
        match &call.instruction {
            AssetInstruction::Register {
                name,
                max_supply,
                mint_authority,
                nonce,
                ..
            } => {
                let metadata =
                    Metadata::new(name.clone(), *max_supply, call.signer, *mint_authority)?;

                let id = AssetContract::derive(&metadata, *nonce)?;

                if self.metadata(id).is_some() {
                    return Err(AssetError::AssetAlreadyExists);
                }
            }

            AssetInstruction::Mint {
                asset,
                nonce,
                amount,
                ..
            } => {
                let metadata = self.metadata(*asset).ok_or(AssetError::UnknownAsset)?;
                if metadata.mint_authority == Address::ZERO
                    || metadata.mint_authority != call.signer
                {
                    return Err(AssetError::Unauthorized);
                }
                let expected_nonce = self
                    .mint_nonce(*asset)
                    .ok_or(AssetError::UnknownAsset)?
                    .checked_add(1)
                    .ok_or(AssetError::InvalidMintNonce)?;
                if *nonce != expected_nonce {
                    return Err(AssetError::InvalidMintNonce);
                }

                self.record(*asset)
                    .ok_or(AssetError::UnknownAsset)?
                    .total_minted
                    .checked_add(*amount)
                    .filter(|total| *total <= metadata.max_supply)
                    .ok_or(AssetError::SupplyOverflow)?;
            }

            AssetInstruction::Burn { asset, inputs, .. } => {
                self.metadata(*asset).ok_or(AssetError::UnknownAsset)?;
                self.validate_inputs(utxos, *asset, inputs)?;
            }
        }

        Ok(())
    }

    fn validate_inputs(
        &self,
        utxos: &utxo::UtxoSet,
        asset: AssetContract,
        inputs: &[Share],
    ) -> Result<Unit, AssetError> {
        if inputs.is_empty() {
            return Err(AssetError::InvalidProgram);
        }

        ensure_unique_shares(inputs)?;

        let mut total = Unit::ZERO;

        for input in inputs {
            let share = utxos.asset(input).ok_or(AssetError::UnknownObject)?;

            if share.asset != asset {
                return Err(AssetError::AssetMismatch);
            }

            //
            // No owner comparison here.
            //
            // Authorization belongs to consensus.
            //

            total = total
                .checked_add(share.amount)
                .ok_or(AssetError::BalanceOverflow)?;
        }

        Ok(total)
    }

    pub fn account_transfer_created_state_weight(
        &self,
        utxos: &utxo::UtxoSet,
        asset: AssetContract,
        inputs: &[Share],
        outputs: &[AssetOutput],
    ) -> Result<u64, AssetError> {
        self.metadata(asset).ok_or(AssetError::UnknownAsset)?;

        let input_total = self.validate_inputs(utxos, asset, inputs)?;

        if input_total != outputs_total(outputs)? {
            return Err(AssetError::InvalidAmount);
        }

        outputs.iter().try_fold(0_u64, |weight, output| {
            checked_entry_weight(
                weight,
                HASH_SIZE,
                &AssetShare {
                    asset: asset,
                    amount: output.amount,
                    owner: output.recipient,
                },
            )
        })
    }
}

impl LedgerState {
    pub(crate) fn rollback_state(
        &mut self,
        journal: StateRollbackJournal,
    ) -> Result<(), StateError> {
        if let Some(asset) = journal.asset {
            self.assets.rollback(&mut self.utxos, asset)?;
        }
        if let Some(spend) = journal.spend {
            self.rollback_spend(spend)?;
        }
        Ok(())
    }

    pub(crate) fn rollback_spend(
        &mut self,
        journal: SpendRollbackJournal,
    ) -> Result<(), StateError> {
        let total_mined = self
            .coin
            .total_mined
            .checked_sub(journal.mined)
            .ok_or(StateError::AmountOverflow)?;

        let total_burned = self
            .coin
            .total_burned
            .checked_sub(journal.burned)
            .ok_or(StateError::BurnUnderflow)?;

        let mut created = std::collections::BTreeSet::new();
        for id in &journal.created_coin_ids {
            if !created.insert(*id) || self.utxos.coin(id).is_none() {
                return Err(StateError::InvalidTransaction);
            }
        }
        let mut consumed = std::collections::BTreeSet::new();
        for (id, _) in &journal.consumed_coins {
            if !consumed.insert(*id) || (self.utxos.coin(id).is_some() && !created.contains(id)) {
                return Err(StateError::InvalidTransaction);
            }
        }

        self.coin.total_mined = total_mined;
        self.coin.total_burned = total_burned;
        for id in journal.created_coin_ids {
            self.utxos.consume_coin(&id)?;
        }
        for (id, coin) in journal.consumed_coins {
            self.utxos.insert_coin(id, coin)?;
        }
        Ok(())
    }

    pub(crate) fn record_protocol_burn(
        &mut self,
        burned: Zeno,
        journal: &mut SpendRollbackJournal,
    ) -> Result<(), StateError> {
        let total_burned = self
            .coin
            .total_burned
            .checked_add(burned)
            .ok_or(StateError::BurnOverflow)?;

        let journal_burned = journal
            .burned
            .checked_add(burned)
            .ok_or(StateError::BurnOverflow)?;

        self.coin.total_burned = total_burned;
        journal.burned = journal_burned;
        Ok(())
    }

    fn finish_spend_transition(
        &mut self,
        journal: SpendRollbackJournal,
        result: Result<(), StateError>,
    ) -> Result<SpendRollbackJournal, StateError> {
        match result {
            Ok(()) => Ok(journal),

            Err(error) => {
                self.rollback_spend(journal)?;
                Err(error)
            }
        }
    }
}

impl AssetState {
    pub fn rollback(
        &mut self,
        utxos: &mut utxo::UtxoSet,
        journal: AssetRollbackJournal,
    ) -> Result<(), StateError> {
        let mut seen = std::collections::BTreeSet::new();
        if journal.utxos.iter().any(|(id, _)| !seen.insert(*id)) {
            return Err(StateError::Asset(AssetError::InvalidProgram));
        }
        restore_map(&mut self.assets, journal.assets);

        for (id, previous) in journal.utxos.into_iter().rev() {
            if utxos.asset(&id).is_some() {
                utxos.consume_asset(&id)?;
            }

            if let Some(previous) = previous {
                utxos.insert_asset(id, previous)?;
            }
        }
        Ok(())
    }
}

//
// Helpers
//

fn output_index(index: usize) -> Result<u32, StateError> {
    u32::try_from(index).map_err(|_| StateError::OutputIndexOverflow)
}

fn coin_output_id(
    commitment: SpendIntentCommitment,
    index: usize,
) -> Result<CoinShare, StateError> {
    Ok(CoinShare::from_output(
        commitment.as_bytes(),
        output_index(index)?,
    ))
}

fn outputs_total(outputs: &[AssetOutput]) -> Result<Unit, AssetError> {
    if outputs.is_empty() {
        return Err(AssetError::InvalidProgram);
    }

    let mut total = Unit::ZERO;

    for output in outputs {
        if output.amount.is_zero() {
            return Err(AssetError::InvalidAmount);
        }

        total = total
            .checked_add(output.amount)
            .ok_or(AssetError::BalanceOverflow)?;
    }

    Ok(total)
}

fn ensure_unique_shares(inputs: &[Share]) -> Result<(), AssetError> {
    let mut sorted = inputs.to_vec();

    sorted.sort_unstable();

    if sorted.windows(2).any(|pair| pair[0] == pair[1]) {
        return Err(AssetError::InvalidProgram);
    }

    Ok(())
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

fn restore_map<K: Ord, V>(map: &mut BTreeMap<K, V>, entries: Vec<(K, Option<V>)>) {
    for (key, previous) in entries.into_iter().rev() {
        match previous {
            Some(value) => {
                map.insert(key, value);
            }

            None => {
                map.remove(&key);
            }
        }
    }
}

#[cfg(test)]
mod phase2_failure_injection_tests {
    use super::*;
    use crate::{
        monetary::coin::CoinOutput,
        transaction::{AssetInstruction, SpendCharges},
    };

    fn address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
    }

    fn asset_fixture() -> (AssetState, utxo::UtxoSet, AssetIntent, AssetContract, Share) {
        let owner = address(1);
        let call = AssetIntent::new(
            AssetInstruction::Register {
                name: "Atomicity".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: owner,
                nonce: 0,
            },
            owner,
        );
        let asset = call.asset().unwrap();
        let share = Share::derive(asset, call.commitment([7; 32]).unwrap(), 0);
        let mut state = AssetState::default();
        let mut utxos = utxo::UtxoSet::default();
        state.apply(&mut utxos, &call, [7; 32]).unwrap();
        (state, utxos, call, asset, share)
    }

    #[test]
    fn coin_spend_failure_after_each_mutation_restores_state_and_retry_root() {
        let owner = address(1);
        let input = CoinShare::from_bytes([3; crypto::HASH16_SIZE]);
        let mut original = LedgerState::default();
        original.coin.total_mined = Zeno::from_zeno(100);
        original
            .utxos
            .insert_coin(
                input,
                CoinUtxo {
                    amount: Zeno::from_zeno(100),
                    owner,
                },
            )
            .unwrap();
        let intent = SpendIntent::coin_with_charges(
            owner,
            vec![input],
            vec![CoinOutput::new(address(2), Zeno::from_zeno(70))],
            SpendCharges::new(Zeno::from_zeno(10)),
        )
        .unwrap();
        let commitment = SpendIntentCommitment::from_bytes([4; HASH_SIZE]);
        let mut expected = original.clone();
        expected
            .apply_onchain_spend_with_commitment(&intent, commitment, address(9))
            .unwrap();
        let expected_bytes = borsh::to_vec(&expected).unwrap();

        for point in [
            TransitionPoint::CoinInputConsumed,
            TransitionPoint::CoinOutputCreated,
            TransitionPoint::MinerFeeCreated,
            TransitionPoint::ProtocolBurnRecorded,
        ] {
            let mut state = original.clone();
            assert!(matches!(
                state.apply_onchain_spend_with_checkpoint(
                    &intent,
                    commitment,
                    address(9),
                    |seen| if seen == point {
                        Err(StateError::InvalidTransaction)
                    } else {
                        Ok(())
                    },
                ),
                Err(StateError::InvalidTransaction)
            ));
            assert_eq!(state, original, "failure at {point:?}");
            state
                .apply_onchain_spend_with_commitment(&intent, commitment, address(9))
                .unwrap();
            assert_eq!(borsh::to_vec(&state).unwrap(), expected_bytes);
        }
    }

    #[test]
    fn asset_calls_failure_after_each_mutation_restores_state_and_retry_bytes() {
        let owner = address(1);
        let (_, _, register, asset, input) = asset_fixture();
        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: address(2),
                amount: Unit::from_units(5),
            },
            owner,
        );
        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![input],
                amount: Unit::from_units(4),
                output: Unit::from_units(6),
            },
            owner,
        );
        for (call, points) in [
            (
                register,
                vec![
                    TransitionPoint::AssetRecordUpdated,
                    TransitionPoint::AssetOutputCreated,
                ],
            ),
            (
                mint,
                vec![
                    TransitionPoint::AssetRecordUpdated,
                    TransitionPoint::AssetOutputCreated,
                ],
            ),
            (
                burn,
                vec![
                    TransitionPoint::AssetRecordUpdated,
                    TransitionPoint::AssetInputConsumed,
                    TransitionPoint::AssetOutputCreated,
                ],
            ),
        ] {
            let (start_state, start_utxos) =
                if matches!(&call.instruction, AssetInstruction::Register { .. }) {
                    (AssetState::default(), utxo::UtxoSet::default())
                } else {
                    let (state, utxos, _, _, _) = asset_fixture();
                    (state, utxos)
                };
            let mut expected_state = start_state.clone();
            let mut expected_utxos = start_utxos.clone();
            expected_state
                .apply(&mut expected_utxos, &call, [7; 32])
                .unwrap();
            for point in points {
                let mut state = start_state.clone();
                let mut utxos = start_utxos.clone();
                assert_eq!(
                    state.apply_with_checkpoint(&mut utxos, &call, [7; 32], |seen| {
                        if seen == point {
                            Err(AssetError::InvalidProgram)
                        } else {
                            Ok(())
                        }
                    }),
                    Err(AssetError::InvalidProgram),
                    "failure at {point:?}"
                );
                assert_eq!(state, start_state);
                assert_eq!(utxos, start_utxos);
                state.apply(&mut utxos, &call, [7; 32]).unwrap();
                assert_eq!(
                    (state, utxos),
                    (expected_state.clone(), expected_utxos.clone())
                );
            }
        }
    }

    #[test]
    fn asset_transfer_failure_after_input_or_output_restores_state() {
        let (original_state, original_utxos, _, asset, input) = asset_fixture();
        let outputs = [AssetOutput::new(address(2), Unit::from_units(10))];
        let mut expected_state = original_state.clone();
        let mut expected_utxos = original_utxos.clone();
        expected_state
            .apply_account_transfer(
                &mut expected_utxos,
                asset,
                &[input],
                &outputs,
                [8; HASH_SIZE],
            )
            .unwrap();
        for point in [
            TransitionPoint::AssetInputConsumed,
            TransitionPoint::AssetOutputCreated,
        ] {
            let mut state = original_state.clone();
            let mut utxos = original_utxos.clone();
            assert_eq!(
                state.apply_account_transfer_with_checkpoint(
                    &mut utxos,
                    asset,
                    &[input],
                    &outputs,
                    [8; HASH_SIZE],
                    |seen| if seen == point {
                        Err(AssetError::InvalidProgram)
                    } else {
                        Ok(())
                    },
                ),
                Err(AssetError::InvalidProgram)
            );
            assert_eq!(
                (state.clone(), utxos.clone()),
                (original_state.clone(), original_utxos.clone())
            );
            state
                .apply_account_transfer(&mut utxos, asset, &[input], &outputs, [8; HASH_SIZE])
                .unwrap();
            assert_eq!(
                (state, utxos),
                (expected_state.clone(), expected_utxos.clone())
            );
        }
    }

    #[test]
    fn burn_accounting_and_invalid_rollback_journal_fail_without_partial_mutation() {
        let mut state = LedgerState::default();
        let mut journal = SpendRollbackJournal {
            burned: Zeno::from_zeno(u64::MAX),
            ..SpendRollbackJournal::default()
        };
        assert!(matches!(
            state.record_protocol_burn(Zeno::ONE, &mut journal),
            Err(StateError::BurnOverflow)
        ));
        assert_eq!(state.coin.total_burned, Zeno::ZERO);
        assert_eq!(journal.burned, Zeno::from_zeno(u64::MAX));

        state.coin.total_mined = Zeno::from_zeno(100);
        state.coin.total_burned = Zeno::from_zeno(5);
        let before = state.clone();
        let corrupt = SpendRollbackJournal {
            created_coin_ids: vec![CoinShare::from_bytes([9; crypto::HASH16_SIZE])],
            mined: Zeno::from_zeno(10),
            burned: Zeno::ONE,
            ..SpendRollbackJournal::default()
        };
        assert!(matches!(
            state.rollback_spend(corrupt),
            Err(StateError::InvalidTransaction)
        ));
        assert_eq!(state, before);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
    }

    fn register(authority: Address) -> AssetIntent {
        AssetIntent::new(
            AssetInstruction::Register {
                name: "Nonce Asset".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: authority,
                nonce: 0,
            },
            address(1),
        )
    }

    #[test]
    fn mint_nonce_is_sequential_and_rollback_restores_it() {
        let authority = address(2);
        let register = register(authority);
        let asset = register.asset().unwrap();
        let mut state = AssetState::default();
        let mut utxos = utxo::UtxoSet::default();

        state.apply(&mut utxos, &register, [3; 32]).unwrap();
        assert_eq!(state.mint_nonce(asset), Some(0));

        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: address(4),
                amount: Unit::from_units(5),
            },
            authority,
        );
        let journal = state.apply(&mut utxos, &mint, [3; 32]).unwrap();
        assert_eq!(state.mint_nonce(asset), Some(1));
        assert_eq!(state.supply(asset), Unit::from_units(15));
        assert_eq!(state.total_minted(asset), Some(Unit::from_units(15)));

        assert_eq!(
            state.apply(&mut utxos, &mint, [3; 32]),
            Err(AssetError::InvalidMintNonce)
        );

        state.rollback(&mut utxos, journal).unwrap();
        assert_eq!(state.mint_nonce(asset), Some(0));
        assert_eq!(state.supply(asset), Unit::from_units(10));
        assert_eq!(state.total_minted(asset), Some(Unit::from_units(10)));
    }

    #[test]
    fn mint_nonce_does_not_replace_authority_check() {
        let authority = address(2);
        let register = register(authority);
        let asset = register.asset().unwrap();
        let mut state = AssetState::default();
        let mut utxos = utxo::UtxoSet::default();
        state.apply(&mut utxos, &register, [3; 32]).unwrap();

        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: address(4),
                amount: Unit::from_units(5),
            },
            address(9),
        );
        assert_eq!(
            state.apply(&mut utxos, &mint, [3; 32]),
            Err(AssetError::Unauthorized)
        );
    }

    #[test]
    fn burned_supply_cannot_be_minted_past_cumulative_cap() {
        let authority = address(2);
        let register = register(authority);
        let asset = register.asset().unwrap();
        let genesis_hash = [3; 32];
        let initial_share = Share::derive(asset, register.commitment(genesis_hash).unwrap(), 0);
        let mut state = AssetState::default();
        let mut utxos = utxo::UtxoSet::default();
        state.apply(&mut utxos, &register, genesis_hash).unwrap();

        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share],
                amount: Unit::from_units(10),
                output: Unit::ZERO,
            },
            address(1),
        );
        state.apply(&mut utxos, &burn, genesis_hash).unwrap();
        assert_eq!(state.supply(asset), Unit::ZERO);
        assert_eq!(state.total_minted(asset), Some(Unit::from_units(10)));

        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: address(4),
                amount: Unit::from_units(95),
            },
            authority,
        );
        assert_eq!(
            state.apply(&mut utxos, &mint, genesis_hash),
            Err(AssetError::SupplyOverflow)
        );
    }
}

#[cfg(test)]
mod invariant_tests {
    use super::*;
    use crate::monetary::coin::CoinOutput;
    use crypto::HASH16_SIZE;

    const GENESIS_HASH: [u8; 32] = [0x33; 32];

    fn test_address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
    }

    fn register_intent(
        creator: Address,
        authority: Address,
        name: &str,
        nonce: u64,
        max_supply: u128,
        initial_mint: u128,
    ) -> AssetIntent {
        AssetIntent::new(
            AssetInstruction::Register {
                name: name.into(),
                max_supply: Unit::from_units(max_supply),
                initial_mint: Unit::from_units(initial_mint),
                mint_authority: authority,
                nonce,
            },
            creator,
        )
    }

    fn setup_asset(
        name: &str,
        nonce: u64,
        max_supply: u128,
        initial_mint: u128,
        creator_byte: u8,
        authority_byte: u8,
    ) -> (AssetState, utxo::UtxoSet, AssetIntent, AssetContract, Share) {
        let creator = test_address(creator_byte);
        let authority = test_address(authority_byte);
        let register = register_intent(creator, authority, name, nonce, max_supply, initial_mint);
        let asset = register.asset().unwrap();
        let initial_share = Share::derive(asset, register.commitment(GENESIS_HASH).unwrap(), 0);

        let mut state = AssetState::default();
        let mut utxos = utxo::UtxoSet::default();

        state.apply(&mut utxos, &register, GENESIS_HASH).unwrap();

        (state, utxos, register, asset, initial_share)
    }

    #[test]
    fn burn_with_change_conserves_value_and_tracks_only_burned_amount() {
        let (mut state, mut utxos, register, asset, initial_share) =
            setup_asset("Burn Change", 0, 100, 10, 1, 2);

        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share],
                amount: Unit::from_units(4),
                output: Unit::from_units(6),
            },
            register.signer,
        );

        let change_share = Share::derive(asset, burn.commitment(GENESIS_HASH).unwrap(), 0);

        state.apply(&mut utxos, &burn, GENESIS_HASH).unwrap();

        assert_eq!(state.supply(asset), Unit::from_units(6));
        assert_eq!(state.total_minted(asset), Some(Unit::from_units(10)));

        let record = state.record(asset).unwrap();
        assert_eq!(record.total_burned, Unit::from_units(4));

        assert!(utxos.asset(&initial_share).is_none());

        let change = utxos.asset(&change_share).expect("burn change share");
        assert_eq!(change.asset, asset);
        assert_eq!(change.amount, Unit::from_units(6));
        assert_eq!(change.owner, register.signer);
    }

    #[test]
    fn burn_rejects_amount_plus_change_that_does_not_equal_inputs_without_mutation() {
        let (mut state, mut utxos, register, asset, initial_share) =
            setup_asset("Bad Burn Sum", 0, 100, 10, 1, 2);

        let before_state = state.clone();
        let before_utxos = utxos.clone();

        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share],
                amount: Unit::from_units(4),
                output: Unit::from_units(5),
            },
            register.signer,
        );

        assert_eq!(
            state.apply(&mut utxos, &burn, GENESIS_HASH),
            Err(AssetError::InvalidAmount)
        );

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);
    }

    #[test]
    fn burn_rollback_restores_asset_record_and_exact_utxo_set() {
        let (mut state, mut utxos, register, asset, initial_share) =
            setup_asset("Burn Rollback", 0, 100, 10, 1, 2);

        let before_state = state.clone();
        let before_utxos = utxos.clone();

        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share],
                amount: Unit::from_units(4),
                output: Unit::from_units(6),
            },
            register.signer,
        );

        let journal = state.apply(&mut utxos, &burn, GENESIS_HASH).unwrap();
        assert_ne!(state, before_state);
        assert_ne!(utxos, before_utxos);

        state.rollback(&mut utxos, journal).unwrap();

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);
    }

    #[test]
    fn duplicate_asset_inputs_are_rejected_without_mutation() {
        let (mut state, mut utxos, register, asset, initial_share) =
            setup_asset("Duplicate Input", 0, 100, 10, 1, 2);

        let before_state = state.clone();
        let before_utxos = utxos.clone();

        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share, initial_share],
                amount: Unit::from_units(10),
                output: Unit::ZERO,
            },
            register.signer,
        );

        assert_eq!(
            state.apply(&mut utxos, &burn, GENESIS_HASH),
            Err(AssetError::InvalidProgram)
        );

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);
    }

    #[test]
    fn share_from_another_asset_is_rejected_without_mutation() {
        let (mut state, mut utxos, register_a, asset_a, share_a) =
            setup_asset("Asset A", 0, 100, 10, 1, 2);

        let register_b = register_intent(test_address(3), test_address(4), "Asset B", 1, 100, 10);
        let asset_b = register_b.asset().unwrap();

        state.apply(&mut utxos, &register_b, GENESIS_HASH).unwrap();

        let before_state = state.clone();
        let before_utxos = utxos.clone();

        let wrong_asset_burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset: asset_b,
                inputs: vec![share_a],
                amount: Unit::from_units(10),
                output: Unit::ZERO,
            },
            register_a.signer,
        );

        assert_eq!(
            state.apply(&mut utxos, &wrong_asset_burn, GENESIS_HASH),
            Err(AssetError::AssetMismatch)
        );

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);

        // Ensure the original asset remains intact as well.
        assert_eq!(state.supply(asset_a), Unit::from_units(10));
    }

    #[test]
    fn asset_transfer_requires_exact_input_output_conservation() {
        let (mut state, mut utxos, _register, asset, initial_share) =
            setup_asset("Transfer Conservation", 0, 100, 10, 1, 2);

        let before_state = state.clone();
        let before_utxos = utxos.clone();

        let outputs = vec![AssetOutput::new(test_address(7), Unit::from_units(9))];

        assert_eq!(
            state.apply_account_transfer(
                &mut utxos,
                asset,
                &[initial_share],
                &outputs,
                [0x55; HASH_SIZE],
            ),
            Err(AssetError::InvalidAmount)
        );

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);
    }

    #[test]
    fn successful_asset_transfer_and_rollback_restore_exact_state() {
        let (mut state, mut utxos, _register, asset, initial_share) =
            setup_asset("Transfer Rollback", 0, 100, 10, 1, 2);

        let before_state = state.clone();
        let before_utxos = utxos.clone();
        let commitment = [0x66; HASH_SIZE];

        let outputs = vec![
            AssetOutput::new(test_address(7), Unit::from_units(4)),
            AssetOutput::new(test_address(8), Unit::from_units(6)),
        ];

        let output_a = Share::derive(asset, commitment, 0);
        let output_b = Share::derive(asset, commitment, 1);

        let journal = state
            .apply_account_transfer(&mut utxos, asset, &[initial_share], &outputs, commitment)
            .unwrap();

        assert!(utxos.asset(&initial_share).is_none());
        assert_eq!(
            utxos.asset(&output_a).map(|share| share.amount),
            Some(Unit::from_units(4))
        );
        assert_eq!(
            utxos.asset(&output_b).map(|share| share.amount),
            Some(Unit::from_units(6))
        );

        state.rollback(&mut utxos, journal).unwrap();

        assert_eq!(state, before_state);
        assert_eq!(utxos, before_utxos);
    }

    #[test]
    fn CoinShare_overspend_is_rejected_without_consuming_inputs() {
        let sender = test_address(1);
        let recipient = test_address(2);
        let input = CoinShare::from_bytes([0x11; HASH16_SIZE]);

        let mut ledger = LedgerState::default();
        ledger
            .utxos
            .insert_coin(
                input,
                CoinUtxo {
                    amount: Zeno::from_zeno(10),
                    owner: sender,
                },
            )
            .unwrap();

        let before = ledger.clone();

        let intent = SpendIntent::coin(
            sender,
            vec![input],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(11))],
        )
        .unwrap();

        let result = ledger.apply_onchain_spend_with_commitment(
            &intent,
            SpendIntentCommitment::from_bytes([0x77; HASH_SIZE]),
            test_address(9),
        );

        assert!(matches!(result, Err(StateError::InvalidTransaction)));
        assert_eq!(ledger, before);
    }

    #[test]
    fn CoinShare_input_minus_outputs_is_exact_protocol_burn_and_rollback_is_exact() {
        let sender = test_address(1);
        let recipient = test_address(2);
        let input = CoinShare::from_bytes([0x22; HASH16_SIZE]);
        let commitment = SpendIntentCommitment::from_bytes([0x88; HASH_SIZE]);

        let mut ledger = LedgerState::default();
        ledger
            .utxos
            .insert_coin(
                input,
                CoinUtxo {
                    amount: Zeno::from_zeno(10),
                    owner: sender,
                },
            )
            .unwrap();

        let before = ledger.clone();

        let intent = SpendIntent::coin(
            sender,
            vec![input],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(7))],
        )
        .unwrap();

        let journal = ledger
            .apply_onchain_spend_with_commitment(&intent, commitment, test_address(9))
            .unwrap();

        assert_eq!(journal.burned, Zeno::from_zeno(3));
        assert!(ledger.utxos.coin(&input).is_none());

        let output = CoinShare::from_output(commitment.as_bytes(), 0);
        let created = ledger
            .utxos
            .coin(&output)
            .expect("created CoinShare output");
        assert_eq!(created.amount, Zeno::from_zeno(7));
        assert_eq!(created.owner, recipient);

        ledger.rollback_spend(journal).unwrap();

        assert_eq!(ledger, before);
    }
}

#[cfg(test)]
mod global_invariant_tests {
    use super::*;

    const GENESIS_HASH: [u8; HASH_SIZE] = [0x93; HASH_SIZE];

    fn address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
    }

    fn ledger_bytes(ledger: &LedgerState) -> Vec<u8> {
        borsh::to_vec(ledger).expect("ledger must have canonical Borsh encoding")
    }

    fn register_intent(
        creator: Address,
        authority: Address,
        name: &str,
        nonce: u64,
        max_supply: u128,
        initial_mint: u128,
    ) -> AssetIntent {
        AssetIntent::new(
            AssetInstruction::Register {
                name: name.into(),
                max_supply: Unit::from_units(max_supply),
                initial_mint: Unit::from_units(initial_mint),
                mint_authority: authority,
                nonce,
            },
            creator,
        )
    }

    fn assert_global_asset_invariants(state: &AssetState, utxos: &utxo::UtxoSet) {
        // Every registered asset must satisfy:
        //
        //     live supply = cumulative minted - cumulative burned
        //
        for (asset, _) in state.metadata_entries() {
            let record = state
                .record(asset)
                .expect("metadata must have an asset record");

            let expected_supply = record
                .total_minted
                .checked_sub(record.total_burned)
                .expect("total_burned must never exceed total_minted");

            assert_eq!(
                record.supply, expected_supply,
                "asset supply accounting diverged for {asset}"
            );

            let share_total = utxos
                .assets()
                .filter(|(_, share)| share.asset == asset)
                .try_fold(Unit::ZERO, |total, (_, share)| {
                    total.checked_add(share.amount)
                })
                .expect("asset UTXO sum overflow");

            assert_eq!(
                share_total, record.supply,
                "live share UTXOs do not equal recorded supply for {asset}"
            );
        }

        // Every share UTXO must belong to a registered asset.
        for (_, share) in utxos.assets() {
            assert!(
                state.record(share.asset).is_some(),
                "orphan share references an unregistered asset"
            );
        }
    }

    #[test]
    fn register_mint_transfer_burn_preserves_global_asset_invariants() {
        let creator = address(1);
        let authority = address(2);
        let mut ledger = LedgerState::default();

        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        // 1. Register: supply = 10, total_minted = 10, total_burned = 0.
        let register = register_intent(creator, authority, "Global Invariant", 0, 100, 10);
        let asset = register.asset().unwrap();
        let initial_share = Share::derive(asset, register.commitment(GENESIS_HASH).unwrap(), 0);

        ledger
            .assets
            .apply(&mut ledger.utxos, &register, GENESIS_HASH)
            .unwrap();

        assert_eq!(ledger.assets.supply(asset), Unit::from_units(10));
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        // 2. Mint 5: supply = 15, total_minted = 15.
        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: creator,
                amount: Unit::from_units(5),
            },
            authority,
        );
        let minted_share = Share::derive(asset, mint.commitment(GENESIS_HASH).unwrap(), 0);

        ledger
            .assets
            .apply(&mut ledger.utxos, &mint, GENESIS_HASH)
            .unwrap();

        assert_eq!(ledger.assets.supply(asset), Unit::from_units(15));
        assert_eq!(
            ledger.assets.total_minted(asset),
            Some(Unit::from_units(15))
        );
        assert_eq!(
            ledger.utxos.asset(&minted_share).map(|share| share.amount),
            Some(Unit::from_units(5))
        );
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        // 3. Transfer the original 10 into 4 + 6. Supply must not change.
        let transfer_commitment = [0x44; HASH_SIZE];
        let outputs = vec![
            AssetOutput::new(creator, Unit::from_units(4)),
            AssetOutput::new(creator, Unit::from_units(6)),
        ];

        let transferred_a = Share::derive(asset, transfer_commitment, 0);
        let transferred_b = Share::derive(asset, transfer_commitment, 1);

        ledger
            .assets
            .apply_account_transfer(
                &mut ledger.utxos,
                asset,
                &[initial_share],
                &outputs,
                transfer_commitment,
            )
            .unwrap();

        assert_eq!(ledger.assets.supply(asset), Unit::from_units(15));
        assert_eq!(
            ledger.utxos.asset(&transferred_a).map(|share| share.amount),
            Some(Unit::from_units(4))
        );
        assert_eq!(
            ledger.utxos.asset(&transferred_b).map(|share| share.amount),
            Some(Unit::from_units(6))
        );
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        // 4. Burn 1 from the 4-unit share and return 3 as change.
        //    Final supply = 14, total_minted = 15, total_burned = 1.
        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![transferred_a],
                amount: Unit::from_units(1),
                output: Unit::from_units(3),
            },
            creator,
        );
        let burn_change = Share::derive(asset, burn.commitment(GENESIS_HASH).unwrap(), 0);

        ledger
            .assets
            .apply(&mut ledger.utxos, &burn, GENESIS_HASH)
            .unwrap();

        let record = ledger.assets.record(asset).unwrap();
        assert_eq!(record.supply, Unit::from_units(14));
        assert_eq!(record.total_minted, Unit::from_units(15));
        assert_eq!(record.total_burned, Unit::from_units(1));
        assert_eq!(
            ledger.utxos.asset(&burn_change).map(|share| share.amount),
            Some(Unit::from_units(3))
        );

        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);
    }

    #[test]
    fn multi_step_asset_rollback_restores_entire_ledger_byte_for_byte() {
        let creator = address(11);
        let authority = address(12);
        let mut ledger = LedgerState::default();
        let initial_bytes = ledger_bytes(&ledger);

        let register = register_intent(creator, authority, "Rollback Chain", 7, 1_000, 100);
        let asset = register.asset().unwrap();
        let initial_share = Share::derive(asset, register.commitment(GENESIS_HASH).unwrap(), 0);

        let register_journal = ledger
            .assets
            .apply(&mut ledger.utxos, &register, GENESIS_HASH)
            .unwrap();

        let mint = AssetIntent::new(
            AssetInstruction::Mint {
                asset,
                nonce: 1,
                recipient: creator,
                amount: Unit::from_units(50),
            },
            authority,
        );

        let mint_journal = ledger
            .assets
            .apply(&mut ledger.utxos, &mint, GENESIS_HASH)
            .unwrap();

        let transfer_commitment = [0x55; HASH_SIZE];
        let transfer_journal = ledger
            .assets
            .apply_account_transfer(
                &mut ledger.utxos,
                asset,
                &[initial_share],
                &[
                    AssetOutput::new(creator, Unit::from_units(40)),
                    AssetOutput::new(creator, Unit::from_units(60)),
                ],
                transfer_commitment,
            )
            .unwrap();

        let burn_input = Share::derive(asset, transfer_commitment, 0);
        let burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![burn_input],
                amount: Unit::from_units(10),
                output: Unit::from_units(30),
            },
            creator,
        );

        let burn_journal = ledger
            .assets
            .apply(&mut ledger.utxos, &burn, GENESIS_HASH)
            .unwrap();

        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);
        assert_ne!(ledger_bytes(&ledger), initial_bytes);

        // Roll back in exact reverse application order.
        ledger
            .assets
            .rollback(&mut ledger.utxos, burn_journal)
            .unwrap();
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        ledger
            .assets
            .rollback(&mut ledger.utxos, transfer_journal)
            .unwrap();
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        ledger
            .assets
            .rollback(&mut ledger.utxos, mint_journal)
            .unwrap();
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        ledger
            .assets
            .rollback(&mut ledger.utxos, register_journal)
            .unwrap();

        assert!(ledger.assets.is_empty());
        assert!(ledger.utxos.is_empty());
        assert_eq!(ledger_bytes(&ledger), initial_bytes);
    }

    #[test]
    fn failed_transition_does_not_change_full_ledger_encoding() {
        let creator = address(21);
        let authority = address(22);
        let mut ledger = LedgerState::default();

        let register = register_intent(creator, authority, "Atomic Failure", 0, 100, 10);
        let asset = register.asset().unwrap();
        let initial_share = Share::derive(asset, register.commitment(GENESIS_HASH).unwrap(), 0);

        ledger
            .assets
            .apply(&mut ledger.utxos, &register, GENESIS_HASH)
            .unwrap();

        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);

        let before = ledger_bytes(&ledger);

        // Input is 10, but burned + change claims only 9.
        let invalid_burn = AssetIntent::new(
            AssetInstruction::Burn {
                asset,
                inputs: vec![initial_share],
                amount: Unit::from_units(4),
                output: Unit::from_units(5),
            },
            creator,
        );

        assert_eq!(
            ledger
                .assets
                .apply(&mut ledger.utxos, &invalid_burn, GENESIS_HASH),
            Err(AssetError::InvalidAmount)
        );

        assert_eq!(ledger_bytes(&ledger), before);
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);
    }

    #[test]
    fn two_assets_keep_independent_supply_and_utxo_accounting() {
        let mut ledger = LedgerState::default();

        let a = register_intent(address(31), address(32), "Asset One", 1, 100, 10);
        let b = register_intent(address(41), address(42), "Asset Two", 2, 200, 20);

        let asset_a = a.asset().unwrap();
        let asset_b = b.asset().unwrap();

        ledger
            .assets
            .apply(&mut ledger.utxos, &a, GENESIS_HASH)
            .unwrap();
        ledger
            .assets
            .apply(&mut ledger.utxos, &b, GENESIS_HASH)
            .unwrap();

        let mint_a = AssetIntent::new(
            AssetInstruction::Mint {
                asset: asset_a,
                nonce: 1,
                recipient: address(31),
                amount: Unit::from_units(5),
            },
            address(32),
        );

        ledger
            .assets
            .apply(&mut ledger.utxos, &mint_a, GENESIS_HASH)
            .unwrap();

        assert_eq!(ledger.assets.supply(asset_a), Unit::from_units(15));
        assert_eq!(ledger.assets.supply(asset_b), Unit::from_units(20));

        let sum_a = ledger
            .utxos
            .assets()
            .filter(|(_, share)| share.asset == asset_a)
            .fold(Unit::ZERO, |total, (_, share)| {
                total.checked_add(share.amount).unwrap()
            });

        let sum_b = ledger
            .utxos
            .assets()
            .filter(|(_, share)| share.asset == asset_b)
            .fold(Unit::ZERO, |total, (_, share)| {
                total.checked_add(share.amount).unwrap()
            });

        assert_eq!(sum_a, Unit::from_units(15));
        assert_eq!(sum_b, Unit::from_units(20));
        assert_global_asset_invariants(&ledger.assets, &ledger.utxos);
    }
}
