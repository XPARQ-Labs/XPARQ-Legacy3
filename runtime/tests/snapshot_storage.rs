#![allow(dead_code)]

// Compile the storage and snapshot modules as a focused integration target.
// The node's older unit-test module is currently blocked by stale API fixtures.
#[path = "../src/miner.rs"]
mod miner;

#[test]
fn node_restarts_from_a_compact_snapshot() {
    use kernel::{
        block::{Block, Emission, block_bytes},
        common::{Height, Nonce},
        consensus::{
            apply_block, apply_genesis, expected_emission_for_height, expected_next_difficulty,
            new_pow_memory, validate_emission,
        },
        crypto::Address,
        genesis::{EXPECTED_GENESIS_HASH, genesis_block},
        ledger::Ledger,
        monetary::coin::CoinShare,
    };

    let directory = std::env::temp_dir().join(format!(
        "xparq-snapshot-startup-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let genesis = genesis_block().unwrap();
    let mut ledger = Ledger::new();
    apply_genesis(&mut ledger, genesis.clone(), EXPECTED_GENESIS_HASH).unwrap();
    let genesis_ledger = ledger.clone();
    let height = Height(1);
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
    let (root, weight) = ledger.preview_block_commitments(&block).unwrap();
    block.set_state_root(root);
    block.set_block_weight(weight);
    assert!(
        miner::mine_range(
            &mut block,
            miner::MiningRange {
                start_nonce: 0,
                attempts: 100,
            },
            &mut new_pow_memory(),
        )
        .unwrap()
        .is_some()
    );
    apply_block(&mut ledger, block.clone()).unwrap();

    for block in [&genesis, &block] {
        storage::append_block_and_replace_mempool(
            &directory,
            &storage::StoredCanonicalBlock {
                height: block.height().0,
                hash: block.hash().unwrap().0,
                bytes: block_bytes(&block).unwrap(),
                transactions: vec![],
                activities: vec![],
            },
            &[],
        )
        .unwrap();
    }
    let emission_id =
        CoinShare::from_emission_origin(&validate_emission(&block).unwrap().origin().into_bytes());
    assert_eq!(
        storage::read_coin_origin(&directory, emission_id).unwrap(),
        Some(storage::CoinOrigin {
            created_at: None,
            created_in: height,
        })
    );
    let indexed = [&genesis, &block]
        .into_iter()
        .map(|block| storage::CanonicalIndexBlock {
            height: block.height().0,
            hash: block.hash().unwrap().0,
            bytes: block_bytes(block).unwrap(),
            transactions: vec![],
            activities: vec![],
        })
        .collect::<Vec<_>>();
    storage::rebuild_canonical_indexes(&directory, &indexed).unwrap();
    assert_eq!(
        storage::read_coin_origin(&directory, emission_id).unwrap(),
        Some(storage::CoinOrigin {
            created_at: None,
            created_in: height
        })
    );
    snapshot::write_after_large_sync(
        &directory,
        &genesis_ledger,
        snapshot::SNAPSHOT_INTERVAL as usize,
    )
    .unwrap();
    snapshot::write_after_large_sync(&directory, &ledger, snapshot::SNAPSHOT_INTERVAL as usize)
        .unwrap();
    // The storage module caches its last redb handle; switch it away before
    // opening the fixture in a separate node process.
    let spare = directory.with_extension("cache-release");
    storage::snapshots_descending(&spare).unwrap();
    let result = std::process::Command::new(env!("CARGO_BIN_EXE_node"))
        .arg("check")
        .arg(&directory)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let output = String::from_utf8(result.stdout).unwrap();
    assert!(output.contains("snapshot: loaded height=1"), "{output}");
    assert!(output.contains("height: 1"), "{output}");
    assert!(output.contains("database: valid"), "{output}");

    let replacement = Block::from_protocol_transactions(
        height,
        genesis.hash().unwrap(),
        block.header.target_bits,
        Nonce(block.header.nonce.0 + 1),
        Some(Emission::new(
            Address::from_bytes([1; kernel::crypto::ADDRESS_SIZE]),
            expected_emission_for_height(height),
        )),
        vec![],
    )
    .unwrap();
    let replacement_id = CoinShare::from_emission_origin(
        &validate_emission(&replacement)
            .unwrap()
            .origin()
            .into_bytes(),
    );
    let stored = [genesis, replacement]
        .into_iter()
        .map(|block| storage::StoredCanonicalBlock {
            height: block.height().0,
            hash: block.hash().unwrap().0,
            bytes: block_bytes(&block).unwrap(),
            transactions: vec![],
            activities: vec![],
        })
        .collect::<Vec<_>>();
    storage::replace_blocks_and_mempool(&directory, &stored, &[]).unwrap();
    assert_eq!(
        storage::read_coin_origin(&directory, emission_id).unwrap(),
        None
    );
    assert_eq!(
        storage::read_coin_origin(&directory, replacement_id).unwrap(),
        Some(storage::CoinOrigin {
            created_at: None,
            created_in: height
        })
    );
    let retained = storage::snapshots_descending(&directory).unwrap();
    assert_eq!(
        retained
            .iter()
            .map(|(height, _)| *height)
            .collect::<Vec<_>>(),
        vec![0]
    );

    std::fs::remove_dir_all(directory).unwrap();
    std::fs::remove_dir_all(spare).unwrap();
}
#[path = "../src/snapshot.rs"]
mod snapshot;
#[path = "../src/storage.rs"]
mod storage;
