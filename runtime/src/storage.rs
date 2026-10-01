use std::{
    fs,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, OnceLock},
};

use borsh::{BorshDeserialize, BorshSerialize};
use kernel::{
    common::{ChainContext, Height},
    monetary::coin::CoinShare,
    transaction::{AuthorizedTransaction, TransactionId},
};
use redb::{Database, ReadableDatabase, ReadableTable, ReadableTableMetadata, TableDefinition};

const DATABASE_FILE: &str = "xparq.redb";

// Reset-chain generation 3 stores owners inside UTXOs and unified asset records.
const SCHEMA_VERSION: u32 = 3;

const META: TableDefinition<&str, &[u8]> = TableDefinition::new("metadata");
const BLOCKS: TableDefinition<u64, &[u8]> = TableDefinition::new("canonical_blocks");
const MEMPOOL: TableDefinition<u64, &[u8]> = TableDefinition::new("mempool");
const SNAPSHOTS: TableDefinition<u64, &[u8]> = TableDefinition::new("ledger_snapshots");
const AUXILIARY: TableDefinition<&str, &[u8]> = TableDefinition::new("auxiliary");

// Rebuildable, non-consensus runtime indexes.
const BLOCK_HASH_INDEX: TableDefinition<&[u8], u64> = TableDefinition::new("block_hash_index");

const TX_INDEX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("transaction_index");

const INDEX_TIP_HEIGHT_KEY: &str = "canonical_index_tip_height";
const INDEX_TIP_HASH_KEY: &str = "canonical_index_tip_hash";

const ADDRESS_ACTIVITY_INDEX: TableDefinition<&[u8], &[u8]> =
    TableDefinition::new("address_activity_index");
const COIN_ORIGIN_INDEX: TableDefinition<&[u8], &[u8]> = TableDefinition::new("coin_origin_index");
const COIN_ORIGIN_INDEX_VERSION_KEY: &str = "coin_origin_index_version";

/// Rebuildable canonical-chain metadata, separate from consensus UTXO state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct CoinOrigin {
    pub created_at: Option<TransactionId>,
    pub created_in: Height,
}

pub const ADDRESS_ACTIVITY_CURSOR_SIZE: usize = 8 + 1 + 8;

#[derive(Debug)]
pub struct AddressActivityPage {
    pub entries: Vec<(u64, Option<u64>)>,
    pub next_cursor: Option<[u8; ADDRESS_ACTIVITY_CURSOR_SIZE]>,
}

const EMPTY_INDEX_VALUE: &[u8] = &[];

#[derive(Debug, Clone, Copy)]
pub struct StoredAddressActivity {
    pub address: [u8; kernel::crypto::ADDRESS_SIZE],
    pub transaction_index: Option<u64>,
}

#[derive(Debug)]
pub struct StoredCanonicalBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub bytes: Vec<u8>,
    pub transactions: Vec<[u8; 32]>,
    pub activities: Vec<StoredAddressActivity>,
}

#[derive(Debug)]
pub struct CanonicalIndexBlock {
    pub height: u64,
    pub hash: [u8; 32],
    pub bytes: Vec<u8>,
    pub transactions: Vec<[u8; 32]>,
    pub activities: Vec<StoredAddressActivity>,
}

pub fn read_address_activities_page(
    directory: &Path,
    address: [u8; kernel::crypto::ADDRESS_SIZE],
    before: Option<[u8; ADDRESS_ACTIVITY_CURSOR_SIZE]>,
    limit: usize,
) -> Result<AddressActivityPage, String> {
    if limit == 0 {
        return Err("address activity page limit must be greater than zero".into());
    }

    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin address activity page read: {error}"))?;

    let table = transaction
        .open_table(ADDRESS_ACTIVITY_INDEX)
        .map_err(|error| format!("open address activity index table: {error}"))?;

    let key_size = kernel::crypto::ADDRESS_SIZE + ADDRESS_ACTIVITY_CURSOR_SIZE;

    let mut start = Vec::with_capacity(key_size);
    start.extend_from_slice(&address);
    start.resize(key_size, 0x00);

    let mut end = Vec::with_capacity(key_size);
    end.extend_from_slice(&address);

    match before {
        Some(cursor) => {
            end.extend_from_slice(&cursor);
        }

        None => {
            end.resize(key_size, 0xff);
        }
    }

    let height_start = kernel::crypto::ADDRESS_SIZE;
    let height_end = height_start + 8;
    let kind_index = height_end;
    let transaction_start = kind_index + 1;
    let transaction_end = transaction_start + 8;

    let mut selected = Vec::with_capacity(limit.saturating_add(1));

    let entries = table
        .range(start.as_slice()..=end.as_slice())
        .map_err(|error| format!("range address activity page: {error}"))?;

    for entry in entries.rev() {
        let (key, _) =
            entry.map_err(|error| format!("read address activity page entry: {error}"))?;

        let key = key.value();

        if key.len() != key_size {
            return Err("stored address activity key has invalid length".into());
        }

        if key[..kernel::crypto::ADDRESS_SIZE] != address {
            return Err("stored address activity key has invalid address prefix".into());
        }

        let cursor: [u8; ADDRESS_ACTIVITY_CURSOR_SIZE] = key[kernel::crypto::ADDRESS_SIZE..]
            .try_into()
            .map_err(|_| "stored address activity cursor is invalid")?;

        // `before` is exclusive. The previous page's final entry
        // must not appear again.
        if before.is_some_and(|before| before == cursor) {
            continue;
        }

        let height = u64::from_be_bytes(
            key[height_start..height_end]
                .try_into()
                .map_err(|_| "stored address activity height is invalid")?,
        );

        let transaction_index = match key[kind_index] {
            0 => None,

            1 => Some(u64::from_be_bytes(
                key[transaction_start..transaction_end]
                    .try_into()
                    .map_err(|_| "stored address activity transaction index is invalid")?,
            )),

            _ => {
                return Err("stored address activity kind is invalid".into());
            }
        };

        selected.push(((height, transaction_index), cursor));

        if selected.len() > limit {
            break;
        }
    }

    let has_more = selected.len() > limit;

    if has_more {
        selected.pop();
    }

    let next_cursor = if has_more {
        selected.last().map(|(_, cursor)| *cursor)
    } else {
        None
    };

    let entries = selected.into_iter().map(|(entry, _)| entry).collect();

    Ok(AddressActivityPage {
        entries,
        next_cursor,
    })
}

fn encode_address_activity_key(
    address: [u8; kernel::crypto::ADDRESS_SIZE],
    height: u64,
    transaction_index: Option<u64>,
) -> Vec<u8> {
    let mut key = Vec::with_capacity(kernel::crypto::ADDRESS_SIZE + 8 + 1 + 8);

    key.extend_from_slice(&address);
    key.extend_from_slice(&height.to_be_bytes());

    match transaction_index {
        None => {
            key.push(0);
            key.extend_from_slice(&0_u64.to_be_bytes());
        }

        Some(transaction_index) => {
            key.push(1);
            key.extend_from_slice(&transaction_index.to_be_bytes());
        }
    }

    key
}

fn encode_transaction_location(height: u64, transaction_index: u64) -> [u8; 16] {
    let mut bytes = [0_u8; 16];

    bytes[..8].copy_from_slice(&height.to_le_bytes());
    bytes[8..].copy_from_slice(&transaction_index.to_le_bytes());

    bytes
}

type CachedDatabase = Option<(PathBuf, Arc<Database>)>;

static DATABASE: OnceLock<Mutex<CachedDatabase>> = OnceLock::new();

pub fn canonical_index_tip(directory: &Path) -> Result<Option<(u64, [u8; 32])>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin canonical index metadata read: {error}"))?;

    let metadata = transaction
        .open_table(META)
        .map_err(|error| format!("open metadata table: {error}"))?;

    let height = metadata
        .get(INDEX_TIP_HEIGHT_KEY)
        .map_err(|error| format!("read canonical index tip height: {error}"))?
        .map(|value| value.value().to_vec());

    let hash = metadata
        .get(INDEX_TIP_HASH_KEY)
        .map_err(|error| format!("read canonical index tip hash: {error}"))?
        .map(|value| value.value().to_vec());

    let origin_version = metadata
        .get(COIN_ORIGIN_INDEX_VERSION_KEY)
        .map_err(|error| format!("read coin origin index version: {error}"))?
        .map(|value| value.value().to_vec());

    if origin_version.as_deref() != Some(&[1_u8][..]) {
        return Ok(None);
    }

    let (Some(height), Some(hash)) = (height, hash) else {
        return Ok(None);
    };

    let Ok(height): Result<[u8; 8], _> = height.try_into() else {
        return Ok(None);
    };

    let Ok(hash): Result<[u8; 32], _> = hash.try_into() else {
        return Ok(None);
    };

    Ok(Some((u64::from_le_bytes(height), hash)))
}

pub fn read_block_height_by_hash(directory: &Path, hash: [u8; 32]) -> Result<Option<u64>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin block hash index read: {error}"))?;

    let table = transaction
        .open_table(BLOCK_HASH_INDEX)
        .map_err(|error| format!("open block hash index table: {error}"))?;

    Ok(table
        .get(hash.as_slice())
        .map_err(|error| format!("read block hash index: {error}"))?
        .map(|height| height.value()))
}

pub fn read_transaction_location(
    directory: &Path,
    hash: [u8; 32],
) -> Result<Option<(u64, u64)>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin transaction index read: {error}"))?;

    let table = transaction
        .open_table(TX_INDEX)
        .map_err(|error| format!("open transaction index table: {error}"))?;

    let Some(value) = table
        .get(hash.as_slice())
        .map_err(|error| format!("read transaction index: {error}"))?
    else {
        return Ok(None);
    };

    let bytes: [u8; 16] = value
        .value()
        .try_into()
        .map_err(|_| "stored transaction index location is invalid")?;

    let height = u64::from_le_bytes(
        bytes[..8]
            .try_into()
            .map_err(|_| "stored transaction height is invalid")?,
    );

    let transaction_index = u64::from_le_bytes(
        bytes[8..]
            .try_into()
            .map_err(|_| "stored transaction position is invalid")?,
    );

    Ok(Some((height, transaction_index)))
}

pub fn read_coin_origin(directory: &Path, share: CoinShare) -> Result<Option<CoinOrigin>, String> {
    let database = open(directory)?;
    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin coin origin read: {error}"))?;
    let table = transaction
        .open_table(COIN_ORIGIN_INDEX)
        .map_err(|error| format!("open coin origin index: {error}"))?;
    let Some(value) = table
        .get(share.as_bytes().as_slice())
        .map_err(|error| format!("read coin origin: {error}"))?
    else {
        return Ok(None);
    };
    CoinOrigin::try_from_slice(value.value())
        .map(Some)
        .map_err(|error| format!("decode coin origin: {error}"))
}

fn decode_canonical_block(bytes: &[u8]) -> Result<kernel::blockchain::Block, String> {
    kernel::crypto::canonical_decode(bytes)
        .map_err(|error| format!("decode canonical block for coin origins: {error}"))
}

fn genesis_context(bytes: &[u8]) -> Result<ChainContext, String> {
    let genesis = decode_canonical_block(bytes)?;
    if !genesis.is_genesis() {
        return Err("first canonical block is not genesis".into());
    }
    Ok(ChainContext::new(
        genesis
            .hash()
            .map_err(|error| error.to_string())?
            .into_bytes(),
    ))
}

fn block_coin_origins(
    block: &kernel::blockchain::Block,
    chain: ChainContext,
) -> Result<Vec<(CoinShare, CoinOrigin)>, String> {
    let mut origins = Vec::new();
    let height = block.height();
    if !block.is_genesis() {
        let origin = kernel::consensus::emission_origin(block)
            .map_err(|error| format!("derive emission coin origin: {error}"))?;
        origins.push((
            CoinShare::from_emission_origin(&origin.into_bytes()),
            CoinOrigin {
                created_at: None,
                created_in: height,
            },
        ));
    }
    for transaction in block.transactions() {
        let spend = match transaction {
            AuthorizedTransaction::Spend(tx) => &tx.intent,
            AuthorizedTransaction::Asset(tx) => &tx.payment,
            AuthorizedTransaction::Program(tx) => &tx.payment,
        };
        let (_, outputs) = spend
            .coin_parts()
            .ok_or("transaction has no coin outputs")?;
        let commitment = spend
            .semantic_commitment(chain)
            .map_err(|error| format!("derive coin output origin: {error}"))?;
        let transaction_id = transaction
            .transaction_id()
            .map_err(|error| format!("derive transaction origin: {error}"))?;
        let output_count = outputs.len() + usize::from(!spend.charges.miner_fee.is_zero());
        for index in 0..output_count {
            let index = u32::try_from(index).map_err(|_| "coin output index exceeds u32")?;
            origins.push((
                CoinShare::from_output(commitment.as_bytes(), index),
                CoinOrigin {
                    created_at: Some(transaction_id),
                    created_in: height,
                },
            ));
        }
    }
    Ok(origins)
}

fn insert_coin_origins(
    table: &mut redb::Table<'_, &[u8], &[u8]>,
    block: &kernel::blockchain::Block,
    chain: ChainContext,
) -> Result<(), String> {
    for (share, origin) in block_coin_origins(block, chain)? {
        if table
            .get(share.as_bytes().as_slice())
            .map_err(|error| format!("inspect coin origin: {error}"))?
            .is_some()
        {
            return Err(format!("duplicate coin origin {share}"));
        }
        let bytes =
            borsh::to_vec(&origin).map_err(|error| format!("encode coin origin: {error}"))?;
        table
            .insert(share.as_bytes().as_slice(), bytes.as_slice())
            .map_err(|error| format!("insert coin origin: {error}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod coin_origin_tests {
    use super::*;
    use kernel::{
        blockchain::{Block, Emission, GENESIS_TARGET_BITS},
        common::Nonce,
        crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key},
        monetary::coin::{CoinOutput, Zeno},
        transaction::{AccountAuthorization, AccountIntent, AuthorizedAccountIntent, SpendIntent},
    };

    #[test]
    fn transaction_outputs_and_miner_fee_point_to_transaction_id() {
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([5; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let chain = ChainContext::new([7; kernel::crypto::HASH_SIZE]);
        let mut spend = SpendIntent::coin(
            signer,
            vec![CoinShare::from_bytes([1; kernel::crypto::HASH16_SIZE])],
            vec![CoinOutput::new(signer, Zeno::from_zeno(10))],
        )
        .unwrap();
        spend.charges.miner_fee = Zeno::from_zeno(1);
        let commitment = spend.principal_commitment(chain).unwrap();
        let tx = AuthorizedTransaction::Spend(Box::new(AuthorizedAccountIntent {
            intent: spend.clone(),
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        }));
        let tx_id = tx.transaction_id().unwrap();
        let block = Block::from_protocol_transactions(
            Height(1),
            kernel::crypto::PreviousHash([2; kernel::crypto::HASH_SIZE]),
            GENESIS_TARGET_BITS,
            Nonce(0),
            Some(Emission::new(
                signer,
                kernel::consensus::expected_emission_for_height(Height(1)),
            )),
            vec![tx],
        )
        .unwrap();
        let origins = block_coin_origins(&block, chain).unwrap();
        let spend_commitment = spend.semantic_commitment(chain).unwrap();
        for index in 0..2 {
            let share = CoinShare::from_output(spend_commitment.as_bytes(), index);
            assert_eq!(
                origins
                    .iter()
                    .find(|(id, _)| *id == share)
                    .map(|(_, origin)| *origin),
                Some(CoinOrigin {
                    created_at: Some(tx_id),
                    created_in: Height(1)
                }),
            );
        }
    }
}

#[cfg(test)]
pub fn read_address_activities(
    directory: &Path,
    address: [u8; kernel::crypto::ADDRESS_SIZE],
) -> Result<Vec<(u64, Option<u64>)>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin address activity index read: {error}"))?;

    let table = transaction
        .open_table(ADDRESS_ACTIVITY_INDEX)
        .map_err(|error| format!("open address activity index table: {error}"))?;

    const ACTIVITY_SUFFIX_SIZE: usize = 8 + 1 + 8;

    let key_size = kernel::crypto::ADDRESS_SIZE + ACTIVITY_SUFFIX_SIZE;

    let mut start = Vec::with_capacity(key_size);
    start.extend_from_slice(&address);
    start.resize(key_size, 0x00);

    let mut end = Vec::with_capacity(key_size);
    end.extend_from_slice(&address);
    end.resize(key_size, 0xff);

    let height_start = kernel::crypto::ADDRESS_SIZE;
    let height_end = height_start + 8;
    let kind_index = height_end;
    let transaction_start = kind_index + 1;
    let transaction_end = transaction_start + 8;

    let mut activities = Vec::new();

    for entry in table
        .range(start.as_slice()..=end.as_slice())
        .map_err(|error| format!("range address activity index: {error}"))?
    {
        let (key, _) =
            entry.map_err(|error| format!("read address activity index entry: {error}"))?;

        let key = key.value();

        if key.len() != key_size {
            return Err("stored address activity key has invalid length".into());
        }

        if key[..kernel::crypto::ADDRESS_SIZE] != address {
            return Err("stored address activity key has invalid address prefix".into());
        }

        let height = u64::from_be_bytes(
            key[height_start..height_end]
                .try_into()
                .map_err(|_| "stored address activity height is invalid")?,
        );

        let transaction_index = match key[kind_index] {
            0 => None,

            1 => Some(u64::from_be_bytes(
                key[transaction_start..transaction_end]
                    .try_into()
                    .map_err(|_| "stored address activity transaction index is invalid")?,
            )),

            _ => {
                return Err("stored address activity kind is invalid".into());
            }
        };

        activities.push((height, transaction_index));
    }

    Ok(activities)
}

pub fn rebuild_canonical_indexes(
    directory: &Path,
    blocks: &[CanonicalIndexBlock],
) -> Result<(), String> {
    let tip = blocks
        .last()
        .ok_or("cannot rebuild canonical indexes from an empty chain")?;
    let chain = genesis_context(&blocks[0].bytes)?;
    let canonical_blocks = blocks
        .iter()
        .map(|block| decode_canonical_block(&block.bytes))
        .collect::<Result<Vec<_>, _>>()?;
    for (actual, indexed) in canonical_blocks.iter().zip(blocks) {
        if actual.height().0 != indexed.height
            || actual.hash().map_err(|error| error.to_string())?.0 != indexed.hash
        {
            return Err("canonical blocks changed before index rebuild".into());
        }
    }

    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin canonical index rebuild: {error}"))?;

    {
        let mut block_index = transaction
            .open_table(BLOCK_HASH_INDEX)
            .map_err(|error| format!("open block hash index table: {error}"))?;

        block_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear block hash index: {error}"))?;

        let mut tx_index = transaction
            .open_table(TX_INDEX)
            .map_err(|error| format!("open transaction index table: {error}"))?;

        tx_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear transaction index: {error}"))?;

        let mut activity_index = transaction
            .open_table(ADDRESS_ACTIVITY_INDEX)
            .map_err(|error| format!("open address activity index table: {error}"))?;

        activity_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear address activity index: {error}"))?;

        let mut origin_index = transaction
            .open_table(COIN_ORIGIN_INDEX)
            .map_err(|error| format!("open coin origin index: {error}"))?;
        origin_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear coin origin index: {error}"))?;

        for (block, canonical) in blocks.iter().zip(&canonical_blocks) {
            insert_coin_origins(&mut origin_index, canonical, chain)?;
            if block_index
                .get(block.hash.as_slice())
                .map_err(|error| format!("inspect block hash index: {error}"))?
                .is_some()
            {
                return Err(format!(
                    "duplicate canonical block hash at height {}",
                    block.height,
                ));
            }

            block_index
                .insert(block.hash.as_slice(), block.height)
                .map_err(|error| format!("insert block hash index: {error}"))?;

            for (position, hash) in block.transactions.iter().enumerate() {
                if tx_index
                    .get(hash.as_slice())
                    .map_err(|error| format!("inspect transaction index: {error}"))?
                    .is_some()
                {
                    return Err(format!(
                        "duplicate canonical transaction hash at height {}",
                        block.height,
                    ));
                }

                let position =
                    u64::try_from(position).map_err(|_| "transaction index exceeds u64")?;

                let location = encode_transaction_location(block.height, position);

                tx_index
                    .insert(hash.as_slice(), location.as_slice())
                    .map_err(|error| format!("insert transaction index: {error}"))?;
            }

            for activity in &block.activities {
                let key = encode_address_activity_key(
                    activity.address,
                    block.height,
                    activity.transaction_index,
                );

                activity_index
                    .insert(key.as_slice(), EMPTY_INDEX_VALUE)
                    .map_err(|error| format!("insert address activity index: {error}"))?;
            }
        }

        let mut metadata = transaction
            .open_table(META)
            .map_err(|error| format!("open metadata table: {error}"))?;

        metadata
            .insert(INDEX_TIP_HEIGHT_KEY, tip.height.to_le_bytes().as_slice())
            .map_err(|error| format!("write canonical index tip height: {error}"))?;

        metadata
            .insert(INDEX_TIP_HASH_KEY, tip.hash.as_slice())
            .map_err(|error| format!("write canonical index tip hash: {error}"))?;
        metadata
            .insert(COIN_ORIGIN_INDEX_VERSION_KEY, &[1_u8][..])
            .map_err(|error| format!("write coin origin index version: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit canonical index rebuild: {error}"))
}

fn open(directory: &Path) -> Result<Arc<Database>, String> {
    let slot = DATABASE.get_or_init(|| Mutex::new(None));

    let mut slot = slot.lock().map_err(|_| "redb database cache is poisoned")?;

    if let Some((cached, database)) = slot.as_ref()
        && cached == directory
    {
        return Ok(Arc::clone(database));
    }

    fs::create_dir_all(directory).map_err(|error| format!("create database directory: {error}"))?;

    let database = Arc::new(
        Database::create(directory.join(DATABASE_FILE))
            .map_err(|error| format!("open redb database: {error}"))?,
    );

    initialize(&database)?;

    *slot = Some((directory.to_path_buf(), Arc::clone(&database)));

    Ok(database)
}

fn initialize(database: &Database) -> Result<(), String> {
    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin redb schema transaction: {error}"))?;

    {
        let mut metadata = transaction
            .open_table(META)
            .map_err(|error| format!("open metadata table: {error}"))?;

        let stored_version = metadata
            .get("schema_version")
            .map_err(|error| format!("read schema version: {error}"))?
            .map(|version| version.value().to_vec());

        match stored_version {
            Some(version) => {
                let bytes: [u8; 4] = version
                    .try_into()
                    .map_err(|_| "stored redb schema version is invalid")?;

                if u32::from_le_bytes(bytes) != SCHEMA_VERSION {
                    return Err("redb schema version does not match this node".into());
                }
            }

            None => {
                metadata
                    .insert("schema_version", SCHEMA_VERSION.to_le_bytes().as_slice())
                    .map_err(|error| format!("write schema version: {error}"))?;
            }
        }

        let expected_genesis = kernel::genesis::EXPECTED_GENESIS_HASH.0;

        let stored_genesis = metadata
            .get("genesis_hash")
            .map_err(|error| format!("read stored genesis hash: {error}"))?
            .map(|hash| hash.value().to_vec());

        match stored_genesis {
            Some(hash) if hash.as_slice() != expected_genesis => {
                return Err("redb genesis hash does not match this node".into());
            }

            Some(_) => {}

            None => {
                metadata
                    .insert("genesis_hash", expected_genesis.as_slice())
                    .map_err(|error| format!("write genesis hash: {error}"))?;
            }
        }

        let expected_chain_spec = kernel::genesis::chain_spec_hash()
            .map_err(|error| format!("calculate chain specification: {error}"))?
            .0;

        let stored_chain_spec = metadata
            .get("chain_spec_hash")
            .map_err(|error| format!("read stored chain specification: {error}"))?
            .map(|hash| hash.value().to_vec());

        match stored_chain_spec {
            Some(hash) if hash.as_slice() != expected_chain_spec => {
                return Err("redb chain specification does not match this node".into());
            }

            Some(_) => {}

            None => {
                metadata
                    .insert("chain_spec_hash", expected_chain_spec.as_slice())
                    .map_err(|error| format!("write chain specification: {error}"))?;
            }
        }

        transaction
            .open_table(BLOCKS)
            .map_err(|error| format!("open blocks table: {error}"))?;

        transaction
            .open_table(MEMPOOL)
            .map_err(|error| format!("open mempool table: {error}"))?;

        transaction
            .open_table(SNAPSHOTS)
            .map_err(|error| format!("open snapshots table: {error}"))?;

        transaction
            .open_table(AUXILIARY)
            .map_err(|error| format!("open auxiliary table: {error}"))?;

        transaction
            .open_table(BLOCK_HASH_INDEX)
            .map_err(|error| format!("open block hash index table: {error}"))?;

        transaction
            .open_table(TX_INDEX)
            .map_err(|error| format!("open transaction index table: {error}"))?;

        transaction
            .open_table(ADDRESS_ACTIVITY_INDEX)
            .map_err(|error| format!("open address activity index table: {error}"))?;

        transaction
            .open_table(COIN_ORIGIN_INDEX)
            .map_err(|error| format!("open coin origin index table: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit redb schema: {error}"))
}

pub fn has_blocks(directory: &Path) -> Result<bool, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin block read: {error}"))?;

    let table = transaction
        .open_table(BLOCKS)
        .map_err(|error| format!("open blocks table: {error}"))?;

    Ok(table
        .len()
        .map_err(|error| format!("count blocks: {error}"))?
        > 0)
}

pub fn read_blocks(directory: &Path) -> Result<Vec<Vec<u8>>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin block read: {error}"))?;

    let table = transaction
        .open_table(BLOCKS)
        .map_err(|error| format!("open blocks table: {error}"))?;

    table
        .iter()
        .map_err(|error| format!("iterate blocks: {error}"))?
        .map(|entry| {
            entry
                .map(|(_, value)| value.value().to_vec())
                .map_err(|error| format!("read block: {error}"))
        })
        .collect()
}

pub fn append_block_and_replace_mempool(
    directory: &Path,
    block: &StoredCanonicalBlock,
    mempool: &[Vec<u8>],
) -> Result<(), String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin canonical commit: {error}"))?;

    {
        let mut blocks = transaction
            .open_table(BLOCKS)
            .map_err(|error| format!("open blocks table: {error}"))?;

        if blocks
            .get(block.height)
            .map_err(|error| format!("inspect block height: {error}"))?
            .is_some()
        {
            return Err(format!(
                "canonical block height {} already exists",
                block.height,
            ));
        }

        blocks
            .insert(block.height, block.bytes.as_slice())
            .map_err(|error| format!("insert block: {error}"))?;

        let genesis_bytes = blocks
            .get(0)
            .map_err(|error| format!("read genesis for coin origin: {error}"))?
            .ok_or("genesis block missing while indexing coin origin")?
            .value()
            .to_vec();
        let chain = genesis_context(&genesis_bytes)?;
        let canonical = decode_canonical_block(&block.bytes)?;
        if canonical.height().0 != block.height
            || canonical.hash().map_err(|error| error.to_string())?.0 != block.hash
        {
            return Err("stored block identity does not match coin origin source".into());
        }
        let mut origin_index = transaction
            .open_table(COIN_ORIGIN_INDEX)
            .map_err(|error| format!("open coin origin index: {error}"))?;
        insert_coin_origins(&mut origin_index, &canonical, chain)?;

        let mut block_index = transaction
            .open_table(BLOCK_HASH_INDEX)
            .map_err(|error| format!("open block hash index table: {error}"))?;

        block_index
            .insert(block.hash.as_slice(), block.height)
            .map_err(|error| format!("insert block hash index: {error}"))?;

        let mut tx_index = transaction
            .open_table(TX_INDEX)
            .map_err(|error| format!("open transaction index table: {error}"))?;

        let mut metadata = transaction
            .open_table(META)
            .map_err(|error| format!("open metadata table: {error}"))?;

        let previous_index_height = metadata
            .get(INDEX_TIP_HEIGHT_KEY)
            .map_err(|error| format!("read canonical index tip height: {error}"))?
            .map(|value| value.value().to_vec());

        let has_previous_hash = metadata
            .get(INDEX_TIP_HASH_KEY)
            .map_err(|error| format!("read canonical index tip hash: {error}"))?
            .is_some();

        let has_origin_version = metadata
            .get(COIN_ORIGIN_INDEX_VERSION_KEY)
            .map_err(|error| format!("read coin origin index version: {error}"))?
            .is_some_and(|value| value.value() == [1_u8]);

        let previous_index_height = previous_index_height
            .and_then(|bytes| <[u8; 8]>::try_from(bytes).ok())
            .map(u64::from_le_bytes);

        let indexes_were_complete = block.height == 0
            || (has_previous_hash
                && has_origin_version
                && previous_index_height.and_then(|height| height.checked_add(1))
                    == Some(block.height));

        if indexes_were_complete {
            metadata
                .insert(INDEX_TIP_HEIGHT_KEY, block.height.to_le_bytes().as_slice())
                .map_err(|error| format!("write canonical index tip height: {error}"))?;

            metadata
                .insert(INDEX_TIP_HASH_KEY, block.hash.as_slice())
                .map_err(|error| format!("write canonical index tip hash: {error}"))?;
            metadata
                .insert(COIN_ORIGIN_INDEX_VERSION_KEY, &[1_u8][..])
                .map_err(|error| format!("write coin origin index version: {error}"))?;
        }

        for (position, hash) in block.transactions.iter().enumerate() {
            let position = u64::try_from(position).map_err(|_| "transaction index exceeds u64")?;

            let location = encode_transaction_location(block.height, position);

            tx_index
                .insert(hash.as_slice(), location.as_slice())
                .map_err(|error| format!("insert transaction index: {error}"))?;
        }

        let mut activity_index = transaction
            .open_table(ADDRESS_ACTIVITY_INDEX)
            .map_err(|error| format!("open address activity index table: {error}"))?;

        for activity in &block.activities {
            let key = encode_address_activity_key(
                activity.address,
                block.height,
                activity.transaction_index,
            );

            activity_index
                .insert(key.as_slice(), EMPTY_INDEX_VALUE)
                .map_err(|error| format!("insert address activity index: {error}"))?;
        }

        let mut transactions = transaction
            .open_table(MEMPOOL)
            .map_err(|error| format!("open mempool table: {error}"))?;

        replace_ordered_values(&mut transactions, mempool, "mempool")?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit canonical block, indexes and mempool: {error}"))
}

pub fn replace_blocks_and_mempool(
    directory: &Path,
    blocks: &[StoredCanonicalBlock],
    mempool: &[Vec<u8>],
) -> Result<(), String> {
    let first = blocks
        .first()
        .ok_or("canonical reorg cannot persist an empty chain")?;
    let chain = genesis_context(&first.bytes)?;
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin canonical reorg: {error}"))?;

    {
        let mut canonical = transaction
            .open_table(BLOCKS)
            .map_err(|error| format!("open blocks table: {error}"))?;

        let mut common_height = None;
        for block in blocks {
            let matches = canonical
                .get(block.height)
                .map_err(|error| format!("compare canonical block: {error}"))?
                .is_some_and(|stored| stored.value() == block.bytes.as_slice());
            if !matches {
                break;
            }
            common_height = Some(block.height);
        }

        canonical
            .retain(|_, _| false)
            .map_err(|error| format!("clear canonical blocks: {error}"))?;

        let mut block_index = transaction
            .open_table(BLOCK_HASH_INDEX)
            .map_err(|error| format!("open block hash index table: {error}"))?;

        block_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear block hash index: {error}"))?;

        let mut tx_index = transaction
            .open_table(TX_INDEX)
            .map_err(|error| format!("open transaction index table: {error}"))?;

        tx_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear transaction index: {error}"))?;

        let mut activity_index = transaction
            .open_table(ADDRESS_ACTIVITY_INDEX)
            .map_err(|error| format!("open address activity index table: {error}"))?;

        activity_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear address activity index: {error}"))?;

        let mut origin_index = transaction
            .open_table(COIN_ORIGIN_INDEX)
            .map_err(|error| format!("open coin origin index: {error}"))?;
        origin_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear coin origin index: {error}"))?;

        for block in blocks {
            let canonical_block = decode_canonical_block(&block.bytes)?;
            if canonical_block.height().0 != block.height
                || canonical_block.hash().map_err(|error| error.to_string())?.0 != block.hash
            {
                return Err("stored block identity does not match coin origin source".into());
            }
            insert_coin_origins(&mut origin_index, &canonical_block, chain)?;
            canonical
                .insert(block.height, block.bytes.as_slice())
                .map_err(|error| format!("insert canonical block: {error}"))?;

            block_index
                .insert(block.hash.as_slice(), block.height)
                .map_err(|error| format!("insert block hash index: {error}"))?;

            for (position, hash) in block.transactions.iter().enumerate() {
                let position =
                    u64::try_from(position).map_err(|_| "transaction index exceeds u64")?;

                let location = encode_transaction_location(block.height, position);

                tx_index
                    .insert(hash.as_slice(), location.as_slice())
                    .map_err(|error| format!("insert transaction index: {error}"))?;
            }

            for activity in &block.activities {
                let key = encode_address_activity_key(
                    activity.address,
                    block.height,
                    activity.transaction_index,
                );

                activity_index
                    .insert(key.as_slice(), EMPTY_INDEX_VALUE)
                    .map_err(|error| format!("insert address activity index: {error}"))?;
            }
        }

        let tip = blocks
            .last()
            .ok_or("canonical reorg cannot persist an empty chain")?;

        let mut metadata = transaction
            .open_table(META)
            .map_err(|error| format!("open metadata table: {error}"))?;

        metadata
            .insert(INDEX_TIP_HEIGHT_KEY, tip.height.to_le_bytes().as_slice())
            .map_err(|error| format!("write canonical index tip height: {error}"))?;

        metadata
            .insert(INDEX_TIP_HASH_KEY, tip.hash.as_slice())
            .map_err(|error| format!("write canonical index tip hash: {error}"))?;
        metadata
            .insert(COIN_ORIGIN_INDEX_VERSION_KEY, &[1_u8][..])
            .map_err(|error| format!("write coin origin index version: {error}"))?;

        let mut transactions = transaction
            .open_table(MEMPOOL)
            .map_err(|error| format!("open mempool table: {error}"))?;

        replace_ordered_values(&mut transactions, mempool, "mempool")?;

        let mut snapshots = transaction
            .open_table(SNAPSHOTS)
            .map_err(|error| format!("open snapshots during reorg: {error}"))?;
        snapshots
            .retain(|height, _| common_height.is_some_and(|last| height <= last))
            .map_err(|error| format!("discard snapshots from replaced branch: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit canonical reorg, indexes and mempool: {error}"))
}

pub fn read_mempool(directory: &Path) -> Result<Vec<Vec<u8>>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin mempool read: {error}"))?;

    let table = transaction
        .open_table(MEMPOOL)
        .map_err(|error| format!("open mempool table: {error}"))?;

    table
        .iter()
        .map_err(|error| format!("iterate mempool: {error}"))?
        .map(|entry| {
            entry
                .map(|(_, value)| value.value().to_vec())
                .map_err(|error| format!("read mempool transaction: {error}"))
        })
        .collect()
}

pub fn replace_mempool(directory: &Path, transactions: &[Vec<u8>]) -> Result<(), String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin mempool write: {error}"))?;

    {
        let mut table = transaction
            .open_table(MEMPOOL)
            .map_err(|error| format!("open mempool table: {error}"))?;

        replace_ordered_values(&mut table, transactions, "mempool")?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit mempool: {error}"))
}

fn replace_ordered_values(
    table: &mut redb::Table<'_, u64, &[u8]>,
    values: &[Vec<u8>],
    label: &str,
) -> Result<(), String> {
    table
        .retain(|_, _| false)
        .map_err(|error| format!("clear {label}: {error}"))?;

    for (index, bytes) in values.iter().enumerate() {
        table
            .insert(index as u64, bytes.as_slice())
            .map_err(|error| format!("insert {label} value: {error}"))?;
    }

    Ok(())
}

pub fn put_snapshot(directory: &Path, height: u64, bytes: &[u8]) -> Result<(), String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin snapshot write: {error}"))?;

    {
        let mut table = transaction
            .open_table(SNAPSHOTS)
            .map_err(|error| format!("open snapshots table: {error}"))?;

        table
            .insert(height, bytes)
            .map_err(|error| format!("insert snapshot: {error}"))?;

        while table
            .len()
            .map_err(|error| format!("count snapshots: {error}"))?
            > 2
        {
            let oldest = table
                .first()
                .map_err(|error| format!("read oldest snapshot: {error}"))?
                .map(|(key, _)| key.value());

            if let Some(oldest) = oldest {
                table
                    .remove(oldest)
                    .map_err(|error| format!("remove old snapshot: {error}"))?;
            }
        }
    }

    transaction
        .commit()
        .map_err(|error| format!("commit snapshot: {error}"))
}

pub fn snapshots_descending(directory: &Path) -> Result<Vec<(u64, Vec<u8>)>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin snapshot read: {error}"))?;

    let table = transaction
        .open_table(SNAPSHOTS)
        .map_err(|error| format!("open snapshots table: {error}"))?;

    let mut snapshots = table
        .iter()
        .map_err(|error| format!("iterate snapshots: {error}"))?
        .map(|entry| {
            entry
                .map(|(key, value)| (key.value(), value.value().to_vec()))
                .map_err(|error| format!("read snapshot: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    snapshots.reverse();

    Ok(snapshots)
}

pub fn auxiliary_get(directory: &Path, key: &str) -> Result<Option<Vec<u8>>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_read()
        .map_err(|error| format!("begin auxiliary read: {error}"))?;

    let table = transaction
        .open_table(AUXILIARY)
        .map_err(|error| format!("open auxiliary table: {error}"))?;

    Ok(table
        .get(key)
        .map_err(|error| format!("read auxiliary value: {error}"))?
        .map(|value| value.value().to_vec()))
}

pub fn auxiliary_put(directory: &Path, key: &str, value: &[u8]) -> Result<(), String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin auxiliary write: {error}"))?;

    {
        let mut table = transaction
            .open_table(AUXILIARY)
            .map_err(|error| format!("open auxiliary table: {error}"))?;

        table
            .insert(key, value)
            .map_err(|error| format!("write auxiliary value: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit auxiliary value: {error}"))
}

pub fn auxiliary_get_or_insert(
    directory: &Path,
    key: &str,
    value: &[u8],
) -> Result<Vec<u8>, String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin auxiliary initialization: {error}"))?;

    let result;

    {
        let mut table = transaction
            .open_table(AUXILIARY)
            .map_err(|error| format!("open auxiliary table: {error}"))?;

        let existing = table
            .get(key)
            .map_err(|error| format!("read auxiliary value: {error}"))?
            .map(|stored| stored.value().to_vec());

        result = match existing {
            Some(existing) => existing,

            None => {
                table
                    .insert(key, value)
                    .map_err(|error| format!("initialize auxiliary value: {error}"))?;

                value.to_vec()
            }
        };
    }

    transaction
        .commit()
        .map_err(|error| format!("commit auxiliary initialization: {error}"))?;

    Ok(result)
}

#[cfg(test)]
pub fn clear_canonical_indexes_for_test(directory: &Path) -> Result<(), String> {
    let database = open(directory)?;

    let transaction = database
        .begin_write()
        .map_err(|error| format!("begin canonical index test clear: {error}"))?;

    {
        let mut block_index = transaction
            .open_table(BLOCK_HASH_INDEX)
            .map_err(|error| format!("open block hash index table: {error}"))?;

        block_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear block hash index: {error}"))?;

        let mut tx_index = transaction
            .open_table(TX_INDEX)
            .map_err(|error| format!("open transaction index table: {error}"))?;

        tx_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear transaction index: {error}"))?;

        let mut activity_index = transaction
            .open_table(ADDRESS_ACTIVITY_INDEX)
            .map_err(|error| format!("open address activity index table: {error}"))?;

        activity_index
            .retain(|_, _| false)
            .map_err(|error| format!("clear address activity index: {error}"))?;

        let mut metadata = transaction
            .open_table(META)
            .map_err(|error| format!("open metadata table: {error}"))?;

        metadata
            .remove(INDEX_TIP_HEIGHT_KEY)
            .map_err(|error| format!("remove canonical index tip height: {error}"))?;

        metadata
            .remove(INDEX_TIP_HASH_KEY)
            .map_err(|error| format!("remove canonical index tip hash: {error}"))?;
    }

    transaction
        .commit()
        .map_err(|error| format!("commit canonical index test clear: {error}"))
}
