use std::{
    collections::HashSet,
    io::{Error as IoError, ErrorKind, Read, Write},
};

use borsh::{BorshDeserialize, BorshSerialize};

use crypto::{
    Address, BlockHash, Hash, HashDomain, MerkleHash, PreviousHash, StateRoot, canonical_bytes,
    domain,
};

use crate::{
    blockchain::merkle::{MerkleInclusionProof, merkle_root},
    common::{Height, Nonce},
    error::{BlockError, CodecError},
    monetary::coin::Zeno,
    transaction::{MAX_TRANSACTION_SIZE, Transaction},
};

pub const MAX_BLOCK_SIZE: usize = 2 * 1024 * 1024;
pub const MAX_BLOCK_TRANSACTIONS: usize = 4096;
pub const GENESIS_TARGET_BITS: u32 = 0x207f_ffff;

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Header {
    pub previous_hash: PreviousHash,
    pub merkle_root: MerkleHash,
    pub state_root: StateRoot,
    pub target_bits: u32,
    /// Canonical serialized block size plus any ledger execution reservation.
    pub block_weight: u32,
    pub nonce: Nonce,
}

impl Header {
    pub const fn new(
        previous_hash: PreviousHash,
        merkle_root: MerkleHash,
        state_root: StateRoot,
        target_bits: u32,
        block_weight: u32,
        nonce: Nonce,
    ) -> Self {
        Self {
            previous_hash,
            merkle_root,
            state_root,
            target_bits,
            block_weight,
            nonce,
        }
    }

    pub fn hash(&self) -> Result<BlockHash, CodecError> {
        block_header_hash(self)
    }
}

#[derive(BorshSerialize, Clone, Debug, PartialEq, Eq)]
pub struct Body {
    pub emission: Option<Emission>,
    pub transactions: Vec<Transaction>,
}

#[derive(BorshSerialize, Clone, Debug, PartialEq, Eq)]
pub struct Block {
    pub header: Header,
    pub height: Height,
    pub body: Body,
}

// Bounded decoding avoids trusting a serialized Vec length before the block-size
// limit and block-local invariants have been checked.
impl BorshDeserialize for Block {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        let header = Header::deserialize_reader(reader)?;
        let height = Height::deserialize_reader(reader)?;
        let emission = Option::<Emission>::deserialize_reader(reader)?;
        let transactions = deserialize_block_transactions(reader)?;

        Ok(Self {
            header,
            height,
            body: Body {
                emission,
                transactions,
            },
        })
    }
}

fn deserialize_block_transactions<R: Read>(reader: &mut R) -> std::io::Result<Vec<Transaction>> {
    let length = u32::deserialize_reader(reader)? as usize;
    if length > MAX_BLOCK_TRANSACTIONS {
        return Err(IoError::new(
            ErrorKind::InvalidData,
            "block transaction count exceeds limit",
        ));
    }

    let mut transactions = Vec::new();

    transactions
        .try_reserve(length.min(64))
        .map_err(|_| IoError::new(ErrorKind::OutOfMemory, "block allocation failed"))?;

    for _ in 0..length {
        let mut limited = TransactionReader {
            inner: reader,
            remaining: MAX_TRANSACTION_SIZE,
        };
        transactions.push(Transaction::deserialize_reader(&mut limited)?);
    }

    Ok(transactions)
}

struct TransactionReader<'a, R> {
    inner: &'a mut R,
    remaining: usize,
}

impl<R: Read> Read for TransactionReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.remaining == 0 && !bytes.is_empty() {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "transaction exceeds size limit",
            ));
        }
        let allowed = bytes.len().min(self.remaining);
        let read = self.inner.read(&mut bytes[..allowed])?;
        self.remaining -= read;
        Ok(read)
    }
}

struct CappedCounter {
    count: usize,
    maximum: usize,
}

impl Write for CappedCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let next = self
            .count
            .checked_add(bytes.len())
            .ok_or_else(|| IoError::new(ErrorKind::InvalidData, "block exceeds size limit"))?;
        if next > self.maximum {
            return Err(IoError::new(
                ErrorKind::InvalidData,
                "block exceeds size limit",
            ));
        }
        self.count = next;
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug, PartialEq, Eq, Hash)]
pub struct Emission {
    pub to: Address,
    pub subsidy: Zeno,
}

impl Emission {
    pub const fn new(to: Address, subsidy: Zeno) -> Self {
        Self { to, subsidy }
    }

    pub fn hash(&self) -> Result<Hash, CodecError> {
        let bytes = canonical_bytes(self).map_err(|_| CodecError::EncodeFailed)?;
        Ok(domain(HashDomain::Emission, &bytes))
    }
}

impl Block {
    pub fn emission(&self) -> Option<&Emission> {
        self.body.emission.as_ref()
    }

    pub fn transactions(&self) -> &[Transaction] {
        &self.body.transactions
    }

    /// Compatibility accessor retained for callers that still use coinbase naming.
    pub fn coinbase(&self) -> Option<&Emission> {
        self.emission()
    }

    pub fn genesis() -> Result<Self, CodecError> {
        Self::from_protocol_transactions(
            Height(0),
            PreviousHash::ZERO,
            GENESIS_TARGET_BITS,
            Nonce(0),
            None,
            vec![],
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn from_protocol_transactions(
        height: Height,
        previous_hash: impl Into<PreviousHash>,
        target_bits: u32,
        nonce: Nonce,
        emission: Option<Emission>,
        transactions: Vec<Transaction>,
    ) -> Result<Self, CodecError> {
        let previous_hash = previous_hash.into();
        let merkle_root = calculate_merkle_root(emission.as_ref(), &transactions)?;

        let mut block = Self {
            header: Header::new(
                previous_hash,
                merkle_root,
                StateRoot::ZERO,
                target_bits,
                0,
                nonce,
            ),
            height,
            body: Body {
                emission,
                transactions,
            },
        };

        block.refresh_block_weight()?;
        Ok(block)
    }

    /// Validates only deterministic rules that depend on the block itself.
    /// Signatures, values, state burn, and state root execution remain
    /// consensus/ledger responsibilities.
    pub fn validate_structure(&self) -> Result<(), BlockError> {
        if self.body.transactions.len() > MAX_BLOCK_TRANSACTIONS {
            return Err(BlockError::InvalidTransaction);
        }
        if self.is_genesis() {
            if self.body.emission.is_some() {
                return Err(BlockError::UnexpectedEmission);
            }
            if self.transaction_count() != 0 {
                return Err(BlockError::InvalidTransaction);
            }
        } else if self.body.emission.is_none() {
            return Err(BlockError::MissingEmission);
        }

        if !transactions_are_structurally_valid(&self.body.transactions) {
            return Err(BlockError::InvalidTransaction);
        }

        if self.header.block_weight as usize > MAX_BLOCK_SIZE {
            return Err(BlockError::BlockTooHeavy);
        }

        let mut counter = CappedCounter {
            count: 0,
            maximum: MAX_BLOCK_SIZE,
        };
        self.serialize(&mut counter)
            .map_err(|_| BlockError::BlockTooHeavy)?;
        let serialized_weight = counter.count;
        if (self.header.block_weight as usize) < serialized_weight {
            return Err(BlockError::InvalidBlockWeight);
        }

        if self.body.transactions.iter().any(|transaction| {
            canonical_bytes(transaction).map_or(true, |bytes| bytes.len() > MAX_TRANSACTION_SIZE)
        }) {
            return Err(BlockError::InvalidTransaction);
        }

        if has_duplicate_transactions(&self.body.transactions)? {
            return Err(BlockError::DuplicateTransaction);
        }

        if self.header.merkle_root
            != calculate_merkle_root(self.body.emission.as_ref(), &self.body.transactions)?
        {
            return Err(BlockError::InvalidMerkleRoot);
        }

        Ok(())
    }

    pub fn hash(&self) -> Result<BlockHash, CodecError> {
        self.header.hash()
    }

    pub const fn height(&self) -> Height {
        self.height
    }

    pub const fn previous_hash(&self) -> PreviousHash {
        self.header.previous_hash
    }

    pub fn miner_address(&self) -> Address {
        self.body
            .emission
            .as_ref()
            .map(|emission| emission.to)
            .unwrap_or(Address([0; crypto::ADDRESS_SIZE]))
    }

    pub const fn state_root(&self) -> StateRoot {
        self.header.state_root
    }

    pub fn set_state_root(&mut self, state_root: impl Into<StateRoot>) {
        self.header.state_root = state_root.into();
    }

    pub fn set_block_weight(&mut self, block_weight: u32) {
        self.header.block_weight = block_weight;
    }

    pub const fn target_bits(&self) -> u32 {
        self.header.target_bits
    }

    pub const fn block_weight(&self) -> u32 {
        self.header.block_weight
    }

    pub fn transaction_count(&self) -> usize {
        self.body.transactions.len()
    }

    pub fn is_genesis(&self) -> bool {
        self.height.0 == 0
    }

    pub fn serialized_size(&self) -> Result<usize, CodecError> {
        Ok(self.to_bytes()?.len())
    }

    pub fn weight(&self) -> Result<usize, CodecError> {
        self.serialized_size()
    }

    pub fn refresh_block_weight(&mut self) -> Result<(), CodecError> {
        self.header.block_weight = 0;
        let weight = self.weight()?;
        self.header.block_weight = u32::try_from(weight).map_err(|_| CodecError::EncodeFailed)?;
        Ok(())
    }

    pub fn to_bytes(&self) -> Result<Vec<u8>, CodecError> {
        block_bytes(self)
    }

    pub fn calculate_merkle_root(&self) -> Result<MerkleHash, CodecError> {
        calculate_merkle_root(self.body.emission.as_ref(), &self.body.transactions)
    }

    pub fn transaction_inclusion_proof(
        &self,
        transaction_index: usize,
    ) -> Result<MerkleInclusionProof, CodecError> {
        if transaction_index >= self.body.transactions.len() {
            return Err(CodecError::InvalidBlock);
        }

        let leaves = merkle_leaves(self.body.emission.as_ref(), &self.body.transactions)?;
        let leaf_index = usize::from(self.body.emission.is_some()) + transaction_index;

        MerkleInclusionProof::create(&leaves, leaf_index, HashDomain::MerkleNode)
            .ok_or(CodecError::InvalidBlock)
    }

    /// Backward-compatible plural name used by older callers.
    pub fn transaction_inclusion_proofs(
        &self,
        transaction_index: usize,
    ) -> Result<MerkleInclusionProof, CodecError> {
        self.transaction_inclusion_proof(transaction_index)
    }

    pub fn refresh_merkle_root(&mut self) -> Result<(), CodecError> {
        self.refresh_commitments()
    }

    pub fn refresh_commitments(&mut self) -> Result<(), CodecError> {
        self.header.merkle_root = self.calculate_merkle_root()?;
        self.refresh_block_weight()?;
        Ok(())
    }

    pub fn push_transaction(&mut self, transaction: Transaction) -> Result<(), CodecError> {
        self.body.transactions.push(transaction);
        self.refresh_commitments()
    }
}

fn merkle_leaves(
    emission: Option<&Emission>,
    transactions: &[Transaction],
) -> Result<Vec<Hash>, CodecError> {
    let mut leaves = Vec::with_capacity(usize::from(emission.is_some()) + transactions.len());

    if let Some(emission) = emission {
        leaves.push(emission.hash()?);
    }

    for transaction in transactions {
        leaves.push(Hash(
            transaction.id().map_err(|_| CodecError::EncodeFailed)?,
        ));
    }

    Ok(leaves)
}

fn calculate_merkle_root(
    emission: Option<&Emission>,
    transactions: &[Transaction],
) -> Result<MerkleHash, CodecError> {
    if emission.is_none() && transactions.is_empty() {
        return Ok(MerkleHash::ZERO);
    }

    let leaves = merkle_leaves(emission, transactions)?;
    merkle_root(&leaves, HashDomain::MerkleNode)
        .map(|root| MerkleHash(root.into_bytes()))
        .ok_or(CodecError::InvalidBlock)
}

fn has_duplicate_transactions(transactions: &[Transaction]) -> Result<bool, CodecError> {
    let mut seen = HashSet::with_capacity(transactions.len());

    for transaction in transactions {
        let id = Hash(transaction.id().map_err(|_| CodecError::EncodeFailed)?);
        if !seen.insert(id) {
            return Ok(true);
        }
    }

    Ok(false)
}

fn transactions_are_structurally_valid(transactions: &[Transaction]) -> bool {
    transactions
        .iter()
        .all(|transaction| transaction.validate_structure().is_ok())
}

pub fn block_header_bytes(header: &Header) -> Result<Vec<u8>, CodecError> {
    canonical_bytes(header).map_err(|_| CodecError::EncodeFailed)
}

pub fn block_bytes(block: &Block) -> Result<Vec<u8>, CodecError> {
    canonical_bytes(block).map_err(|_| CodecError::EncodeFailed)
}

pub fn block_header_hash(header: &Header) -> Result<BlockHash, CodecError> {
    Ok(BlockHash(
        domain(HashDomain::Header, &block_header_bytes(header)?).into_bytes(),
    ))
}

pub fn decode_block(bytes: &[u8]) -> Result<Block, CodecError> {
    if bytes.len() > MAX_BLOCK_SIZE {
        return Err(CodecError::InvalidBlock);
    }

    let block = Block::try_from_slice(bytes).map_err(|_| CodecError::InvalidBlock)?;
    block
        .validate_structure()
        .map_err(|_| CodecError::InvalidBlock)?;
    Ok(block)
}

#[cfg(test)]
mod p3e_replay_tests {
    use super::*;

    use crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key};

    use crate::{
        common::ChainContext,
        monetary::coin::{CoinOutput, CoinShare, Zeno},
        transaction::{
            AccountAuthorization, AuthorizedProgramTransaction, AuthorizedTransaction,
            CoinTransition, program_transaction_commitment,
        },
    };

    fn duplicate_fixture() -> AuthorizedTransaction {
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([0x51; 32]));

        let signer = address_from_public_key(&seed.public_key());
        let chain = ChainContext::new([0x71; crypto::HASH_SIZE]);

        let intent = CoinTransition::coin(
            signer,
            vec![CoinShare::from_bytes([0x31; crypto::HASH16_SIZE])],
            vec![CoinOutput::new(signer, Zeno::from_zeno(1))],
        )
        .expect("valid structural spend fixture");

        let call = extension::coin_program::transfer_call();
        let commitment = program_transaction_commitment(signer, &call, &intent, chain)
            .expect("authorization commitment");

        AuthorizedTransaction::Program(Box::new(AuthorizedProgramTransaction {
            signer,
            call,
            payment: intent,
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        }))
    }

    #[test]
    fn duplicate_transaction_id_is_rejected_by_block_structure() {
        let transaction = duplicate_fixture();

        assert_eq!(
            transaction.transaction_id().unwrap(),
            transaction.clone().transaction_id().unwrap()
        );

        let miner = Address([0x22; crypto::ADDRESS_SIZE]);

        let block = Block::from_protocol_transactions(
            Height(1),
            PreviousHash::ZERO,
            GENESIS_TARGET_BITS,
            Nonce(0),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![transaction.clone(), transaction],
        )
        .expect("block construction itself may contain duplicate txs");

        assert!(matches!(
            block.validate_structure(),
            Err(BlockError::DuplicateTransaction)
        ));
    }

    #[test]
    fn oversized_block_transaction_count_prefix_is_rejected() {
        let mut bytes = Block::genesis().unwrap().to_bytes().unwrap();
        let count = bytes.len() - 4;
        bytes[count..].copy_from_slice(&((MAX_BLOCK_TRANSACTIONS + 1) as u32).to_le_bytes());
        assert!(decode_block(&bytes).is_err());
    }

    #[test]
    fn block_size_counter_stops_before_exceeding_cap() {
        let mut counter = CappedCounter {
            count: 0,
            maximum: 4,
        };
        counter.write_all(&[1, 2, 3, 4]).unwrap();
        assert!(counter.write_all(&[5]).is_err());
        assert_eq!(counter.count, 4);
    }
}
