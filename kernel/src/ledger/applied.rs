use crate::{
    consensus::{AuthorizationValidated, ValidatedTransaction},
    ledger::{CoinUtxo, LedgerState, SpendRollbackJournal, StateError, StateRollbackJournal},
    monetary::coin::{CoinShare, Zeno},
    transaction::{SpendIntent, SpendIntentCommitment},
};
use crypto::Address;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TransitionPoint {
    CoinInputConsumed,
    CoinOutputCreated,
    MinerFeeCreated,
    ProtocolBurnRecorded,
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
            ValidatedTransaction::Program(prepared) => self
                .apply_program_transaction(
                    prepared.transaction.clone(),
                    block_miner,
                    chain,
                    prepared.height,
                )
                .map_err(|_| StateError::InvalidTransaction),
            ValidatedTransaction::CoinSpend(transaction) => {
                let spend = self.apply_validated_onchain_spend(&transaction.spend, block_miner)?;
                Ok(StateRollbackJournal {
                    spend: Some(spend),
                    extension: None,
                })
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

impl LedgerState {
    pub(crate) fn rollback_state(
        &mut self,
        journal: StateRollbackJournal,
    ) -> Result<(), StateError> {
        let mut staged = self.clone();
        if let Some(extension) = journal.extension {
            staged.extensions.assets.rollback(extension);
        }
        if let Some(spend) = journal.spend {
            staged.rollback_spend(spend)?;
        }
        *self = staged;
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

impl LedgerState {
    /// Atomic execution of an authenticated Program transaction.
    /// Validation and both state transitions run on a clone, committed only on success.
    pub fn apply_program_transaction(
        &mut self,
        transaction: crate::transaction::AuthorizedProgramTransaction,
        miner: Address,
        chain: crate::common::ChainContext,
        height: u64,
    ) -> Result<StateRollbackJournal, super::LedgerError> {
        let prepared =
            crate::program::prepare_program_transaction(transaction, chain, height, self)?;
        let tx = prepared.transaction;
        let mut staged = self.clone();
        let commitment = tx
            .payment
            .semantic_commitment(chain)
            .map_err(crate::consensus::TransactionConsensusError::Intent)?;
        let spend = staged.apply_onchain_spend_with_commitment(&tx.payment, commitment, miner)?;
        let journal = match extension::script::execute::decode_program(&tx.call)
            .map_err(|_| StateError::InvalidTransaction)?
        {
            extension::script::execute::DecodedProgramCall::XpqTransfer => None,
            extension::script::execute::DecodedProgramCall::Asset(call) => {
                let bytes = crypto::canonical_bytes(&(
                    chain.genesis_hash,
                    tx.signer,
                    &tx.call,
                    &tx.payment,
                ))?;
                let journal = staged
                    .extensions
                    .assets
                    .apply(
                        &call,
                        extension::asset_program::state::ExecutionContext {
                            signer: tx.signer,
                            commitment: crypto::domain(crypto::HashDomain::AssetIntent, &bytes)
                                .into_bytes(),
                        },
                    )
                    .map_err(|_| StateError::InvalidTransaction)?;
                Some(journal)
            }
        };
        staged.validate_supply_invariants()?;
        *self = staged;
        Ok(StateRollbackJournal {
            spend: Some(spend),
            extension: journal,
        })
    }
}

#[cfg(test)]
mod coin_atomicity_tests {
    use super::*;
    use crate::{monetary::coin::CoinOutput, transaction::SpendCharges};
    use crypto::HASH_SIZE;
    fn address(byte: u8) -> Address {
        Address([byte; crypto::ADDRESS_SIZE])
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
