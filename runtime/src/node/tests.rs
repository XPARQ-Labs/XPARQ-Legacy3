use super::mining::{MiningAttempt, mine_block_database};
use super::*;
use super::{
    chain_sync::*, explorer::*, gossip::*, index::*, mempool::*, protocol::*, rpc::*, state::*,
    util::*,
};

#[test]
fn embedded_api_documentation_is_valid_and_references_every_rpc_route() {
    let specification: serde_json::Value = serde_json::from_slice(OPENAPI_JSON).unwrap();
    assert_eq!(specification["openapi"], "3.1.0");
    for route in [
        "/status",
        "/fee-policy",
        "/blocks/latest",
        "/block/{height}",
        "/balance/{address}",
        "/account/{address}",
        "/asset/{asset}",
        "/asset/{asset}/balance/{address}",
        "/pools",
        "/pool/{pool}",
        "/pool/shares/{address}",
        "/explorer/address/{address}",
        "/explorer/transaction/{transaction_id}",
        "/transaction",
    ] {
        assert!(
            specification["paths"].get(route).is_some(),
            "missing {route}"
        );
    }
    assert!(
        API_DOCS_HTML
            .windows(b"/openapi.json".len())
            .any(|window| window == b"/openapi.json")
    );
}

#[test]
fn asset_transaction_projection_exposes_asset_and_action() {
    let chain = kernel::genesis::chain_context().unwrap();
    let seed =
        kernel::crypto::SigningSeed::new(kernel::crypto::Signature::MlDsa44, Box::new([0x51; 32]));
    let public_key = seed.public_key();
    let signer = kernel::crypto::address_from_public_key(&public_key);
    let asset_call = kernel::transaction::AssetIntent::new(
        kernel::transaction::AssetInstruction::Register {
            name: "Test Token".into(),
            max_supply: kernel::monetary::asset::Unit::from_units(100_000_000_000_000_000_000_000),
            initial_mint: kernel::monetary::asset::Unit::from_units(1_000_000),
            mint_authority: signer,
            nonce: 0,
        },
        signer,
    );
    let asset = asset_call.asset().unwrap().to_string();
    let payment = kernel::transaction::SpendIntent::coin_with_charges(
        signer,
        vec![kernel::monetary::coin::CoinShare::from_bytes(
            [1; kernel::crypto::HASH16_SIZE],
        )],
        vec![],
        kernel::transaction::SpendCharges::new(Zeno::ONE),
    )
    .unwrap();
    let commitment =
        kernel::transaction::asset_call_commitment(&asset_call, &payment, chain).unwrap();
    let transaction = kernel::transaction::AuthorizedAssetTransaction {
        call: asset_call,
        payment,
        authorization: kernel::transaction::AccountAuthorization {
            public_key,
            signature: seed.sign(commitment.as_bytes()),
        },
    };
    let response = asset_transaction_response(&transaction, Zeno::from_zeno(4_782));
    assert_eq!(response["asset"], asset);
    assert_eq!(response["asset_instruction"]["type"], "register");
    assert_eq!(response["miner_fee"], 1);
    assert_eq!(response["protocol_burn"], 4_782);
    assert_eq!(
        response["asset_instruction"]["max_supply"],
        "100000000000000000000000"
    );
}

#[test]
fn account_projection_lists_asset_supply_and_creator_shares() {
    let seed =
        kernel::crypto::SigningSeed::new(kernel::crypto::Signature::MlDsa44, Box::new([0x61; 32]));
    let authority = kernel::crypto::address_from_public_key(&seed.public_key());
    let call = kernel::transaction::AssetIntent::new(
        kernel::transaction::AssetInstruction::Register {
            name: "Authority Asset".into(),
            max_supply: kernel::monetary::asset::Unit::from_units(10),
            initial_mint: kernel::monetary::asset::Unit::from_units(4),
            mint_authority: authority,
            nonce: 0,
        },
        authority,
    );
    let mut ledger = Ledger::new();
    ledger
        .state
        .assets
        .apply(&mut ledger.state.utxos, &call, [0; 32])
        .unwrap();

    let assets = account_asset_balances(&ledger, authority).unwrap();
    assert_eq!(assets.len(), 1);
    assert_eq!(assets[0]["max_supply"], "10");
    assert_eq!(assets[0]["mint"], "4");
    assert!(assets[0].get("balance").is_none());
    assert_eq!(assets[0]["shares"].as_array().unwrap().len(), 1);
    assert_eq!(assets[0]["shares"][0]["amount"], "4");
    assert!(
        assets[0]["shares"][0]["share_id"].as_str().unwrap().len() == kernel::crypto::HASH_SIZE * 2
    );
}

#[test]
fn explorer_address_response_is_aggregate_only() {
    let ledger = kernel::genesis::genesis_ledger().unwrap();
    let response = explorer_address_response(
        Path::new("test explorer address"),
        &ledger,
        &[],
        Address([7; kernel::crypto::ADDRESS_SIZE]),
        true,
        DEFAULT_ADDRESS_ACTIVITY_LIMIT,
        None,
    )
    .unwrap();
    assert_eq!(response["balance"]["total"], 0);
    assert_eq!(response["activity_count"], 0);
    assert!(response.get("utxos").is_none());
}

#[test]
fn explorer_address_pagination_rebuilds_after_reorg() {
    let database = test_database("explorer-address-pagination-reorg");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x92; kernel::crypto::ADDRESS_SIZE]);

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    // Branch A: heights 1 -> 2 -> 3.
    for height in 1_u64..=3 {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("branch A tip"),
            target_bits,
            Nonce(height),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("construct branch A block");

        ledger
            .chain
            .insert_block(block)
            .expect("insert branch A block");
    }

    let page_a =
        address_activity_page(&database, &ledger, miner, None, 2).expect("read branch A page");

    assert_eq!(
        page_a.locations,
        vec![
            ActivityLocation::Emission { height: Height(3) },
            ActivityLocation::Emission { height: Height(2) },
        ],
    );

    // Remove branch A completely.
    for _ in 0..3 {
        let tip = ledger.tip_hash().expect("branch A tip");
        ledger.chain.remove_tip(tip).expect("remove branch A tip");
    }

    // Branch B: heights 1 -> 2 -> 3 -> 4.
    for height in 1_u64..=4 {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("branch B tip"),
            target_bits,
            Nonce(100 + height),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("construct branch B block");

        ledger
            .chain
            .insert_block(block)
            .expect("insert branch B block");
    }

    // Tip changed, so persistent activity index must rebuild.
    let page_b =
        address_activity_page(&database, &ledger, miner, None, 2).expect("read branch B page");

    assert_eq!(
        page_b.locations,
        vec![
            ActivityLocation::Emission { height: Height(4) },
            ActivityLocation::Emission { height: Height(3) },
        ],
    );

    let cursor = page_b.next_cursor.expect("branch B first page cursor");

    let page_b2 = address_activity_page(&database, &ledger, miner, Some(cursor), 2)
        .expect("read branch B second page");

    assert_eq!(
        page_b2.locations,
        vec![
            ActivityLocation::Emission { height: Height(2) },
            ActivityLocation::Emission { height: Height(1) },
        ],
    );

    assert_eq!(page_b2.next_cursor, None);
}

#[test]
fn explorer_address_pagination_advances_when_emissions_are_hidden() {
    let database = test_database("explorer-address-pagination-hidden-emissions");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x93; kernel::crypto::ADDRESS_SIZE]);

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    for height in 1_u64..=3 {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("canonical tip"),
            target_bits,
            Nonce(height),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("construct emission block");

        ledger
            .chain
            .insert_block(block)
            .expect("insert emission block");
    }

    let first = explorer_address_response(&database, &ledger, &[], miner, false, 2, None)
        .expect("first hidden-emission page");

    assert_eq!(first["activity_count"], 0);
    assert_eq!(first["emission_count"], 2);

    assert_eq!(
        first["activities"]
            .as_array()
            .expect("activities array")
            .len(),
        0,
    );

    let cursor_hex = first["next_cursor"]
        .as_str()
        .expect("first page has next cursor");

    let cursor_bytes = hex::decode(cursor_hex).expect("decode activity cursor");

    let cursor: [u8; crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE] = cursor_bytes
        .try_into()
        .expect("activity cursor has correct size");

    let second = explorer_address_response(&database, &ledger, &[], miner, false, 2, Some(cursor))
        .expect("second hidden-emission page");

    assert_eq!(second["activity_count"], 0);
    assert_eq!(second["emission_count"], 1);

    assert_eq!(
        second["activities"]
            .as_array()
            .expect("activities array")
            .len(),
        0,
    );

    assert!(second["next_cursor"].is_null());
}

#[test]
fn explorer_activity_reports_net_transfer_for_sender_and_recipient() {
    let mnemonic = wallet::encode_bip39_mnemonic(&[3; 16]).unwrap();
    let sender =
        wallet::account_wallet_from_bip39_mnemonic(&mnemonic, kernel::crypto::Signature::MlDsa44)
            .unwrap();
    let recipient = Address([4; kernel::crypto::ADDRESS_SIZE]);
    let miner = Address([5; kernel::crypto::ADDRESS_SIZE]);
    let intent = kernel::transaction::SpendIntent::coin(
        sender.address,
        vec![kernel::monetary::coin::CoinShare::from_bytes(
            [6; kernel::monetary::coin::CoinShare::SIZE],
        )],
        vec![
            CoinOutput::new(recipient, Zeno::from_zeno(10)),
            CoinOutput::new(sender.address, Zeno::from_zeno(5)),
        ],
    )
    .unwrap();
    let transaction =
        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()));
    let genesis = genesis_block().unwrap();
    let block = Block::from_protocol_transactions(
        Height(1),
        genesis.hash().unwrap(),
        1,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction.clone()],
    )
    .unwrap();

    let outgoing = address_transaction_activity(&transaction, sender.address, &block)
        .unwrap()
        .unwrap();
    assert_eq!(outgoing["direction"], "out");
    assert_eq!(outgoing["amount"], 10);
    assert_eq!(
        outgoing["size_bytes"],
        canonical_bytes(&transaction).unwrap().len()
    );
    let incoming = address_transaction_activity(&transaction, recipient, &block)
        .unwrap()
        .unwrap();
    assert_eq!(incoming["direction"], "in");
    assert_eq!(incoming["amount"], 10);
    assert!(
        address_transaction_activity(
            &transaction,
            Address([9; kernel::crypto::ADDRESS_SIZE]),
            &block,
        )
        .unwrap()
        .is_none()
    );
    assert_eq!(
        parse_hash(&hex::encode(transaction.id().unwrap())).unwrap(),
        transaction.id().unwrap()
    );
}

fn read_test_http_request(parts: &[&[u8]]) -> Result<HttpRequest, String> {
    let bytes = parts.concat();
    read_http_request(&mut std::io::Cursor::new(bytes))
}

fn test_database(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kernel-node-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn failed_reorg_root_check_keeps_persisted_canonical_chain() {
    let database = test_database("failed-reorg-root");
    let _ = load_or_initialize_owned(&database).expect("initialize genesis");
    let miner = Address([0xa1; kernel::crypto::ADDRESS_SIZE]);
    let mut memory = new_pow_memory();
    assert!(matches!(
        mine_block_database(&database, miner, 0, 100, &mut memory).unwrap(),
        MiningAttempt::Mined
    ));

    let original_blocks = crate::storage::read_blocks(&database).unwrap();
    let mut altered = load_or_initialize_owned(&database).unwrap();
    let original_tip = altered.tip_hash().unwrap();
    let (coin_id, coin) = altered.state.utxos.coins().next().expect("emission coin");
    let mut coin = *coin;
    coin.owner = Address([0xa2; kernel::crypto::ADDRESS_SIZE]);
    altered.state.utxos.consume_coin(&coin_id).unwrap();
    altered.state.utxos.insert_coin(coin_id, coin).unwrap();
    assert!(altered.state.validate_supply_invariants().is_ok());
    update_ledger_cache(&database, altered).unwrap();

    let genesis = kernel::genesis::genesis_block().unwrap();
    let genesis_hash = genesis.hash().unwrap();
    let target_bits = expected_next_difficulty(&load_existing(&database).unwrap().chain).unwrap();
    let alternative_one = Block::from_protocol_transactions(
        Height(1),
        genesis_hash,
        target_bits,
        Nonce(100),
        Some(Emission::new(
            miner,
            expected_emission_for_height(Height(1)),
        )),
        vec![],
    )
    .unwrap();
    let alternative_two = Block::from_protocol_transactions(
        Height(2),
        alternative_one.hash().unwrap(),
        target_bits,
        Nonce(101),
        Some(Emission::new(
            miner,
            expected_emission_for_height(Height(2)),
        )),
        vec![],
    )
    .unwrap();
    let sync = HeaderSyncResult {
        ancestor_height: Height(0),
        ancestor_hash: genesis_hash,
        headers: vec![
            kernel::consensus::HeaderAtHeight::new(Height(1), alternative_one.header.clone()),
            kernel::consensus::HeaderAtHeight::new(Height(2), alternative_two.header.clone()),
        ],
        peer_work: Work::MAX,
        peer_weight: u64::MAX,
        preferred: true,
    };

    let error = apply_verified_branch(&database, sync, vec![alternative_one, alternative_two])
        .expect_err("corrupt active state must abort reorg");
    assert!(error.contains("active state root"), "{error}");
    assert_eq!(
        crate::storage::read_blocks(&database).unwrap(),
        original_blocks
    );
    assert_eq!(
        load_existing(&database).unwrap().tip_hash(),
        Some(original_tip)
    );
}

#[test]
fn failed_reorg_after_applying_alternative_block_keeps_database_and_cache() {
    let database = test_database("failed-reorg-after-apply");
    let _ = load_or_initialize_owned(&database).expect("initialize genesis");
    let mut memory = new_pow_memory();
    assert!(matches!(
        mine_block_database(
            &database,
            Address([0xb1; kernel::crypto::ADDRESS_SIZE]),
            0,
            100,
            &mut memory,
        )
        .unwrap(),
        MiningAttempt::Mined
    ));
    let stored_before = crate::storage::read_blocks(&database).unwrap();
    let canonical_before = load_or_initialize_owned(&database).unwrap();
    let original_tip = canonical_before.tip_hash();
    let original_root = canonical_before.state_root().unwrap();

    let mut alternative = kernel::genesis::genesis_ledger().unwrap();
    let miner = Address([0xb2; kernel::crypto::ADDRESS_SIZE]);
    let mut first = Block::from_protocol_transactions(
        Height(1),
        alternative.tip_hash().unwrap(),
        expected_next_difficulty(&alternative.chain).unwrap(),
        Nonce(0),
        Some(Emission::new(
            miner,
            expected_emission_for_height(Height(1)),
        )),
        vec![],
    )
    .unwrap();
    let (first_root, first_weight) = alternative.preview_block_commitments(&first).unwrap();
    first.set_state_root(first_root);
    first.set_block_weight(first_weight);
    assert!(
        crate::miner::mine_range(
            &mut first,
            crate::miner::MiningRange {
                start_nonce: 0,
                attempts: 100
            },
            &mut memory,
        )
        .unwrap()
        .is_some()
    );
    kernel::consensus::apply_block(&mut alternative, first.clone()).unwrap();

    let mut second = Block::from_protocol_transactions(
        Height(2),
        alternative.tip_hash().unwrap(),
        expected_next_difficulty(&alternative.chain).unwrap(),
        Nonce(0),
        Some(Emission::new(
            miner,
            expected_emission_for_height(Height(2)),
        )),
        vec![],
    )
    .unwrap();
    let (correct_root, second_weight) = alternative.preview_block_commitments(&second).unwrap();
    assert_ne!(correct_root, kernel::crypto::StateRoot::ZERO);
    second.set_state_root(kernel::crypto::StateRoot::ZERO);
    second.set_block_weight(second_weight);
    assert!(
        crate::miner::mine_range(
            &mut second,
            crate::miner::MiningRange {
                start_nonce: 0,
                attempts: 100
            },
            &mut memory,
        )
        .unwrap()
        .is_some()
    );
    let sync = HeaderSyncResult {
        ancestor_height: Height(0),
        ancestor_hash: kernel::genesis::genesis_block().unwrap().hash().unwrap(),
        headers: vec![
            kernel::consensus::HeaderAtHeight::new(Height(1), first.header.clone()),
            kernel::consensus::HeaderAtHeight::new(Height(2), second.header.clone()),
        ],
        peer_work: Work::MAX,
        peer_weight: u64::MAX,
        preferred: true,
    };
    let error = apply_verified_branch(&database, sync, vec![first, second])
        .expect_err("second alternative block has an invalid state root");
    assert!(error.contains("state root"), "{error}");
    assert_eq!(
        crate::storage::read_blocks(&database).unwrap(),
        stored_before
    );
    let cached = load_or_initialize_owned(&database).unwrap();
    assert_eq!(cached.tip_hash(), original_tip);
    assert_eq!(cached.state_root().unwrap(), original_root);
    assert_eq!(load_existing(&database).unwrap().tip_hash(), original_tip);
}

fn append_synthetic_header_block(ledger: &mut Ledger, miner: Address) {
    let height = Height(
        ledger
            .tip_height()
            .expect("synthetic chain has genesis")
            .0
            .saturating_add(1),
    );

    let previous = ledger
        .tip_hash()
        .expect("synthetic chain has canonical tip");

    // Use a known-valid target encoding from genesis.
    // This test checks checkpoint accounting, not difficulty adjustment.
    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("synthetic chain has genesis")
        .target_bits();

    let mut block = Block::from_protocol_transactions(
        height,
        previous,
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        Vec::new(),
    )
    .expect("construct synthetic block");

    // Make cumulative weight sensitive to skipped/double-counted blocks.
    block.set_block_weight(
        u32::try_from(height.0).expect("synthetic test height fits block weight"),
    );

    ledger
        .chain
        .insert_block(block)
        .expect("append synthetic canonical block");
}

fn sequential_header_metrics(ledger: &Ledger, target_height: Height) -> (Work, u64) {
    let mut cumulative_work = Work::ZERO;
    let mut cumulative_weight = 0_u64;

    for value in 1..=target_height.0 {
        let height = Height(value);

        let block = ledger
            .chain
            .block(&height)
            .expect("sequential test block exists");

        let block_work = kernel::consensus::block_work(block.target_bits())
            .expect("synthetic target bits are valid");

        cumulative_work = cumulative_work.saturating_add(block_work);

        cumulative_weight = cumulative_weight.saturating_add(u64::from(block.block_weight()));
    }

    (cumulative_work, cumulative_weight)
}

#[test]
fn checkpoint_state_matches_sequential_state_at_boundaries() {
    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x73; kernel::crypto::ADDRESS_SIZE]);

    while ledger.tip_height() != Some(Height(512)) {
        append_synthetic_header_block(&mut ledger, miner);
    }

    let (checkpoints, total_work, total_weight) =
        build_header_state_checkpoints(&ledger).expect("build header checkpoints");

    assert_eq!(
        checkpoints
            .iter()
            .map(|checkpoint| checkpoint.height.0)
            .collect::<Vec<_>>(),
        vec![0, 256, 512],
    );

    let (expected_total_work, expected_total_weight) =
        sequential_header_metrics(&ledger, Height(512));

    assert_eq!(total_work, expected_total_work);
    assert_eq!(total_weight, expected_total_weight);

    for value in [0_u64, 1, 255, 256, 257, 511, 512] {
        let height = Height(value);

        let state = ledger_header_state_at_height(&ledger, &checkpoints, height)
            .expect("checkpoint header state");

        let (expected_work, expected_weight) = sequential_header_metrics(&ledger, height);

        assert_eq!(
            state.cumulative_work, expected_work,
            "cumulative work mismatch at height {value}",
        );

        assert_eq!(
            state.cumulative_weight, expected_weight,
            "cumulative weight mismatch at height {value}",
        );

        assert_eq!(state.height, height);

        assert_eq!(
            state.difficulty_anchor.height,
            if value == 0 { Height(0) } else { Height(1) },
            "difficulty anchor mismatch at height {value}",
        );

        if kernel::consensus::RECENT_HEADER_WINDOW > 0 {
            assert_eq!(
                state
                    .recent_headers
                    .last()
                    .expect("recent headers contain target")
                    .height,
                height,
            );
        }
    }
}

#[test]
fn explorer_tx_index_finds_canonical_transaction() {
    let database = test_database("tx-index");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let mnemonic = wallet::encode_bip39_mnemonic(&[0x31; 16]).unwrap();

    let sender =
        wallet::account_wallet_from_bip39_mnemonic(&mnemonic, kernel::crypto::Signature::MlDsa44)
            .unwrap();

    let recipient = Address([0x32; kernel::crypto::ADDRESS_SIZE]);

    let miner = Address([0x33; kernel::crypto::ADDRESS_SIZE]);

    let intent = kernel::transaction::SpendIntent::coin(
        sender.address,
        vec![kernel::monetary::coin::CoinShare::from_bytes(
            [0x34; kernel::monetary::coin::CoinShare::SIZE],
        )],
        vec![CoinOutput::new(recipient, Zeno::from_zeno(10))],
    )
    .unwrap();

    let transaction =
        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()));

    let transaction_hash = transaction.id().expect("transaction ID");

    let previous = ledger.tip_hash().expect("genesis tip");

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    let block = Block::from_protocol_transactions(
        Height(1),
        previous,
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction],
    )
    .expect("construct transaction block");

    ledger
        .chain
        .insert_block(block)
        .expect("insert transaction block");

    let location = transaction_location(&database, &ledger, transaction_hash)
        .expect("transaction index lookup")
        .expect("indexed transaction");

    assert_eq!(location.height, Height(1));
    assert_eq!(location.transaction_index, 0);

    crate::storage::clear_canonical_indexes_for_test(&database)
        .expect("clear persistent canonical indexes");

    assert_eq!(
        crate::storage::canonical_index_tip(&database).expect("read cleared canonical index tip"),
        None,
    );

    assert_eq!(
        crate::storage::read_transaction_location(&database, transaction_hash,)
            .expect("read cleared transaction index"),
        None,
    );

    // Lookup berikutnya harus mendeteksi marker hilang dan rebuild
    // BLOCK_HASH_INDEX + TX_INDEX dari canonical ledger.
    let rebuilt_location = transaction_location(&database, &ledger, transaction_hash)
        .expect("rebuild persistent transaction index")
        .expect("transaction after persistent index rebuild");

    assert_eq!(rebuilt_location.height, Height(1));
    assert_eq!(rebuilt_location.transaction_index, 0);

    let block = ledger.chain.block(&Height(1)).expect("canonical block 1");

    let block_hash = block.hash().expect("canonical block hash").0;

    assert_eq!(
        canonical_block_height(&database, &ledger, block_hash,)
            .expect("lookup rebuilt block hash index"),
        Some(Height(1)),
    );

    assert_eq!(
        crate::storage::canonical_index_tip(&database).expect("read rebuilt canonical index tip"),
        Some((Height(1).0, block_hash)),
    );

    assert!(
        transaction_location(&database, &ledger, [0xff; 32],)
            .expect("missing transaction lookup")
            .is_none()
    );
}

#[test]
fn handshake_rejects_a_different_wire_version() {
    let mut handshake = Handshake {
        magic: P2P_MAGIC,
        protocol_version: P2P_PROTOCOL_VERSION + 1,
        node_id: [1; 32],
        genesis_hash: EXPECTED_GENESIS_HASH.0,
        chain_spec_hash: chain_spec_hash().unwrap().0,
        capabilities: LOCAL_CAPABILITIES,
        tip_height: Height(0),
        tip_hash: EXPECTED_GENESIS_HASH.0,
        cumulative_work: [0; 8],
        cumulative_weight: 0,
    };
    assert!(
        validate_handshake(&handshake)
            .unwrap_err()
            .contains("unsupported P2P protocol version")
    );

    handshake.protocol_version = P2P_PROTOCOL_VERSION;
    assert!(validate_handshake(&handshake).is_ok());

    handshake.chain_spec_hash[0] ^= 1;
    assert!(
        validate_handshake(&handshake)
            .unwrap_err()
            .contains("chain specification")
    );
}

#[test]
fn explorer_index_extends_after_canonical_append() {
    let database = test_database("explorer-index-append");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x41; kernel::crypto::ADDRESS_SIZE]);

    let make_transaction = |seed_byte: u8, input_byte: u8, recipient_byte: u8| {
        let mnemonic = wallet::encode_bip39_mnemonic(&[seed_byte; 16]).unwrap();

        let sender = wallet::account_wallet_from_bip39_mnemonic(
            &mnemonic,
            kernel::crypto::Signature::MlDsa44,
        )
        .unwrap();

        let recipient = Address([recipient_byte; kernel::crypto::ADDRESS_SIZE]);

        let intent = kernel::transaction::SpendIntent::coin(
            sender.address,
            vec![kernel::monetary::coin::CoinShare::from_bytes(
                [input_byte; kernel::monetary::coin::CoinShare::SIZE],
            )],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(10))],
        )
        .unwrap();

        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()))
    };

    let transaction_one = make_transaction(0x42, 0x43, 0x44);

    let hash_one = transaction_one.id().expect("first transaction ID");

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    let block_one = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction_one],
    )
    .expect("construct first block");

    ledger
        .chain
        .insert_block(block_one)
        .expect("insert first block");

    // First lookup builds the Explorer index through height 1.
    let first_location = transaction_location(&database, &ledger, hash_one)
        .expect("first index lookup")
        .expect("first transaction indexed");

    assert_eq!(first_location.height, Height(1));
    assert_eq!(first_location.transaction_index, 0);

    let transaction_two = make_transaction(0x45, 0x46, 0x47);

    let hash_two = transaction_two.id().expect("second transaction ID");

    let block_two = Block::from_protocol_transactions(
        Height(2),
        ledger.tip_hash().expect("height-one tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction_two],
    )
    .expect("construct second block");

    ledger
        .chain
        .insert_block(block_two)
        .expect("insert second block");

    // This lookup should take the incremental extension path.
    let second_location = transaction_location(&database, &ledger, hash_two)
        .expect("second index lookup")
        .expect("second transaction indexed");

    assert_eq!(second_location.height, Height(2));
    assert_eq!(second_location.transaction_index, 0);

    // Existing entries must survive the extension.
    let first_location_after_append = transaction_location(&database, &ledger, hash_one)
        .expect("old transaction lookup after append")
        .expect("old transaction remains indexed");

    assert_eq!(first_location_after_append, first_location,);
}

#[test]
fn explorer_index_rebuilds_after_reorg_and_drops_orphan_transaction() {
    let database = test_database("explorer-index-reorg");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner_a = Address([0x51; kernel::crypto::ADDRESS_SIZE]);

    let miner_b = Address([0x52; kernel::crypto::ADDRESS_SIZE]);

    let make_transaction = |seed_byte: u8, input_byte: u8, recipient_byte: u8| {
        let mnemonic = wallet::encode_bip39_mnemonic(&[seed_byte; 16]).unwrap();

        let sender = wallet::account_wallet_from_bip39_mnemonic(
            &mnemonic,
            kernel::crypto::Signature::MlDsa44,
        )
        .unwrap();

        let recipient = Address([recipient_byte; kernel::crypto::ADDRESS_SIZE]);

        let intent = kernel::transaction::SpendIntent::coin(
            sender.address,
            vec![kernel::monetary::coin::CoinShare::from_bytes(
                [input_byte; kernel::monetary::coin::CoinShare::SIZE],
            )],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(10))],
        )
        .unwrap();

        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()))
    };

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    // Canonical branch A.
    let transaction_a = make_transaction(0x53, 0x54, 0x55);

    let hash_a = transaction_a.id().expect("branch A transaction ID");

    let block_a = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner_a, Zeno::from_zeno(1))),
        vec![transaction_a],
    )
    .expect("construct branch A block");

    ledger
        .chain
        .insert_block(block_a)
        .expect("insert branch A block");

    // Build index against branch A.
    assert!(
        transaction_location(&database, &ledger, hash_a,)
            .expect("branch A lookup")
            .is_some()
    );

    let branch_a_tip = ledger.tip_hash().expect("branch A tip");

    ledger
        .chain
        .remove_tip(branch_a_tip)
        .expect("remove branch A tip");

    // Alternative branch B at the same height.
    let transaction_b = make_transaction(0x56, 0x57, 0x58);

    let hash_b = transaction_b.id().expect("branch B transaction ID");

    let block_b = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip after reorg"),
        target_bits,
        Nonce(1),
        Some(Emission::new(miner_b, Zeno::from_zeno(1))),
        vec![transaction_b],
    )
    .expect("construct branch B block");

    ledger
        .chain
        .insert_block(block_b)
        .expect("insert branch B block");

    // Tip height is still 1, but tip hash changed.
    // refresh_explorer_index() must rebuild, not extend.
    assert!(
        transaction_location(&database, &ledger, hash_a,)
            .expect("orphan transaction lookup")
            .is_none(),
        "orphan transaction remained in explorer index",
    );

    let location_b = transaction_location(&database, &ledger, hash_b)
        .expect("branch B lookup")
        .expect("branch B transaction indexed");

    assert_eq!(location_b.height, Height(1));
    assert_eq!(location_b.transaction_index, 0);
}

#[test]
fn explorer_address_index_rebuilds_after_reorg() {
    let database = test_database("explorer-address-index-reorg");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner_a = Address([0x61; kernel::crypto::ADDRESS_SIZE]);
    let miner_b = Address([0x62; kernel::crypto::ADDRESS_SIZE]);

    let recipient_a = Address([0x63; kernel::crypto::ADDRESS_SIZE]);
    let recipient_b = Address([0x64; kernel::crypto::ADDRESS_SIZE]);

    let make_transaction = |seed_byte: u8, input_byte: u8, recipient: Address| {
        let mnemonic = wallet::encode_bip39_mnemonic(&[seed_byte; 16]).unwrap();

        let sender = wallet::account_wallet_from_bip39_mnemonic(
            &mnemonic,
            kernel::crypto::Signature::MlDsa44,
        )
        .unwrap();

        let intent = kernel::transaction::SpendIntent::coin(
            sender.address,
            vec![kernel::monetary::coin::CoinShare::from_bytes(
                [input_byte; kernel::monetary::coin::CoinShare::SIZE],
            )],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(10))],
        )
        .unwrap();

        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()))
    };

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    let transaction_a = make_transaction(0x65, 0x66, recipient_a);

    let block_a = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner_a, Zeno::from_zeno(1))),
        vec![transaction_a],
    )
    .expect("construct branch A block");

    ledger
        .chain
        .insert_block(block_a)
        .expect("insert branch A block");

    let recipient_a_activities = address_activity_locations(&database, &ledger, recipient_a)
        .expect("branch A recipient activities");

    assert_eq!(
        recipient_a_activities,
        vec![ActivityLocation::Transaction {
            height: Height(1),
            transaction_index: 0,
        }],
    );

    let miner_a_activities =
        address_activity_locations(&database, &ledger, miner_a).expect("branch A miner activities");

    assert!(miner_a_activities.contains(&ActivityLocation::Emission { height: Height(1) },));

    let branch_a_tip = ledger.tip_hash().expect("branch A tip");

    ledger
        .chain
        .remove_tip(branch_a_tip)
        .expect("remove branch A tip");

    let transaction_b = make_transaction(0x67, 0x68, recipient_b);

    let block_b = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip after reorg"),
        target_bits,
        Nonce(1),
        Some(Emission::new(miner_b, Zeno::from_zeno(1))),
        vec![transaction_b],
    )
    .expect("construct branch B block");

    ledger
        .chain
        .insert_block(block_b)
        .expect("insert branch B block");

    assert!(
        address_activity_locations(&database, &ledger, recipient_a,)
            .expect("orphan recipient lookup")
            .is_empty(),
        "orphan recipient activity remained indexed",
    );

    assert!(
        address_activity_locations(&database, &ledger, miner_a,)
            .expect("orphan miner lookup")
            .is_empty(),
        "orphan emission remained indexed",
    );

    assert_eq!(
        address_activity_locations(&database, &ledger, recipient_b,)
            .expect("branch B recipient lookup"),
        vec![ActivityLocation::Transaction {
            height: Height(1),
            transaction_index: 0,
        }],
    );

    assert!(
        address_activity_locations(&database, &ledger, miner_b,)
            .expect("branch B miner lookup")
            .contains(&ActivityLocation::Emission { height: Height(1) },)
    );

    // Simulate missing/corrupted rebuildable persistent indexes.
    crate::storage::clear_canonical_indexes_for_test(&database)
        .expect("clear persistent canonical indexes");

    assert_eq!(
        crate::storage::canonical_index_tip(&database).expect("read cleared canonical index tip"),
        None,
    );

    assert!(
        crate::storage::read_address_activities(&database, recipient_b.0,)
            .expect("read cleared recipient activity index")
            .is_empty(),
    );

    assert!(
        crate::storage::read_address_activities(&database, miner_b.0,)
            .expect("read cleared miner activity index")
            .is_empty(),
    );

    // First address lookup must rebuild all persistent canonical indexes.
    let rebuilt_recipient = address_activity_locations(&database, &ledger, recipient_b)
        .expect("rebuild recipient address activity index");

    assert_eq!(
        rebuilt_recipient,
        vec![ActivityLocation::Transaction {
            height: Height(1),
            transaction_index: 0,
        }],
    );

    let rebuilt_miner = address_activity_locations(&database, &ledger, miner_b)
        .expect("lookup rebuilt miner activity index");

    assert!(rebuilt_miner.contains(&ActivityLocation::Emission { height: Height(1) },),);

    assert_eq!(
        crate::storage::canonical_index_tip(&database).expect("read rebuilt canonical index tip"),
        Some((
            ledger.tip_height().expect("canonical tip height").0,
            ledger.tip_hash().expect("canonical tip hash").0,
        )),
    );

    assert!(
        !crate::storage::read_address_activities(&database, recipient_b.0,)
            .expect("read rebuilt recipient activity index")
            .is_empty(),
    );
}

#[test]
fn discovered_peer_response_is_bounded() {
    let peers = (0..MAX_DISCOVERED_PEERS)
        .map(|index| format!("8.8.{}.{}:6677", index / 255, index % 255))
        .collect::<Vec<_>>();
    let encoded = canonical_bytes(&peers).unwrap();
    assert!(encoded.len() <= MAX_PEERS_RESPONSE_SIZE);
}

#[test]
fn explorer_address_pagination_uses_exclusive_cursor() {
    let database = test_database("explorer-address-pagination");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x91; kernel::crypto::ADDRESS_SIZE]);

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    for height in 1_u64..=5 {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("canonical tip"),
            target_bits,
            Nonce(height),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("construct pagination block");

        ledger
            .chain
            .insert_block(block)
            .expect("insert pagination block");
    }

    // Page 1: newest two activities.
    let page1 = address_activity_page(&database, &ledger, miner, None, 2)
        .expect("read first activity page");

    assert_eq!(
        page1.locations,
        vec![
            ActivityLocation::Emission { height: Height(5) },
            ActivityLocation::Emission { height: Height(4) },
        ],
    );

    let cursor1 = page1.next_cursor.expect("first page has next cursor");

    // Cursor must be exclusive: height 4 must not appear again.
    let page2 = address_activity_page(&database, &ledger, miner, Some(cursor1), 2)
        .expect("read second activity page");

    assert_eq!(
        page2.locations,
        vec![
            ActivityLocation::Emission { height: Height(3) },
            ActivityLocation::Emission { height: Height(2) },
        ],
    );

    let cursor2 = page2.next_cursor.expect("second page has next cursor");

    let page3 = address_activity_page(&database, &ledger, miner, Some(cursor2), 2)
        .expect("read final activity page");

    assert_eq!(
        page3.locations,
        vec![ActivityLocation::Emission { height: Height(1) }],
    );

    assert_eq!(page3.next_cursor, None);

    // Combined result proves there was no duplicate or skipped entry.
    let heights = page1
        .locations
        .iter()
        .chain(&page2.locations)
        .chain(&page3.locations)
        .map(|location| match location {
            ActivityLocation::Emission { height } => height.0,
            ActivityLocation::Transaction { .. } => {
                panic!("unexpected transaction activity")
            }
        })
        .collect::<Vec<_>>();

    assert_eq!(heights, vec![5, 4, 3, 2, 1]);
}

#[test]
fn rpc_request_reader_accepts_fragmented_binary_body() {
    let request = read_test_http_request(&[
        b"POST /transaction HTTP/1.1\r\nContent-Len",
        b"gth: 4\r\nConnection: close\r\n\r\n\x00\x01",
        b"\x02\x03",
    ])
    .unwrap();

    assert!(request.headers.starts_with("POST /transaction HTTP/1.1"));
    assert_eq!(request.body, [0, 1, 2, 3]);
}

#[test]
fn rpc_request_reader_rejects_ambiguous_or_oversized_framing() {
    let duplicate = read_test_http_request(&[
        b"POST /transaction HTTP/1.1\r\nContent-Length: 0\r\nContent-Length: 0\r\n\r\n",
    ])
    .unwrap_err();
    assert!(duplicate.contains("duplicate RPC Content-Length"));

    let transfer = read_test_http_request(&[
        b"POST /transaction HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n",
    ])
    .unwrap_err();
    assert!(transfer.contains("Transfer-Encoding"));

    let oversized = format!(
        "POST /transaction HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
        MAX_STORED_TRANSACTION_SIZE + 1
    );
    let oversized = read_test_http_request(&[oversized.as_bytes()]).unwrap_err();
    assert!(oversized.contains("exceeds transaction size limit"));
}

#[test]
fn p2p_transaction_frame_rejects_oversized_length_before_body() {
    let error = validate_frame_length(
        MAX_STORED_TRANSACTION_SIZE + 2,
        MAX_STORED_TRANSACTION_SIZE + 1,
    )
    .unwrap_err();
    assert!(error.contains("outside allowed range"));
    assert!(
        validate_session_frame_length(MAX_STORED_TRANSACTION_SIZE + 2, SUBMIT_TRANSACTION_MESSAGE,)
            .is_err()
    );
    assert!(
        validate_session_frame_length(MAX_STORED_TRANSACTION_SIZE + 2, SUBMIT_BLOCK_MESSAGE,)
            .is_ok()
    );
}

#[test]
fn explorer_index_rebuilds_after_deep_reorg_to_longer_branch() {
    let database = test_database("explorer-index-deep-reorg");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x71; kernel::crypto::ADDRESS_SIZE]);

    let make_transaction = |seed_byte: u8, input_byte: u8, recipient_byte: u8| {
        let mnemonic = wallet::encode_bip39_mnemonic(&[seed_byte; 16]).unwrap();

        let sender = wallet::account_wallet_from_bip39_mnemonic(
            &mnemonic,
            kernel::crypto::Signature::MlDsa44,
        )
        .unwrap();

        let recipient = Address([recipient_byte; kernel::crypto::ADDRESS_SIZE]);

        let intent = kernel::transaction::SpendIntent::coin(
            sender.address,
            vec![kernel::monetary::coin::CoinShare::from_bytes(
                [input_byte; kernel::monetary::coin::CoinShare::SIZE],
            )],
            vec![CoinOutput::new(recipient, Zeno::from_zeno(10))],
        )
        .unwrap();

        AuthorizedTransaction::Spend(Box::new(sender.sign_account_intent(intent).unwrap()))
    };

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    // Branch A: heights 1 → 2.
    let transaction_a = make_transaction(0x72, 0x73, 0x74);

    let hash_a = transaction_a.id().expect("branch A transaction ID");

    let block_a1 = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction_a],
    )
    .expect("branch A height 1");

    ledger.chain.insert_block(block_a1).unwrap();

    let block_a2 = Block::from_protocol_transactions(
        Height(2),
        ledger.tip_hash().expect("branch A height-one tip"),
        target_bits,
        Nonce(0),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![],
    )
    .expect("branch A height 2");

    ledger.chain.insert_block(block_a2).unwrap();

    // Build index at branch A height 2.
    assert!(
        transaction_location(&database, &ledger, hash_a)
            .unwrap()
            .is_some()
    );

    // Roll back branch A completely.
    for _ in 0..2 {
        let tip = ledger.tip_hash().expect("branch A tip");
        ledger.chain.remove_tip(tip).unwrap();
    }

    // Branch B: heights 1 → 2 → 3.
    let transaction_b = make_transaction(0x75, 0x76, 0x77);

    let hash_b = transaction_b.id().expect("branch B transaction ID");

    let block_b1 = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(1),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![transaction_b],
    )
    .expect("branch B height 1");

    ledger.chain.insert_block(block_b1).unwrap();

    for height in [2_u64, 3] {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("branch B tip"),
            target_bits,
            Nonce(height),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("branch B block");

        ledger.chain.insert_block(block).unwrap();
    }

    // New tip is higher than indexed tip, but old height-2 hash
    // is no longer canonical. Index must rebuild, not extend.
    assert!(
        transaction_location(&database, &ledger, hash_a)
            .unwrap()
            .is_none(),
        "orphan transaction survived deep reorg",
    );

    let location_b = transaction_location(&database, &ledger, hash_b)
        .unwrap()
        .expect("branch B transaction indexed");

    assert_eq!(location_b.height, Height(1));
    assert_eq!(location_b.transaction_index, 0);
}

#[test]
fn block_index_extends_and_rebuilds_after_reorg() {
    let database = test_database("block-index-reorg");

    let mut ledger = kernel::genesis::genesis_ledger().expect("genesis ledger");

    let miner = Address([0x81; kernel::crypto::ADDRESS_SIZE]);

    let target_bits = ledger
        .chain
        .block(&Height(0))
        .expect("genesis block")
        .target_bits();

    let block_a1 = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip"),
        target_bits,
        Nonce(1),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![],
    )
    .expect("branch A block 1");

    let hash_a1 = block_a1.hash().expect("branch A hash 1").0;

    ledger
        .chain
        .insert_block(block_a1)
        .expect("insert branch A block 1");

    // Initial build.
    assert_eq!(
        canonical_block_height(&database, &ledger, hash_a1).unwrap(),
        Some(Height(1)),
    );

    let block_a2 = Block::from_protocol_transactions(
        Height(2),
        ledger.tip_hash().expect("branch A tip"),
        target_bits,
        Nonce(2),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![],
    )
    .expect("branch A block 2");

    let hash_a2 = block_a2.hash().expect("branch A hash 2").0;

    ledger
        .chain
        .insert_block(block_a2)
        .expect("insert branch A block 2");

    // Incremental extension.
    assert_eq!(
        canonical_block_height(&database, &ledger, hash_a2).unwrap(),
        Some(Height(2)),
    );

    // Remove branch A completely.
    for _ in 0..2 {
        let tip = ledger.tip_hash().expect("branch A tip");
        ledger.chain.remove_tip(tip).unwrap();
    }

    let block_b1 = Block::from_protocol_transactions(
        Height(1),
        ledger.tip_hash().expect("genesis tip after rollback"),
        target_bits,
        Nonce(11),
        Some(Emission::new(miner, Zeno::from_zeno(1))),
        vec![],
    )
    .expect("branch B block 1");

    let hash_b1 = block_b1.hash().expect("branch B hash 1").0;

    ledger.chain.insert_block(block_b1).unwrap();

    for (height, nonce) in [(2_u64, 12_u64), (3_u64, 13_u64)] {
        let block = Block::from_protocol_transactions(
            Height(height),
            ledger.tip_hash().expect("branch B tip"),
            target_bits,
            Nonce(nonce),
            Some(Emission::new(miner, Zeno::from_zeno(1))),
            vec![],
        )
        .expect("branch B block");

        ledger.chain.insert_block(block).unwrap();
    }

    // Old canonical hashes must disappear after rebuild.
    assert_eq!(
        canonical_block_height(&database, &ledger, hash_a1).unwrap(),
        None,
    );

    assert_eq!(
        canonical_block_height(&database, &ledger, hash_a2).unwrap(),
        None,
    );

    assert_eq!(
        canonical_block_height(&database, &ledger, hash_b1).unwrap(),
        Some(Height(1)),
    );
}

#[test]
fn gossip_inventory_round_trips_and_rejects_excess_items_before_decode() {
    let inventory = GossipInventory {
        tip_height: Height(7),
        tip_hash: [3; 32],
        cumulative_work: Work::pow2(7).to_be_limbs(),
        cumulative_weight: 123,
        hash: vec![[4; 32], [5; 32]],
    };
    let encoded = canonical_bytes(&inventory).unwrap();
    let decoded = decode_gossip_inventory(&encoded).unwrap();
    assert_eq!(decoded.tip_height, inventory.tip_height);
    assert_eq!(decoded.tip_hash, inventory.tip_hash);
    assert_eq!(decoded.cumulative_work, inventory.cumulative_work);
    assert_eq!(decoded.cumulative_weight, inventory.cumulative_weight);
    assert_eq!(decoded.hash, inventory.hash);

    let mut oversized = encoded;
    oversized[112..116].copy_from_slice(&((MAX_GOSSIP_INVENTORY_ITEMS + 1) as u32).to_le_bytes());
    assert!(
        decode_gossip_inventory(&oversized)
            .unwrap_err()
            .contains("item count exceeds limit")
    );
}

#[test]
fn gossip_inventory_prefers_work_then_weight_then_smaller_tip_hash() {
    let inventory = |work, weight, tip_hash| GossipInventory {
        tip_height: Height(7),
        tip_hash,
        cumulative_work: Work::from_be_limbs(work).to_be_limbs(),
        cumulative_weight: weight,
        hash: Vec::new(),
    };
    let weaker = inventory([0, 0, 0, 0, 0, 0, 0, 7], 999, [1; 32]);
    let stronger = inventory([0, 0, 0, 0, 0, 0, 0, 8], 1, [9; 32]);
    assert!(inventory_preferred(&stronger, &weaker));

    let lighter = inventory([0, 0, 0, 0, 0, 0, 0, 8], 10, [1; 32]);
    let heavier = inventory([0, 0, 0, 0, 0, 0, 0, 8], 11, [9; 32]);
    assert!(inventory_preferred(&heavier, &lighter));

    let larger_hash = inventory([0, 0, 0, 0, 0, 0, 0, 8], 11, [9; 32]);
    let smaller_hash = inventory([0, 0, 0, 0, 0, 0, 0, 8], 11, [2; 32]);
    assert!(inventory_preferred(&smaller_hash, &larger_hash));
}

#[test]
fn redb_startup_round_trips_canonical_genesis() {
    let database = test_database("redb-roundtrip");
    let ledger = load_or_initialize_uncached(&database).unwrap();
    let recovered = load_existing(&database).unwrap();
    assert_eq!(recovered.tip_hash(), ledger.tip_hash());
    assert!(database.join("xparq.redb").is_file());
    fs::remove_dir_all(database).unwrap();
}

#[test]
fn startup_discards_invalid_redb_mempool_entries() {
    let database = test_database("corrupt-mempool");
    let ledger = load_or_initialize_uncached(&database).unwrap();
    crate::storage::replace_mempool(&database, &[vec![0xff, 0xff, 0xff]]).unwrap();

    recover_mempool(&database, &ledger).unwrap();

    assert!(read_mempool(&database).unwrap().is_empty());
    fs::remove_dir_all(database).unwrap();
}
