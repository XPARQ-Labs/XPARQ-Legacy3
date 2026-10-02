use crate::{
    consensus::ValidatedTransaction,
    ledger::{CoinUtxo, LedgerState, SpendRollbackJournal, StateError, StateRollbackJournal},
    monetary::coin::{CoinShare, Zeno},
    transaction::{CoinTransition, CoinTransitionCommitment},
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
        }
    }
}

//
// XPQ Program execution through a restricted coin host
//

impl LedgerState {
    fn execute_coin_program(
        &mut self,
        intent: &CoinTransition,
        commitment: CoinTransitionCommitment,
        block_miner: Address,
    ) -> Result<SpendRollbackJournal, StateError> {
        self.execute_coin_program_with_checkpoint(intent, commitment, block_miner, |_| Ok(()))
    }

    fn execute_coin_program_with_checkpoint(
        &mut self,
        intent: &CoinTransition,
        commitment: CoinTransitionCommitment,
        block_miner: Address,
        mut checkpoint: impl FnMut(TransitionPoint) -> Result<(), StateError>,
    ) -> Result<SpendRollbackJournal, StateError> {
        let mut journal = SpendRollbackJournal::default();

        let result = (|| {
            let (inputs, outputs) = intent.coin_parts().ok_or(StateError::InvalidTransaction)?;

            let outputs: Vec<_> = outputs
                .iter()
                .map(|output| (output.output, output.amount.as_zeno()))
                .collect();
            let mut host = KernelCoinHost {
                state: self,
                journal: &mut journal,
                commitment,
                output_count: outputs.len(),
                checkpoint: &mut checkpoint,
            };
            extension::coin_program::execute_transfer(
                &mut host,
                inputs,
                &outputs,
                block_miner,
                intent.charges.miner_fee.as_zeno(),
            )
            .map_err(|error| match error {
                extension::coin_program::TransferError::Host(error) => error,
                extension::coin_program::TransferError::InvalidBalance => {
                    StateError::InvalidTransaction
                }
                extension::coin_program::TransferError::AmountOverflow => {
                    StateError::AmountOverflow
                }
                extension::coin_program::TransferError::OutputIndexOverflow => {
                    StateError::OutputIndexOverflow
                }
            })?;

            Ok(())
        })();

        self.finish_spend_transition(journal, result)
    }
}

/// Private adapter: only Program execution requests coin mutations. Consensus
/// has already checked signatures, ownership, charges and input uniqueness.
struct KernelCoinHost<'a, F> {
    state: &'a mut LedgerState,
    journal: &'a mut SpendRollbackJournal,
    commitment: CoinTransitionCommitment,
    output_count: usize,
    checkpoint: &'a mut F,
}

impl<F: FnMut(TransitionPoint) -> Result<(), StateError>> extension::coin_program::CoinHost
    for KernelCoinHost<'_, F>
{
    type CoinId = CoinShare;
    type Error = StateError;

    fn input_amount(&self, id: &CoinShare) -> Result<u64, StateError> {
        self.state
            .utxos
            .coin(id)
            .map(|coin| coin.amount.as_zeno())
            .ok_or(StateError::InvalidTransaction)
    }

    fn consume(&mut self, id: CoinShare) -> Result<(), StateError> {
        let coin = self.state.utxos.consume_coin(&id)?;
        self.journal.consumed_coins.push((id, coin));
        (self.checkpoint)(TransitionPoint::CoinInputConsumed)
    }

    fn create(&mut self, index: u32, owner: Address, amount: u64) -> Result<(), StateError> {
        let id = CoinShare::from_output(self.commitment.as_bytes(), index);
        self.state.utxos.insert_coin(
            id,
            CoinUtxo {
                amount: Zeno::from_zeno(amount),
                owner,
            },
        )?;
        self.journal.created_coin_ids.push(id);
        (self.checkpoint)(if index as usize == self.output_count {
            TransitionPoint::MinerFeeCreated
        } else {
            TransitionPoint::CoinOutputCreated
        })
    }

    fn burn(&mut self, amount: u64) -> Result<(), StateError> {
        self.state
            .record_protocol_burn(Zeno::from_zeno(amount), self.journal)?;
        (self.checkpoint)(TransitionPoint::ProtocolBurnRecorded)
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
        let spend = staged.execute_coin_program(&tx.payment, commitment, miner)?;
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
    use crate::{monetary::coin::CoinOutput, transaction::CoinCharges};
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
        let intent = CoinTransition::coin_with_charges(
            owner,
            vec![input],
            vec![CoinOutput::new(address(2), Zeno::from_zeno(70))],
            CoinCharges::new(Zeno::from_zeno(10)),
        )
        .unwrap();
        let commitment = CoinTransitionCommitment::from_bytes([4; HASH_SIZE]);
        let mut expected = original.clone();
        expected
            .execute_coin_program(&intent, commitment, address(9))
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
                state.execute_coin_program_with_checkpoint(
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
                .execute_coin_program(&intent, commitment, address(9))
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
