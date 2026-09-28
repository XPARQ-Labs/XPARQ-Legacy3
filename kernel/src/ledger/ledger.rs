use std::{collections::BTreeMap, error::Error as StdError, fmt};

use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{BlockHash, HashDomain, StateRoot, canonical_bytes, domain};

use crate::{
    blockchain::{Block, Chain, ChainError},
    common::Height,
    consensus::{
        ApplyBlockState, CoinInputState, ConsensusError, EmissionError, TransactionConsensusError,
        TransactionStateView, ValidatedBlock, validate_emission, validate_transaction,
    },
    ledger::{CoinUtxo, LedgerState, SpendRollbackJournal, StateError, StateRollbackJournal},
    monetary::{
        asset::{AssetContract, Unit},
        coin::{CoinShare, Zeno},
    },
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
    TransactionApplied,
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
        self.execute_block_with_checkpoint(block, |_| Ok(()))
    }

    fn execute_block_with_checkpoint(
        &self,
        block: &Block,
        mut checkpoint: impl FnMut(BlockTransitionPoint) -> Result<(), LedgerError>,
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
            let id = CoinShare::from_emission_origin(&emission.origin().0);
            state.utxos.insert_coin(
                id,
                CoinUtxo {
                    amount: emission.miner_emission(),
                    owner: emission.recipient(),
                },
            )?;
            checkpoint(BlockTransitionPoint::EmissionCreated)?;
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
                asset: None,
            });
        }

        for transaction in block.transactions() {
            let validated =
                validate_transaction(transaction.clone(), chain_context, height.0, &state)?;
            journals.push(state.apply_validated_transaction(
                &validated,
                block.miner_address(),
                chain_context,
            )?);
            checkpoint(BlockTransitionPoint::TransactionApplied)?;
        }

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
    fn coin(&self, id: CoinShare) -> Option<CoinInputState> {
        self.utxos.coin(&id).map(|coin| CoinInputState {
            amount: coin.amount,
            owner: coin.owner,
        })
    }

    fn asset_share(
        &self,
        id: crate::monetary::asset::Share,
    ) -> Option<crate::monetary::asset::AssetShare> {
        self.utxos.asset(&id).copied()
    }

    fn asset_spend_created_state_weight(
        &self,
        intent: &crate::transaction::SpendIntent,
    ) -> Result<u64, crate::monetary::asset::AssetError> {
        let (asset, inputs, outputs) = intent
            .asset_parts()
            .ok_or(crate::monetary::asset::AssetError::InvalidProgram)?;

        self.assets
            .account_transfer_created_state_weight(&self.utxos, asset, inputs, outputs)
    }

    fn asset_transition_created_state_weight(
        &self,
        call: &crate::transaction::AssetIntent,
        genesis_hash: [u8; 32],
    ) -> Result<u64, crate::monetary::asset::AssetError> {
        self.assets
            .validate_transition(&self.utxos, call, genesis_hash)?;

        call.created_state_weight()
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

        let mut asset_totals = BTreeMap::<AssetContract, Unit>::new();
        for (_, share) in self.utxos.assets() {
            if self.assets.record(share.asset).is_none() {
                return Err(LedgerError::UnknownAssetShare);
            }
            let total = asset_totals.entry(share.asset).or_insert(Unit::ZERO);
            *total = total
                .checked_add(share.amount)
                .ok_or(LedgerError::SupplyOverflow)?;
        }
        for (&asset, record) in &self.assets.assets {
            if record.total_minted > record.metadata.max_supply
                || record.total_minted.checked_sub(record.total_burned) != Some(record.supply)
                || asset_totals.get(&asset).copied().unwrap_or(Unit::ZERO) != record.supply
            {
                return Err(LedgerError::AssetSupplyMismatch);
            }
        }
        Ok(())
    }

    pub(crate) fn application_state_root(&self) -> Result<StateRoot, LedgerError> {
        if self.assets.is_empty()
            && self.utxos.is_empty()
            && self.coin.total_mined.is_zero()
            && self.coin.total_burned.is_zero()
        {
            return Ok(StateRoot::ZERO);
        }

        let state = canonical_bytes(&(&self.utxos, &self.coin, &self.assets))?;

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
            ledger.execute_block_with_checkpoint(&block, |point| {
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
}
