use std::path::Path;

use borsh::{BorshDeserialize, BorshSerialize};
use kernel::{
    block::Block,
    common::Height,
    crypto::{BlockHash, canonical_bytes, canonical_decode, hash_bytes},
    genesis::{EXPECTED_GENESIS_HASH, chain_spec_hash},
    ledger::{Ledger, LedgerSnapshot},
};

pub const SNAPSHOT_INTERVAL: u64 = 1_000;

const SNAPSHOT_MAGIC: [u8; 8] = *b"XPQSNAP1";
const SNAPSHOT_VERSION: u32 = 2;
const CHECKSUM_SIZE: usize = 32;

#[derive(BorshSerialize, BorshDeserialize)]
struct SnapshotPayload {
    magic: [u8; 8],
    version: u32,
    genesis_hash: BlockHash,
    chain_spec_hash: [u8; 32],
    height: Height,
    tip_hash: BlockHash,
    ledger: LedgerSnapshot,
}

#[derive(BorshSerialize, BorshDeserialize)]
struct LegacySnapshotPayload {
    magic: [u8; 8],
    version: u32,
    genesis_hash: BlockHash,
    chain_spec_hash: [u8; 32],
    height: Height,
    tip_hash: BlockHash,
    ledger: Ledger,
}

pub fn write_if_due(database: &Path, ledger: &Ledger) -> Result<bool, String> {
    let height = ledger
        .tip_height()
        .ok_or("cannot snapshot an empty ledger")?;
    if height.0 == 0 || height.0 % SNAPSHOT_INTERVAL != 0 {
        return Ok(false);
    }
    write(database, ledger)?;
    Ok(true)
}

pub fn write_after_large_sync(
    database: &Path,
    ledger: &Ledger,
    applied_blocks: usize,
) -> Result<bool, String> {
    if applied_blocks < SNAPSHOT_INTERVAL as usize {
        return Ok(false);
    }
    write(database, ledger)?;
    Ok(true)
}

fn write(database: &Path, ledger: &Ledger) -> Result<(), String> {
    let height = ledger
        .tip_height()
        .ok_or("cannot snapshot an empty ledger")?;
    let tip_hash = ledger
        .tip_hash()
        .ok_or("cannot snapshot a ledger without a tip")?;
    let payload = SnapshotPayload {
        magic: SNAPSHOT_MAGIC,
        version: SNAPSHOT_VERSION,
        genesis_hash: EXPECTED_GENESIS_HASH,
        chain_spec_hash: chain_spec_hash().map_err(|error| error.to_string())?.0,
        height,
        tip_hash,
        ledger: ledger.snapshot(),
    };
    let payload = canonical_bytes(&payload).map_err(|error| format!("encode snapshot: {error}"))?;
    let checksum = hash_bytes(&payload);
    let mut bytes = Vec::with_capacity(payload.len() + CHECKSUM_SIZE);
    bytes.extend_from_slice(&payload);
    bytes.extend_from_slice(&checksum.0);

    crate::storage::put_snapshot(database, height.0, &bytes)?;
    println!(
        "snapshot: saved height={} tip={}",
        height.0,
        hex::encode(tip_hash.0)
    );
    Ok(())
}

pub fn load(database: &Path, blocks: &[Block]) -> Result<Option<(Ledger, usize)>, String> {
    let mut errors = Vec::new();
    for (height, bytes) in crate::storage::snapshots_descending(database)? {
        match load_bytes(height, &bytes, blocks) {
            Ok(Some(snapshot)) => return Ok(Some(snapshot)),
            Ok(None) => {}
            Err(error) => errors.push(format!("height {height}: {error}")),
        }
    }
    if errors.is_empty() {
        Ok(None)
    } else {
        Err(errors.join("; "))
    }
}

fn load_bytes(
    stored_height: u64,
    bytes: &[u8],
    blocks: &[Block],
) -> Result<Option<(Ledger, usize)>, String> {
    if bytes.len() <= CHECKSUM_SIZE {
        return Err("snapshot is truncated".into());
    }
    let payload_length = bytes.len() - CHECKSUM_SIZE;
    let (payload, stored_checksum) = bytes.split_at(payload_length);
    if hash_bytes(payload).0.as_slice() != stored_checksum {
        return Err("snapshot checksum does not match".into());
    }
    let snapshot = canonical_decode::<SnapshotPayload>(payload);
    let (magic, version, genesis_hash, spec_hash, height, tip_hash, compact, legacy) =
        match snapshot {
            Ok(snapshot) => (
                snapshot.magic,
                snapshot.version,
                snapshot.genesis_hash,
                snapshot.chain_spec_hash,
                snapshot.height,
                snapshot.tip_hash,
                Some(snapshot.ledger),
                None,
            ),
            Err(_) => {
                let snapshot: LegacySnapshotPayload = canonical_decode(payload)
                    .map_err(|error| format!("decode snapshot: {error}"))?;
                (
                    snapshot.magic,
                    snapshot.version,
                    snapshot.genesis_hash,
                    snapshot.chain_spec_hash,
                    snapshot.height,
                    snapshot.tip_hash,
                    None,
                    Some(snapshot.ledger),
                )
            }
        };
    if magic != SNAPSHOT_MAGIC
        || !(version == SNAPSHOT_VERSION || version == 1 && legacy.is_some())
        || genesis_hash != EXPECTED_GENESIS_HASH
        || spec_hash != chain_spec_hash().map_err(|error| error.to_string())?.0
    {
        return Err("snapshot format or genesis does not match this node".into());
    }
    if height.0 != stored_height {
        return Err("snapshot table key does not match payload height".into());
    }
    let index = usize::try_from(height.0).map_err(|_| "snapshot height is too large")?;
    let canonical = blocks
        .get(index)
        .ok_or("snapshot height is beyond the block log")?;
    if canonical.height() != height
        || canonical.hash().map_err(|error| error.to_string())? != tip_hash
    {
        return Err("snapshot tip does not match the canonical block log".into());
    }
    let ledger = match (compact, legacy) {
        (Some(snapshot), None) => Ledger::from_snapshot(snapshot, &blocks[..=index])
            .map_err(|error| format!("restore snapshot: {error}"))?,
        (None, Some(ledger)) => ledger,
        _ => return Err("snapshot format is invalid".into()),
    };
    if ledger.tip_height() != Some(height) || ledger.tip_hash() != Some(tip_hash) {
        return Err("snapshot metadata does not match its ledger".into());
    }
    if ledger.state_root().map_err(|error| error.to_string())? != canonical.state_root() {
        return Err("snapshot state root does not match the canonical block".into());
    }
    if version == 1
        && (ledger.chain.blocks().count() != index + 1
            || !ledger
                .chain
                .blocks()
                .zip(&blocks[..=index])
                .all(|(saved, stored)| saved == stored))
    {
        return Err("legacy snapshot block log does not match the canonical chain".into());
    }
    println!(
        "snapshot: loaded height={} tip={}",
        height.0,
        hex::encode(tip_hash.0)
    );
    Ok(Some((ledger, index + 1)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use kernel::{
        blockchain::Emission,
        common::Nonce,
        consensus::{
            apply_block, apply_genesis, expected_emission_for_height, expected_next_difficulty,
            new_pow_memory,
        },
        crypto::Address,
        genesis::genesis_block,
    };
    use std::fs;

    fn test_directory(label: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!(
            "kernel-snapshot-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn snapshot_roundtrip_is_bound_to_canonical_tip() {
        let directory = test_directory("roundtrip");
        fs::create_dir_all(&directory).unwrap();
        let genesis = genesis_block().unwrap();
        let mut ledger = Ledger::new();
        apply_genesis(&mut ledger, genesis.clone(), EXPECTED_GENESIS_HASH).unwrap();

        write(&directory, &ledger).unwrap();
        let (loaded, next) = load(&directory, &[genesis]).unwrap().unwrap();
        assert_eq!(loaded, ledger);
        assert_eq!(next, 1);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn corrupted_snapshot_is_rejected() {
        let directory = test_directory("corrupt");
        fs::create_dir_all(&directory).unwrap();
        crate::storage::put_snapshot(&directory, 0, &[0_u8; CHECKSUM_SIZE + 1]).unwrap();
        assert!(load(&directory, &[]).unwrap_err().contains("checksum"));
        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compact_snapshot_omits_duplicate_chain_and_accepts_legacy_snapshot() {
        let directory = test_directory("legacy");
        fs::create_dir_all(&directory).unwrap();
        let genesis = genesis_block().unwrap();
        let mut ledger = Ledger::new();
        apply_genesis(&mut ledger, genesis.clone(), EXPECTED_GENESIS_HASH).unwrap();

        write(&directory, &ledger).unwrap();
        let (_, compact) = crate::storage::snapshots_descending(&directory)
            .unwrap()
            .remove(0);
        let legacy = LegacySnapshotPayload {
            magic: SNAPSHOT_MAGIC,
            version: 1,
            genesis_hash: EXPECTED_GENESIS_HASH,
            chain_spec_hash: chain_spec_hash().unwrap().0,
            height: Height(0),
            tip_hash: EXPECTED_GENESIS_HASH,
            ledger: ledger.clone(),
        };
        let legacy_bytes = canonical_bytes(&legacy).unwrap();
        assert!(compact.len() < legacy_bytes.len() + CHECKSUM_SIZE);

        let mut stored = legacy_bytes.clone();
        stored.extend_from_slice(&hash_bytes(&legacy_bytes).0);
        crate::storage::put_snapshot(&directory, 0, &stored).unwrap();
        let (loaded, next) = load(&directory, &[genesis]).unwrap().unwrap();
        assert_eq!(loaded, ledger);
        assert_eq!(next, 1);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn compact_snapshot_rejects_state_that_disagrees_with_block() {
        let directory = test_directory("state-root");
        fs::create_dir_all(&directory).unwrap();
        let genesis = genesis_block().unwrap();
        let mut canonical = Ledger::new();
        apply_genesis(&mut canonical, genesis.clone(), EXPECTED_GENESIS_HASH).unwrap();
        let canonical_bytes = borsh::to_vec(&canonical).unwrap();
        let mut altered = canonical.clone();
        altered.state.coin.total_mined = kernel::monetary::coin::Zeno::from_zeno(1);
        altered.state.coin.total_burned = kernel::monetary::coin::Zeno::from_zeno(1);

        write(&directory, &altered).unwrap();
        assert!(
            load(&directory, &[genesis])
                .unwrap_err()
                .contains("state root")
        );
        assert_eq!(borsh::to_vec(&canonical).unwrap(), canonical_bytes);

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn restored_snapshot_can_rollback_a_mined_block() {
        let directory = test_directory("rollback");
        fs::create_dir_all(&directory).unwrap();
        let genesis = genesis_block().unwrap();
        let mut ledger = Ledger::new();
        apply_genesis(&mut ledger, genesis.clone(), EXPECTED_GENESIS_HASH).unwrap();
        let genesis_ledger = ledger.clone();
        write(&directory, &genesis_ledger).unwrap();
        let mut blocks = vec![genesis];
        let mut memory = new_pow_memory();

        for height in 1..=2 {
            let height = Height(height);
            let mut block = Block::from_protocol_transactions(
                height,
                ledger.tip_hash().unwrap(),
                expected_next_difficulty(&ledger.chain).unwrap(),
                Nonce(0),
                Some(Emission::new(
                    Address::ZERO,
                    expected_emission_for_height(height),
                )),
                vec![],
            )
            .unwrap();
            let (state_root, weight) = ledger.preview_block_commitments(&block).unwrap();
            block.set_state_root(state_root);
            block.set_block_weight(weight);
            assert!(
                crate::miner::mine_range(
                    &mut block,
                    crate::miner::MiningRange {
                        start_nonce: 0,
                        attempts: 100,
                    },
                    &mut memory,
                )
                .unwrap()
                .is_some()
            );
            apply_block(&mut ledger, block.clone()).unwrap();
            blocks.push(block);
        }

        write(&directory, &ledger).unwrap();
        let (_, compact) = crate::storage::snapshots_descending(&directory)
            .unwrap()
            .remove(0);
        let legacy = LegacySnapshotPayload {
            magic: SNAPSHOT_MAGIC,
            version: 1,
            genesis_hash: EXPECTED_GENESIS_HASH,
            chain_spec_hash: chain_spec_hash().unwrap().0,
            height: Height(2),
            tip_hash: ledger.tip_hash().unwrap(),
            ledger: ledger.clone(),
        };
        let legacy_size = canonical_bytes(&legacy).unwrap().len() + CHECKSUM_SIZE;
        println!(
            "snapshot sizes: compact={} legacy={legacy_size}",
            compact.len()
        );
        assert!(compact.len() < legacy_size);
        let (mut restored, next) = load(&directory, &blocks).unwrap().unwrap();
        assert_eq!(next, 3);
        assert_eq!(restored, ledger);
        assert_eq!(restored.rollback_tip().unwrap(), blocks[2]);
        assert_eq!(restored.tip_hash(), Some(blocks[1].hash().unwrap()));

        let mut changed_body = blocks.clone();
        changed_body[1].body.emission.as_mut().unwrap().to = Address::from_bytes([1; 21]);
        assert!(
            load_bytes(2, &compact, &changed_body)
                .unwrap_err()
                .contains("invalid block")
        );

        let legacy_bytes = canonical_bytes(&legacy).unwrap();
        let mut stored_legacy = legacy_bytes.clone();
        stored_legacy.extend_from_slice(&hash_bytes(&legacy_bytes).0);
        assert!(
            load_bytes(2, &stored_legacy, &changed_body)
                .unwrap_err()
                .contains("legacy snapshot block log")
        );

        let mut different_tip = blocks.clone();
        different_tip[2].header.nonce = Nonce(99);
        let (fallback, next) = load(&directory, &different_tip).unwrap().unwrap();
        assert_eq!(fallback, genesis_ledger);
        assert_eq!(next, 1);

        fs::remove_dir_all(directory).unwrap();
    }
}
