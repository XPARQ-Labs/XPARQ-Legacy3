use std::{collections::BTreeMap, error::Error as StdError, fmt};

use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{BlockHash, HashDomain, StateRoot, canonical_bytes, domain};

use crate::{
    blockchain::{Block, Chain, ChainError},
    common::Height,
    consensus::{
        ApplyBlockState, CoinInputState, ConsensusError, EmissionError, TransactionConsensusError,
        TransactionStateView, ValidatedBlock, ValidatedTransaction, validate_emission,
        validate_transaction,
    },
    ledger::{CoinUtxo, LedgerState, SpendRollbackJournal, StateError, StateRollbackJournal},
    monetary::coin::{CoinShare, Zeno},
    transaction::SpendIntent,
};

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct Ledger {
    pub chain: Chain,
    pub state: LedgerState,

    journals: BTreeMap<Height, Vec<StateRollbackJournal>>,

    chain_context: Option<crate::common::ChainContext>,
}

/// Ledger data that cannot be rebuilt from the canonical block log alone.
#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq)]
pub struct LedgerSnapshot {
    state: LedgerState,
    journals: BTreeMap<Height, Vec<StateRollbackJournal>>,
}

struct ExecutedBlock {
    state: LedgerState,
    journals: Vec<StateRollbackJournal>,
    state_root: StateRoot,
    block_weight: u32,
    chain_context: crate::common::ChainContext,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BlockTransitionPoint {
    EmissionCreated,
    BeforeAccountingCheck,
}

impl Ledger {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn snapshot(&self) -> LedgerSnapshot {
        LedgerSnapshot {
            state: self.state.clone(),
            journals: self.journals.clone(),
        }
    }

    pub fn from_snapshot(snapshot: LedgerSnapshot, blocks: &[Block]) -> Result<Self, LedgerError> {
        let genesis = blocks.first().ok_or(LedgerError::EmptyChain)?;
        let tip = blocks.last().ok_or(LedgerError::EmptyChain)?;
        let mut chain = Chain::new();
        for block in blocks {
            block.validate_structure().map_err(ConsensusError::from)?;
            chain.insert_block(block.clone())?;
            let expected_journals = usize::from(block.emission().is_some())
                .checked_add(block.transactions().len())
                .ok_or(LedgerError::MissingRollbackJournal)?;
            if snapshot.journals.get(&block.height()).map(Vec::len) != Some(expected_journals) {
                return Err(LedgerError::MissingRollbackJournal);
            }
        }
        if snapshot.journals.len() != blocks.len() {
            return Err(LedgerError::MissingRollbackJournal);
        }
        let ledger = Self {
            chain,
            state: snapshot.state,
            journals: snapshot.journals,
            chain_context: Some(crate::common::ChainContext::new(
                genesis.hash()?.into_bytes(),
            )),
        };
        ledger.state.validate_supply_invariants()?;
        if ledger.state_root()? != tip.state_root() {
            return Err(LedgerError::InvalidStateRoot);
        }
        Ok(ledger)
    }

    pub fn tip_height(&self) -> Option<Height> {
        self.chain.tip_height()
    }

    pub fn tip_hash(&self) -> Option<BlockHash> {
        self.chain.tip_hash()
    }

    pub fn state(&self) -> &LedgerState {
        &self.state
    }

    pub fn state_root(&self) -> Result<StateRoot, LedgerError> {
        self.state.application_state_root()
    }

    pub fn transaction_protocol_burns(
        &self,
        height: Height,
    ) -> Option<Vec<crate::monetary::coin::Zeno>> {
        let block = self.chain.block(&height)?;
        let journals = self.journals.get(&height)?;
        let offset = usize::from(block.emission().is_some());
        let transaction_journals = journals.get(offset..)?;
        if transaction_journals.len() != block.transactions().len() {
            return None;
        }
        Some(
            transaction_journals
                .iter()
                .map(StateRollbackJournal::protocol_burn)
                .collect(),
        )
    }

    pub fn preview_block_state_root(&self, block: &Block) -> Result<StateRoot, LedgerError> {
        self.preview_block_commitments(block).map(|(root, _)| root)
    }

    pub fn preview_block_commitments(
        &self,
        block: &Block,
    ) -> Result<(StateRoot, u32), LedgerError> {
        let executed = self.execute_block(block)?;
        Ok((executed.state_root, executed.block_weight))
    }

    fn execute_block(&self, block: &Block) -> Result<ExecutedBlock, LedgerError> {
        self.execute_block_with_checkpoint(block, |_, _| Ok(()))
    }

    fn execute_block_with_checkpoint(
        &self,
        block: &Block,
        mut checkpoint: impl FnMut(BlockTransitionPoint, &mut LedgerState) -> Result<(), LedgerError>,
    ) -> Result<ExecutedBlock, LedgerError> {
        self.chain.validate_next_block(block)?;
        match self.chain.tip_height() {
            Some(height) => {
                let tip = self.chain.block(&height).ok_or(LedgerError::EmptyChain)?;
                if self.state_root()? != tip.state_root() {
                    return Err(LedgerError::InvalidPriorStateRoot);
                }
            }
            None if self.state_root()? != StateRoot::ZERO => {
                return Err(LedgerError::InvalidPriorStateRoot);
            }
            None => {}
        }

        let mut state = self.state.clone();
        let coin_supply_before = coin_utxo_total(&state)?;
        let mut validated_subsidy = Zeno::ZERO;
        let mut expected_burns = Zeno::ZERO;
        let mut journals = Vec::new();
        let block_weight =
            u32::try_from(block.weight()?).map_err(|_| LedgerError::InvalidBlockWeight)?;
        let height = block.height();
        let chain_context = match self.chain_context {
            Some(context) => context,
            None if block.is_genesis() => {
                crate::common::ChainContext::new(block.hash()?.into_bytes())
            }
            None => return Err(LedgerError::EmptyChain),
        };

        if !block.is_genesis() {
            let emission = validate_emission(block)?;
            validated_subsidy = emission.subsidy();
            expected_burns = emission.protocol_burn();
            let id = CoinShare::from_emission_origin(&emission.origin().0);
            state.utxos.insert_coin(
                id,
                CoinUtxo {
                    amount: emission.miner_emission(),
                    owner: emission.recipient(),
                },
            )?;
            checkpoint(BlockTransitionPoint::EmissionCreated, &mut state)?;
            state.coin.total_mined = state
                .coin
                .total_mined
                .checked_add(emission.subsidy())
                .ok_or(StateError::AmountOverflow)?;

            let mut spend = SpendRollbackJournal {
                created_coin_ids: vec![id],
                mined: emission.subsidy(),
                ..SpendRollbackJournal::default()
            };
            state.record_protocol_burn(emission.protocol_burn(), &mut spend)?;
            journals.push(StateRollbackJournal {
                spend: Some(spend),
                extension: None,
            });
        }

        for transaction in block.transactions() {
            let validated =
                validate_transaction(transaction.clone(), chain_context, height.0, &state)?;
            let spend = match &validated {
                ValidatedTransaction::CoinSpend(tx) => tx.spend.intent(),
                ValidatedTransaction::Program(tx) => &tx.transaction.payment,
            };
            let burn = expected_spend_burn(&state, spend)?;
            expected_burns = expected_burns
                .checked_add(burn)
                .ok_or(LedgerError::SupplyOverflow)?;
            journals.push(state.apply_validated_transaction(
                &validated,
                block.miner_address(),
                chain_context,
            )?);
        }

        checkpoint(BlockTransitionPoint::BeforeAccountingCheck, &mut state)?;
        validate_block_accounting(
            coin_supply_before,
            &state,
            validated_subsidy,
            expected_burns,
        )?;
        state.validate_supply_invariants()?;
        let state_root = state.application_state_root()?;
        Ok(ExecutedBlock {
            state,
            journals,
            state_root,
            block_weight,
            chain_context,
        })
    }

    pub fn rollback_tip(&mut self) -> Result<Block, LedgerError> {
        let height = self.chain.tip_height().ok_or(LedgerError::EmptyChain)?;

        let hash = self.chain.tip_hash().ok_or(LedgerError::EmptyChain)?;
        let tip = self.chain.block(&height).ok_or(LedgerError::EmptyChain)?;
        if self.state_root()? != tip.state_root() {
            return Err(LedgerError::InvalidPriorStateRoot);
        }

        let journals = self
            .journals
            .get(&height)
            .cloned()
            .ok_or(LedgerError::MissingRollbackJournal)?;

        let mut staged_state = self.state.clone();

        for journal in journals.into_iter().rev() {
            staged_state.rollback_state(journal)?;
        }

        let mut staged_chain = self.chain.clone();

        let block = staged_chain.remove_tip(hash)?;

        staged_state.validate_supply_invariants()?;
        let expected_root = match staged_chain.tip_height() {
            Some(parent_height) => staged_chain
                .block(&parent_height)
                .ok_or(LedgerError::EmptyChain)?
                .state_root(),
            None => StateRoot::ZERO,
        };
        if staged_state.application_state_root()? != expected_root {
            return Err(LedgerError::InvalidRollbackStateRoot);
        }
        self.state = staged_state;

        self.chain = staged_chain;

        self.journals.remove(&height);

        if self.chain.tip_height().is_none() {
            self.chain_context = None;
        }

        Ok(block)
    }

    fn apply_validated_block(&mut self, validated: ValidatedBlock) -> Result<(), LedgerError> {
        let block = validated.block();
        let height = block.height();
        let executed = self.execute_block(block)?;

        if executed.block_weight != block.block_weight() {
            return Err(LedgerError::InvalidBlockWeight);
        }
        if block.state_root() != executed.state_root {
            return Err(LedgerError::InvalidStateRoot);
        }
        let mut staged_chain = self.chain.clone();
        staged_chain.insert_block(block.clone())?;
        self.state = executed.state;
        self.chain = staged_chain;
        self.chain_context = Some(executed.chain_context);
        self.journals.insert(height, executed.journals);
        Ok(())
    }
}

fn coin_utxo_total(state: &LedgerState) -> Result<Zeno, LedgerError> {
    state
        .utxos
        .coins()
        .try_fold(Zeno::ZERO, |total, (_, coin)| {
            total
                .checked_add(coin.amount)
                .ok_or(LedgerError::SupplyOverflow)
        })
}

fn expected_spend_burn(state: &LedgerState, intent: &SpendIntent) -> Result<Zeno, LedgerError> {
    let (inputs, outputs) = intent
        .coin_parts()
        .ok_or(LedgerError::BlockAccountingMismatch)?;
    let input_total = inputs.iter().try_fold(Zeno::ZERO, |total, id| {
        let coin = state
            .utxos
            .coin(id)
            .ok_or(LedgerError::BlockAccountingMismatch)?;
        total
            .checked_add(coin.amount)
            .ok_or(LedgerError::SupplyOverflow)
    })?;
    let output_total = outputs
        .iter()
        .try_fold(intent.charges.miner_fee, |total, output| {
            total
                .checked_add(output.amount)
                .ok_or(LedgerError::SupplyOverflow)
        })?;
    input_total
        .checked_sub(output_total)
        .ok_or(LedgerError::BlockAccountingMismatch)
}

fn validate_block_accounting(
    before: Zeno,
    after: &LedgerState,
    subsidy: Zeno,
    burns: Zeno,
) -> Result<(), LedgerError> {
    let expected = before
        .checked_add(subsidy)
        .and_then(|total| total.checked_sub(burns))
        .ok_or(LedgerError::BlockAccountingMismatch)?;
    if coin_utxo_total(after)? != expected {
        return Err(LedgerError::BlockAccountingMismatch);
    }
    Ok(())
}

//
// Consensus state interface
//

impl ApplyBlockState for Ledger {
    type Error = LedgerError;

    fn consensus_chain(&self) -> &Chain {
        &self.chain
    }

    fn commit_validated_block(&mut self, block: ValidatedBlock) -> Result<(), Self::Error> {
        self.apply_validated_block(block)
    }
}

//
// Transaction state view
//

impl TransactionStateView for LedgerState {
    fn extension_state(&self) -> Option<&extension::script::state::ExtensionState> {
        Some(&self.extensions)
    }

    fn coin(&self, id: CoinShare) -> Option<CoinInputState> {
        self.utxos.coin(&id).map(|coin| CoinInputState {
            amount: coin.amount,
            owner: coin.owner,
        })
    }
}

//
// Canonical application state root
//

impl LedgerState {
    /// Cross-check accounting records against independently stored live UTXOs.
    pub fn validate_supply_invariants(&self) -> Result<(), LedgerError> {
        let coin_total = self
            .utxos
            .coins()
            .try_fold(Zeno::ZERO, |total, (_, coin)| {
                total.checked_add(coin.amount)
            })
            .ok_or(LedgerError::SupplyOverflow)?;
        if self.coin.supply() != Some(coin_total) {
            return Err(LedgerError::CoinSupplyMismatch);
        }

        let assets = &self.extensions.assets;
        let mut totals = BTreeMap::new();
        for share in assets.shares.values() {
            if !assets.records.contains_key(&share.asset) {
                return Err(LedgerError::UnknownAssetShare);
            }
            let total = totals
                .entry(share.asset)
                .or_insert(extension::asset_program::asset::Unit::ZERO);
            *total = total
                .checked_add(share.amount)
                .ok_or(LedgerError::SupplyOverflow)?;
        }
        for (asset, record) in &assets.records {
            if record.total_minted > record.metadata.max_supply
                || record.total_minted.checked_sub(record.total_burned) != Some(record.supply)
                || totals
                    .get(asset)
                    .copied()
                    .unwrap_or(extension::asset_program::asset::Unit::ZERO)
                    != record.supply
            {
                return Err(LedgerError::AssetSupplyMismatch);
            }
        }
        Ok(())
    }

    pub(crate) fn application_state_root(&self) -> Result<StateRoot, LedgerError> {
        if self.extensions == extension::script::state::ExtensionState::default()
            && self.utxos.is_empty()
            && self.coin.total_mined.is_zero()
            && self.coin.total_burned.is_zero()
        {
            return Ok(StateRoot::ZERO);
        }

        let state = canonical_bytes(&(&self.utxos, &self.coin, &self.extensions))?;

        Ok(StateRoot(
            domain(HashDomain::ProtocolState, &state).into_bytes(),
        ))
    }
}

//
// Ledger errors
//

#[derive(Debug)]
pub enum LedgerError {
    Consensus(ConsensusError),

    Transaction(TransactionConsensusError),

    State(StateError),

    Chain(ChainError),

    Emission(EmissionError),

    EmptyChain,

    MissingParentEmission,

    MissingRollbackJournal,

    InvalidStateRoot,

    InvalidPriorStateRoot,

    InvalidRollbackStateRoot,

    InvalidBlockWeight,

    SupplyOverflow,

    CoinSupplyMismatch,

    BlockAccountingMismatch,

    AssetSupplyMismatch,

    UnknownAssetShare,
}

impl fmt::Display for LedgerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Consensus(error) => {
                write!(formatter, "consensus validation failed: {error}")
            }

            Self::Transaction(error) => {
                write!(formatter, "transaction validation failed: {error}")
            }

            Self::State(error) => {
                write!(formatter, "ledger state transition failed: {error}")
            }

            Self::Chain(error) => {
                write!(formatter, "chain transition failed: {error}")
            }

            Self::Emission(error) => {
                write!(formatter, "emission validation failed: {error}")
            }

            Self::EmptyChain => formatter.write_str("ledger chain is empty"),

            Self::MissingParentEmission => formatter.write_str("parent emission is missing"),

            Self::MissingRollbackJournal => formatter.write_str("rollback journal is missing"),

            Self::InvalidStateRoot => formatter.write_str("block state root does not match ledger"),

            Self::InvalidPriorStateRoot => {
                formatter.write_str("active state root does not match canonical tip")
            }

            Self::InvalidRollbackStateRoot => {
                formatter.write_str("rolled-back state root does not match parent block")
            }

            Self::InvalidBlockWeight => {
                formatter.write_str("block execution weight does not match ledger")
            }

            Self::SupplyOverflow => formatter.write_str("UTXO supply sum overflowed"),

            Self::CoinSupplyMismatch => {
                formatter.write_str("coin UTXO total does not match supply")
            }

            Self::BlockAccountingMismatch => {
                formatter.write_str("block coin supply delta does not match subsidy and burns")
            }

            Self::AssetSupplyMismatch => {
                formatter.write_str("asset share total does not match recorded supply")
            }

            Self::UnknownAssetShare => formatter.write_str("asset share has no registered asset"),
        }
    }
}

impl StdError for LedgerError {}

impl From<ConsensusError> for LedgerError {
    fn from(error: ConsensusError) -> Self {
        Self::Consensus(error)
    }
}

impl From<TransactionConsensusError> for LedgerError {
    fn from(error: TransactionConsensusError) -> Self {
        Self::Transaction(error)
    }
}

impl From<EmissionError> for LedgerError {
    fn from(error: EmissionError) -> Self {
        Self::Emission(error)
    }
}

impl From<StateError> for LedgerError {
    fn from(error: StateError) -> Self {
        Self::State(error)
    }
}

impl From<crate::ledger::utxo::Error> for LedgerError {
    fn from(error: crate::ledger::utxo::Error) -> Self {
        Self::State(StateError::Utxo(error))
    }
}

impl From<ChainError> for LedgerError {
    fn from(error: ChainError) -> Self {
        Self::Chain(error)
    }
}

impl From<crypto::CodecError> for LedgerError {
    fn from(_error: crypto::CodecError) -> Self {
        Self::Consensus(ConsensusError::Serialization)
    }
}

#[cfg(all(test, feature = "mainnet"))]
#[path = "phase4_vectors.rs"]
mod phase4_vectors;

#[cfg(test)]
mod p3e_block_atomicity_tests {
    use super::*;

    use crate::{
        blockchain::{Block, Emission},
        common::Nonce,
        consensus::{
            ConsensusError, expected_emission_for_height, expected_next_difficulty,
            initial_block_emission, validate_candidate_for_apply,
        },
        genesis,
    };

    fn ledger_bytes(ledger: &Ledger) -> Vec<u8> {
        borsh::to_vec(ledger).expect("ledger must serialize canonically")
    }

    #[test]
    fn unexpected_coin_supply_delta_is_rejected() {
        let mut state = LedgerState::default();
        state
            .utxos
            .insert_coin(
                CoinShare::from_bytes([0x44; crypto::HASH16_SIZE]),
                CoinUtxo {
                    amount: Zeno::from_zeno(109),
                    owner: crypto::Address([0x55; crypto::ADDRESS_SIZE]),
                },
            )
            .unwrap();
        assert!(matches!(
            validate_block_accounting(
                Zeno::from_zeno(100),
                &state,
                Zeno::from_zeno(10),
                Zeno::from_zeno(2),
            ),
            Err(LedgerError::BlockAccountingMismatch)
        ));
    }

    #[test]
    fn block_with_forged_extra_coin_is_rejected_even_if_supply_record_matches() {
        let ledger = genesis::genesis_ledger().unwrap();
        let before = ledger_bytes(&ledger);
        let block =
            empty_height_one_candidate(&ledger, crypto::Address([0x56; crypto::ADDRESS_SIZE]));
        let result = ledger.execute_block_with_checkpoint(&block, |point, state| {
            if point == BlockTransitionPoint::BeforeAccountingCheck {
                let (id, coin) = state.utxos.coins().next().unwrap();
                let forged = CoinUtxo {
                    amount: coin.amount.checked_add(Zeno::ONE).unwrap(),
                    ..*coin
                };
                state.utxos.consume_coin(&id).unwrap();
                state.utxos.insert_coin(id, forged).unwrap();
                state.coin.total_mined = state.coin.total_mined.checked_add(Zeno::ONE).unwrap();
                assert!(state.validate_supply_invariants().is_ok());
            }
            Ok(())
        });
        assert!(matches!(result, Err(LedgerError::BlockAccountingMismatch)));
        assert_eq!(ledger_bytes(&ledger), before);
    }

    fn empty_next_candidate(ledger: &Ledger, miner: crypto::Address) -> Block {
        let height = Height(
            ledger
                .tip_height()
                .map_or(0, |height| height.0.saturating_add(1)),
        );

        let previous = ledger.tip_hash().expect("canonical tip");

        let target_bits = expected_next_difficulty(&ledger.chain).expect("next target bits");

        let subsidy = expected_emission_for_height(height);

        Block::from_protocol_transactions(
            height,
            previous,
            target_bits,
            Nonce(0),
            Some(Emission::new(miner, subsidy)),
            vec![],
        )
        .expect("empty candidate")
    }

    fn commit_empty_block(ledger: &mut Ledger, miner: crypto::Address) -> Block {
        let mut block = empty_next_candidate(ledger, miner);

        let (state_root, block_weight) = ledger
            .preview_block_commitments(&block)
            .expect("preview commitments");

        block.set_state_root(state_root);
        block.set_block_weight(block_weight);

        let validated =
            validate_candidate_for_apply(&block, &ledger.chain).expect("valid candidate");

        ledger
            .apply_validated_block(validated)
            .expect("commit block");

        block
    }

    fn empty_height_one_candidate(ledger: &Ledger, miner: crypto::Address) -> Block {
        let previous = ledger.tip_hash().expect("genesis tip");

        let target_bits = expected_next_difficulty(&ledger.chain).expect("next target bits");

        Block::from_protocol_transactions(
            Height(1),
            previous,
            target_bits,
            Nonce(0),
            Some(Emission::new(miner, initial_block_emission())),
            vec![],
        )
        .expect("height-one candidate")
    }

    #[test]
    fn invalid_state_root_after_staging_does_not_mutate_ledger() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner = crypto::Address([0x41; crypto::ADDRESS_SIZE]);

        let before = ledger_bytes(&ledger);

        let before_tip = ledger.tip_hash();

        let block = empty_height_one_candidate(&ledger, miner);

        let validated = validate_candidate_for_apply(&block, &ledger.chain)
            .expect("candidate must pass pre-application consensus");

        assert!(matches!(
            ledger.apply_validated_block(validated),
            Err(LedgerError::InvalidStateRoot)
        ));

        assert_eq!(ledger_bytes(&ledger), before);

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger.tip_height(), Some(Height(0)));
    }

    #[test]
    fn committed_block_then_rollback_restores_entire_ledger_byte_for_byte() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner = crypto::Address([0x42; crypto::ADDRESS_SIZE]);

        let before = ledger_bytes(&ledger);

        let before_tip = ledger.tip_hash();

        let mut block = empty_height_one_candidate(&ledger, miner);

        let (state_root, block_weight) = ledger
            .preview_block_commitments(&block)
            .expect("preview commitments");

        block.set_state_root(state_root);
        block.set_block_weight(block_weight);

        let validated =
            validate_candidate_for_apply(&block, &ledger.chain).expect("valid candidate");

        ledger
            .apply_validated_block(validated)
            .expect("commit block");

        assert_eq!(ledger.tip_height(), Some(Height(1)));

        assert_ne!(ledger_bytes(&ledger), before);

        let removed = ledger.rollback_tip().expect("rollback tip");

        assert_eq!(removed, block);

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger.tip_height(), Some(Height(0)));

        assert_eq!(ledger_bytes(&ledger), before);
    }

    #[test]
    fn tampered_active_state_rejects_next_block_and_rollback_without_mutation() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");
        commit_empty_block(&mut ledger, crypto::Address([0x81; crypto::ADDRESS_SIZE]));
        let (coin_id, coin) = ledger.state.utxos.coins().next().expect("emission coin");
        let mut altered = *coin;
        altered.owner = crypto::Address([0x82; crypto::ADDRESS_SIZE]);
        ledger.state.utxos.consume_coin(&coin_id).unwrap();
        ledger.state.utxos.insert_coin(coin_id, altered).unwrap();
        assert!(ledger.state.validate_supply_invariants().is_ok());

        let before = ledger_bytes(&ledger);
        let next = empty_next_candidate(&ledger, crypto::Address([0x83; crypto::ADDRESS_SIZE]));
        assert!(matches!(
            ledger.preview_block_commitments(&next),
            Err(LedgerError::InvalidPriorStateRoot)
        ));
        assert!(matches!(
            ledger.rollback_tip(),
            Err(LedgerError::InvalidPriorStateRoot)
        ));
        assert_eq!(ledger_bytes(&ledger), before);
    }

    #[test]
    fn rollback_rejects_journal_that_preserves_supply_but_changes_parent_root() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");
        commit_empty_block(&mut ledger, crypto::Address([0x84; crypto::ADDRESS_SIZE]));
        let journal = &mut ledger
            .journals
            .get_mut(&Height(1))
            .expect("height-one journal")[0]
            .spend
            .as_mut()
            .expect("emission journal");
        journal.mined = journal.mined.checked_sub(Zeno::ONE).unwrap();
        journal.burned = journal.burned.checked_sub(Zeno::ONE).unwrap();

        let before = ledger_bytes(&ledger);
        assert!(matches!(
            ledger.rollback_tip(),
            Err(LedgerError::InvalidRollbackStateRoot)
        ));
        assert_eq!(ledger_bytes(&ledger), before);
        assert_eq!(ledger.tip_height(), Some(Height(1)));
    }

    #[test]
    fn rollback_to_non_genesis_parent_checks_parent_root() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");
        commit_empty_block(&mut ledger, crypto::Address([0x85; crypto::ADDRESS_SIZE]));
        commit_empty_block(&mut ledger, crypto::Address([0x86; crypto::ADDRESS_SIZE]));
        let journal = &mut ledger
            .journals
            .get_mut(&Height(2))
            .expect("height-two journal")[0]
            .spend
            .as_mut()
            .expect("emission journal");
        journal.mined = journal.mined.checked_sub(Zeno::ONE).unwrap();
        journal.burned = journal.burned.checked_sub(Zeno::ONE).unwrap();

        let before = ledger_bytes(&ledger);
        assert!(matches!(
            ledger.rollback_tip(),
            Err(LedgerError::InvalidRollbackStateRoot)
        ));
        assert_eq!(ledger_bytes(&ledger), before);
        assert_eq!(ledger.tip_height(), Some(Height(2)));
    }

    #[test]
    fn rolling_back_genesis_restores_empty_state_root() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");
        let genesis = ledger.chain.block(&Height(0)).unwrap().clone();
        assert_eq!(ledger.rollback_tip().unwrap(), genesis);
        assert_eq!(ledger.tip_height(), None);
        assert_eq!(ledger.state_root().unwrap(), StateRoot::ZERO);
    }

    #[test]
    fn failure_after_emission_creation_does_not_change_canonical_ledger() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");
        let mut baseline = ledger.clone();
        let miner = crypto::Address([0x91; crypto::ADDRESS_SIZE]);
        let block = empty_next_candidate(&ledger, miner);
        let before = ledger_bytes(&ledger);

        assert!(matches!(
            ledger.execute_block_with_checkpoint(&block, |point, _| {
                if point == BlockTransitionPoint::EmissionCreated {
                    Err(LedgerError::InvalidStateRoot)
                } else {
                    Ok(())
                }
            }),
            Err(LedgerError::InvalidStateRoot)
        ));
        assert_eq!(ledger_bytes(&ledger), before);

        commit_empty_block(&mut ledger, miner);
        commit_empty_block(&mut baseline, miner);
        assert_eq!(ledger_bytes(&ledger), ledger_bytes(&baseline));
    }

    #[test]
    fn two_committed_blocks_then_two_rollbacks_restore_genesis_byte_for_byte() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");

        let genesis_bytes = ledger_bytes(&ledger);

        let genesis_tip = ledger.tip_hash();

        let miner_one = crypto::Address([0x51; crypto::ADDRESS_SIZE]);

        let miner_two = crypto::Address([0x52; crypto::ADDRESS_SIZE]);

        let block_one = commit_empty_block(&mut ledger, miner_one);

        assert_eq!(ledger.tip_height(), Some(Height(1)));

        let height_one_bytes = ledger_bytes(&ledger);

        let height_one_tip = ledger.tip_hash();

        let block_two = commit_empty_block(&mut ledger, miner_two);

        assert_eq!(ledger.tip_height(), Some(Height(2)));

        assert_ne!(ledger_bytes(&ledger), height_one_bytes);

        let removed_two = ledger.rollback_tip().expect("rollback height two");

        assert_eq!(removed_two, block_two);

        assert_eq!(ledger.tip_height(), Some(Height(1)));

        assert_eq!(ledger.tip_hash(), height_one_tip);

        assert_eq!(ledger_bytes(&ledger), height_one_bytes);

        let removed_one = ledger.rollback_tip().expect("rollback height one");

        assert_eq!(removed_one, block_one);

        assert_eq!(ledger.tip_height(), Some(Height(0)));

        assert_eq!(ledger.tip_hash(), genesis_tip);

        assert_eq!(ledger_bytes(&ledger), genesis_bytes);
    }

    #[test]
    fn failed_second_block_does_not_mutate_committed_first_block() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner_one = crypto::Address([0x61; crypto::ADDRESS_SIZE]);

        let miner_two = crypto::Address([0x62; crypto::ADDRESS_SIZE]);

        let block_one = commit_empty_block(&mut ledger, miner_one);

        assert_eq!(ledger.tip_height(), Some(Height(1)));

        let before = ledger_bytes(&ledger);

        let before_tip = ledger.tip_hash();

        let block_two = empty_next_candidate(&ledger, miner_two);

        let validated = validate_candidate_for_apply(&block_two, &ledger.chain)
            .expect("candidate must pass pre-application consensus");

        assert!(matches!(
            ledger.apply_validated_block(validated),
            Err(LedgerError::InvalidStateRoot)
        ));

        assert_eq!(ledger.tip_height(), Some(Height(1)));

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger_bytes(&ledger), before);

        let removed = ledger.rollback_tip().expect("height-one rollback");

        assert_eq!(removed, block_one);

        assert_eq!(ledger.tip_height(), Some(Height(0)));
    }

    #[test]
    fn invalid_block_weight_after_staging_does_not_mutate_ledger() {
        let mut ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner = crypto::Address([0x71; crypto::ADDRESS_SIZE]);

        let before = ledger_bytes(&ledger);
        let before_tip = ledger.tip_hash();

        let mut block = empty_next_candidate(&ledger, miner);

        let (state_root, block_weight) = ledger
            .preview_block_commitments(&block)
            .expect("preview commitments");

        block.set_state_root(state_root);

        //
        // Keep the weight structurally plausible, but make it
        // different from the canonical execution weight.
        //
        block.set_block_weight(
            block_weight
                .checked_add(1)
                .expect("fixture block weight overflow"),
        );

        //
        // The malformed weight is still large enough to satisfy
        // block-local structural validation.
        //
        let validated = validate_candidate_for_apply(&block, &ledger.chain)
            .expect("candidate must pass pre-application consensus");

        assert!(matches!(
            ledger.apply_validated_block(validated),
            Err(LedgerError::InvalidBlockWeight)
        ));

        assert_eq!(ledger_bytes(&ledger), before);

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger.tip_height(), Some(Height(0)));
    }

    #[test]
    fn invalid_next_height_is_rejected_without_mutating_ledger() {
        let ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner = crypto::Address([0x72; crypto::ADDRESS_SIZE]);

        let before = ledger_bytes(&ledger);
        let before_tip = ledger.tip_hash();

        let mut block = empty_next_candidate(&ledger, miner);

        //
        // Genesis tip is height 0, therefore the only valid
        // next block is height 1.
        //
        block.height = Height(2);

        assert!(matches!(
            validate_candidate_for_apply(&block, &ledger.chain,),
            Err(ConsensusError::InvalidHeight)
        ));

        assert_eq!(ledger_bytes(&ledger), before);

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger.tip_height(), Some(Height(0)));
    }
    #[test]
    fn invalid_previous_hash_is_rejected_without_mutating_ledger() {
        let ledger = genesis::genesis_ledger().expect("genesis ledger");

        let miner = crypto::Address([0x73; crypto::ADDRESS_SIZE]);

        let before = ledger_bytes(&ledger);
        let before_tip = ledger.tip_hash();

        let mut block = empty_next_candidate(&ledger, miner);

        let mut wrong_previous = ledger.tip_hash().expect("genesis tip").0;

        wrong_previous[0] ^= 0xff;

        block.header.previous_hash = crypto::PreviousHash(wrong_previous);

        assert!(matches!(
            validate_candidate_for_apply(&block, &ledger.chain,),
            Err(ConsensusError::InvalidPreviousHash)
        ));

        assert_eq!(ledger_bytes(&ledger), before);

        assert_eq!(ledger.tip_hash(), before_tip);

        assert_eq!(ledger.tip_height(), Some(Height(0)));
    }
    #[test]
    fn program_block_snapshot_replay_and_reorg_restore_exact_state() {
        use crate::consensus::{ProtocolBurn, StateTransitionWeight};
        use crate::monetary::coin::CoinOutput;
        use crate::transaction::{
            AccountAuthorization, AuthorizedProgramTransaction, AuthorizedTransaction, SpendIntent,
            program_transaction_commitment,
        };
        use crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key};
        use extension::{
            asset_program::{
                asset::Unit as ExtUnit,
                opcode::AssetOpcode,
                state::ExecutionContext,
                type_::{AssetCall, Register},
            },
            script::call::{ProgramCall, ProgramId},
        };
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([24; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let miner = crypto::Address([0x82; crypto::ADDRESS_SIZE]);
        let mut ledger = genesis::genesis_ledger().unwrap();
        commit_empty_block(&mut ledger, signer);
        let parent = ledger.clone();
        let chain = ledger.chain_context.unwrap();
        let (input, coin) = ledger.state.utxos.coins().next().unwrap();
        let amount = coin.amount;
        let register = Register {
            name: "ATOMIC".into(),
            max_supply: ExtUnit::from_units(100),
            initial_mint: ExtUnit::from_units(10),
            mint_authority: signer,
            nonce: 1,
        };
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: AssetOpcode::Register as u8,
            payload: borsh::to_vec(&register).unwrap(),
        };
        let sign = |output| {
            let payment =
                SpendIntent::coin(signer, vec![input], vec![CoinOutput::new(signer, output)])
                    .unwrap();
            let commitment =
                program_transaction_commitment(signer, &call, &payment, chain).unwrap();
            AuthorizedProgramTransaction {
                signer,
                call: call.clone(),
                payment,
                authorization: AccountAuthorization {
                    public_key: seed.public_key(),
                    signature: seed.sign(commitment.as_bytes()),
                },
            }
        };
        let dummy = sign(Zeno::ONE);
        let mut preview = ledger.state.extensions.clone();
        preview
            .assets
            .apply(
                &AssetCall::Register(register),
                ExecutionContext {
                    signer,
                    commitment: [9; 32],
                },
            )
            .unwrap();
        let growth = (canonical_bytes(&preview).unwrap().len()
            - canonical_bytes(&ledger.state.extensions).unwrap().len()) as u64;
        let size = canonical_bytes(&AuthorizedTransaction::Program(Box::new(dummy)))
            .unwrap()
            .len() as u64;
        let burn = ProtocolBurn::for_transaction(
            StateTransitionWeight {
                created_coin_utxos: 1,
                consumed_coin_utxos: 1,
                created_state_weight: growth,
            },
            size,
        )
        .unwrap()
        .total()
        .unwrap();
        let tx = AuthorizedTransaction::Program(Box::new(sign(amount.checked_sub(burn).unwrap())));
        let candidate = |transactions| {
            Block::from_protocol_transactions(
                Height(2),
                parent.tip_hash().unwrap(),
                expected_next_difficulty(&parent.chain).unwrap(),
                Nonce(0),
                Some(Emission::new(
                    miner,
                    expected_emission_for_height(Height(2)),
                )),
                transactions,
            )
            .unwrap()
        };
        let mut block = candidate(vec![tx.clone()]);
        assert!(ledger.execute_block(&block).is_ok());
        let bad = candidate(vec![tx.clone(), tx]);
        assert!(ledger.execute_block(&bad).is_err());
        assert_eq!(ledger, parent);
        let executed = ledger.execute_block(&block).unwrap();
        block.set_state_root(executed.state_root);
        block.set_block_weight(executed.block_weight);
        let validated = validate_candidate_for_apply(&block, &ledger.chain).unwrap();
        ledger.apply_validated_block(validated).unwrap();
        assert_eq!(ledger.state.extensions.assets.records.len(), 1);
        assert_eq!(
            ledger.transaction_protocol_burns(Height(2)).unwrap(),
            vec![burn]
        );
        let bytes = ledger_bytes(&ledger);
        let restored = Ledger::try_from_slice(&bytes).unwrap();
        assert_eq!(restored, ledger);
        let blocks = ledger.chain.blocks().cloned().collect::<Vec<_>>();
        let snapshot =
            LedgerSnapshot::try_from_slice(&borsh::to_vec(&ledger.snapshot()).unwrap()).unwrap();
        let mut recovered = Ledger::from_snapshot(snapshot, &blocks).unwrap();
        assert_eq!(recovered, ledger);
        let mut corrupt = recovered.clone();
        corrupt.journals.get_mut(&Height(2)).unwrap()[1]
            .spend
            .as_mut()
            .unwrap()
            .created_coin_ids
            .push(CoinShare::from_bytes([0xfa; crypto::HASH16_SIZE]));
        let before = ledger_bytes(&corrupt);
        assert!(corrupt.rollback_tip().is_err());
        assert_eq!(ledger_bytes(&corrupt), before);
        recovered.rollback_tip().unwrap();
        assert_eq!(recovered, parent);
        let mut replay = parent.clone();
        replay
            .apply_validated_block(validate_candidate_for_apply(&block, &parent.chain).unwrap())
            .unwrap();
        assert_eq!(replay, ledger);
        replay.rollback_tip().unwrap();
        commit_empty_block(&mut replay, miner);
        assert!(replay.state.extensions.assets.records.is_empty());
        replay.rollback_tip().unwrap();
        assert_eq!(replay, parent);
    }

    #[test]
    fn active_program_lifecycle_replays_and_rolls_back_every_opcode() {
        use crate::{
            consensus::{ProtocolBurn, StateTransitionWeight},
            monetary::coin::CoinOutput,
            transaction::{
                AccountAuthorization, AuthorizedProgramTransaction, AuthorizedTransaction,
                SpendCharges, program_transaction_commitment,
            },
        };
        use crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key};
        use extension::{
            asset_program::{
                asset::{AssetOutput, Unit as ExtUnit},
                opcode::AssetOpcode,
                state::ExecutionContext,
                type_::{AssetCall, Burn, Mint, Register, Transfer},
            },
            script::call::{ProgramCall, ProgramId},
        };
        fn commit_call(ledger: &mut Ledger, seed: &SigningSeed, call: AssetCall) {
            let signer = address_from_public_key(&seed.public_key());
            let chain = ledger.chain_context.unwrap();
            let (opcode, payload) = match &call {
                AssetCall::Register(v) => (AssetOpcode::Register, borsh::to_vec(v).unwrap()),
                AssetCall::Mint(v) => (AssetOpcode::Mint, borsh::to_vec(v).unwrap()),
                AssetCall::Transfer(v) => (AssetOpcode::Transfer, borsh::to_vec(v).unwrap()),
                AssetCall::Burn(v) => (AssetOpcode::Burn, borsh::to_vec(v).unwrap()),
            };
            let mut preview = ledger.state.extensions.clone();
            preview
                .assets
                .apply(
                    &call,
                    ExecutionContext {
                        signer,
                        commitment: [11; 32],
                    },
                )
                .unwrap();
            let growth = canonical_bytes(&preview)
                .unwrap()
                .len()
                .saturating_sub(canonical_bytes(&ledger.state.extensions).unwrap().len())
                as u64;
            let call = ProgramCall {
                program: ProgramId::ASSET,
                opcode: opcode as u8,
                payload,
            };
            let (input, coin) = ledger
                .state
                .utxos
                .coins()
                .filter(|(_, v)| v.owner == signer)
                .max_by_key(|(_, v)| v.amount)
                .unwrap();
            let amount = coin.amount;
            let sign = |output| {
                let payment = SpendIntent::coin_with_charges(
                    signer,
                    vec![input],
                    vec![CoinOutput::new(signer, output)],
                    SpendCharges::new(Zeno::ONE),
                )
                .unwrap();
                let commitment =
                    program_transaction_commitment(signer, &call, &payment, chain).unwrap();
                AuthorizedTransaction::Program(Box::new(AuthorizedProgramTransaction {
                    signer,
                    call: call.clone(),
                    payment,
                    authorization: AccountAuthorization {
                        public_key: seed.public_key(),
                        signature: seed.sign(commitment.as_bytes()),
                    },
                }))
            };
            let size = canonical_bytes(&sign(Zeno::ONE)).unwrap().len() as u64;
            let burn = ProtocolBurn::for_transaction(
                StateTransitionWeight {
                    created_coin_utxos: 2,
                    consumed_coin_utxos: 1,
                    created_state_weight: growth,
                },
                size,
            )
            .unwrap()
            .total()
            .unwrap();
            let tx = sign(
                amount
                    .checked_sub(burn)
                    .unwrap()
                    .checked_sub(Zeno::ONE)
                    .unwrap(),
            );
            let height = Height(ledger.tip_height().unwrap().0 + 1);
            let mut block = Block::from_protocol_transactions(
                height,
                ledger.tip_hash().unwrap(),
                expected_next_difficulty(&ledger.chain).unwrap(),
                Nonce(0),
                Some(Emission::new(signer, expected_emission_for_height(height))),
                vec![tx],
            )
            .unwrap();
            let (root, weight) = ledger.preview_block_commitments(&block).unwrap();
            block.set_state_root(root);
            block.set_block_weight(weight);
            ledger
                .apply_validated_block(validate_candidate_for_apply(&block, &ledger.chain).unwrap())
                .unwrap();
            let blocks = ledger.chain.blocks().cloned().collect::<Vec<_>>();
            let snapshot =
                LedgerSnapshot::try_from_slice(&borsh::to_vec(&ledger.snapshot()).unwrap())
                    .unwrap();
            *ledger = Ledger::from_snapshot(snapshot, &blocks).unwrap();
        }
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([25; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let mut ledger = genesis::genesis_ledger().unwrap();
        commit_empty_block(&mut ledger, signer);
        let mut checkpoints = vec![ledger.clone()];
        commit_call(
            &mut ledger,
            &seed,
            AssetCall::Register(Register {
                name: "LIFECYCLE".into(),
                max_supply: ExtUnit::from_units(100),
                initial_mint: ExtUnit::from_units(40),
                mint_authority: signer,
                nonce: 1,
            }),
        );
        checkpoints.push(ledger.clone());
        let asset = *ledger
            .state
            .extensions
            .assets
            .records
            .keys()
            .next()
            .unwrap();
        commit_call(
            &mut ledger,
            &seed,
            AssetCall::Mint(Mint {
                asset,
                nonce: 1,
                recipient: signer,
                amount: ExtUnit::from_units(20),
            }),
        );
        checkpoints.push(ledger.clone());
        let inputs = ledger
            .state
            .extensions
            .assets
            .shares
            .keys()
            .copied()
            .collect();
        commit_call(
            &mut ledger,
            &seed,
            AssetCall::Transfer(Transfer {
                asset,
                inputs,
                outputs: vec![AssetOutput::new(signer, ExtUnit::from_units(60))],
            }),
        );
        checkpoints.push(ledger.clone());
        let inputs = ledger
            .state
            .extensions
            .assets
            .shares
            .keys()
            .copied()
            .collect();
        commit_call(
            &mut ledger,
            &seed,
            AssetCall::Burn(Burn {
                asset,
                inputs,
                amount: ExtUnit::from_units(10),
                output: ExtUnit::from_units(50),
            }),
        );
        assert_eq!(
            ledger.state.extensions.assets.records[&asset].supply,
            ExtUnit::from_units(50)
        );
        let mut replay = genesis::genesis_ledger().unwrap();
        for block in ledger.chain.blocks().skip(1) {
            replay
                .apply_validated_block(validate_candidate_for_apply(block, &replay.chain).unwrap())
                .unwrap();
        }
        assert_eq!(replay, ledger);
        for checkpoint in checkpoints.into_iter().rev() {
            replay.rollback_tip().unwrap();
            assert_eq!(replay, checkpoint);
        }
    }
}
