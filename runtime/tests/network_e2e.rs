use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use kernel::{
    crypto::{Signature, SigningSeed, address_from_public_key, address_to_string, canonical_bytes},
    monetary::coin::{CoinOutput, CoinShare, Zeno},
    transaction::{AuthorizedTransaction, CoinTransition, Transaction},
};
use serde_json::Value;
use wallet::{AccountWallet, account_wallet_from_bip39_mnemonic, encode_bip39_mnemonic};

const WAIT: Duration = Duration::from_secs(120);

struct NodeProcess(Child);

impl Drop for NodeProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn node_binary() -> &'static str {
    env!("CARGO_BIN_EXE_node")
}

fn temp_root(label: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "xparq-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn free_address() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.local_addr().unwrap().to_string()
}

fn miner_address() -> String {
    address_to_string(&sender_wallet().address)
}

fn sender_wallet() -> AccountWallet {
    let mnemonic = encode_bip39_mnemonic(&[42; 16]).unwrap();
    account_wallet_from_bip39_mnemonic(&mnemonic, Signature::MlDsa44).unwrap()
}

fn mine(database: &Path, blocks: u64) {
    for _ in 0..blocks {
        let status = Command::new(node_binary())
            .args(["mine-block", database.to_str().unwrap(), &miner_address()])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(status.success(), "single-block miner failed");
    }
}

fn start_node(
    database: &Path,
    p2p: &str,
    rpc: &str,
    peers: &[&str],
    miner: Option<&str>,
) -> NodeProcess {
    let mut command = Command::new(node_binary());
    command.args([
        "run",
        "--data",
        database.to_str().unwrap(),
        "--p2p",
        p2p,
        "--rpc",
        rpc,
    ]);
    for peer in peers {
        command.args(["--peer", peer]);
    }
    if let Some(miner) = miner {
        command.args(["--miner", miner]);
    }
    NodeProcess(
        command
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    )
}

fn status(rpc: &str) -> Result<Value, String> {
    http_get(rpc, "/status")
}

fn http_get(rpc: &str, route: &str) -> Result<Value, String> {
    let mut stream = TcpStream::connect(rpc).map_err(|error| error.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(
            format!("GET {route} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .map_err(|error| error.to_string())?;
    let mut response = Vec::new();
    stream
        .read_to_end(&mut response)
        .map_err(|error| error.to_string())?;
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| &response[position + 4..])
        .ok_or("HTTP response has no body")?;
    serde_json::from_slice(body).map_err(|error| error.to_string())
}

fn post_transaction(rpc: &str, transaction: &Transaction) -> Value {
    post_program_rpc(rpc, "/transaction", transaction)
}

fn post_program_rpc(rpc: &str, route: &str, transaction: &Transaction) -> Value {
    let body = canonical_bytes(transaction).unwrap();
    let mut stream = TcpStream::connect(rpc).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    write!(
        stream,
        "POST {route} HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .unwrap();
    stream.write_all(&body).unwrap();
    let mut response = Vec::new();
    stream.read_to_end(&mut response).unwrap();
    assert!(
        response.starts_with(b"HTTP/1.1 200"),
        "transaction submission failed: {}",
        String::from_utf8_lossy(&response)
    );
    let body = response
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .map(|position| &response[position + 4..])
        .unwrap();
    serde_json::from_slice(body).unwrap()
}

fn account(rpc: &str, address: &str) -> Result<Value, String> {
    http_get(rpc, &format!("/account/{address}"))
}

fn wait_for_status(rpc: &str, predicate: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Ok(value) = status(rpc)
            && predicate(&value)
        {
            return value;
        }
        assert!(Instant::now() < deadline, "timed out waiting for {rpc}");
        thread::sleep(Duration::from_millis(100));
    }
}

fn copy_tree(source: &Path, destination: &Path) {
    fs::create_dir_all(destination).unwrap();
    for entry in fs::read_dir(source).unwrap() {
        let entry = entry.unwrap();
        let target = destination.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), target).unwrap();
        }
    }
}

#[test]
fn block_gossip_crosses_three_nodes_and_survives_restart() {
    let root = temp_root("three-node-gossip");
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");
    // Build the fixture with the current schema and chain identity.
    mine(&a, 1);

    let a_p2p = free_address();
    let a_rpc = free_address();
    let b_p2p = free_address();
    let b_rpc = free_address();
    let c_p2p = free_address();
    let c_rpc = free_address();
    let a_node = start_node(&a, &a_p2p, &a_rpc, &[], None);
    wait_for_status(&a_rpc, |_| true);
    // A reachable peer that never handshakes must not hold the chain sync lock.
    let silent_peer = TcpListener::bind("127.0.0.1:0").unwrap();
    let silent_address = silent_peer.local_addr().unwrap().to_string();
    let started = Instant::now();
    let b_node = start_node(&b, &b_p2p, &b_rpc, &[&silent_address, &a_p2p], None);
    wait_for_status(&b_rpc, |_| true);
    let c_node = start_node(&c, &c_p2p, &c_rpc, &[&b_p2p], None);
    let synced = wait_for_status(&c_rpc, |status| status["tip_height"] == 1);
    assert!(
        started.elapsed() < Duration::from_secs(30),
        "healthy peer sync waited for the silent peer's 60-second handshake timeout"
    );
    let expected_tip = synced["tip_hash"].clone();

    drop(c_node);
    let c_node = start_node(&c, &c_p2p, &c_rpc, &[&b_p2p], None);
    wait_for_status(&c_rpc, |status| {
        status["tip_height"] == 1 && status["tip_hash"] == expected_tip
    });

    drop(c_node);
    drop(b_node);
    drop(a_node);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn lower_work_node_reorgs_to_a_longer_valid_fork() {
    let root = temp_root("higher-work-reorg");
    let common = root.join("common");
    let weaker = root.join("weaker");
    let stronger = root.join("stronger");
    mine(&common, 1);
    copy_tree(&common, &weaker);
    copy_tree(&common, &stronger);
    mine(&weaker, 1);
    mine(&stronger, 2);

    let strong_p2p = free_address();
    let strong_rpc = free_address();
    let weak_p2p = free_address();
    let weak_rpc = free_address();
    let strong_node = start_node(&stronger, &strong_p2p, &strong_rpc, &[], None);
    let expected = wait_for_status(&strong_rpc, |status| status["tip_height"] == 3);
    let expected_tip = expected["tip_hash"].clone();
    let weak_node = start_node(&weaker, &weak_p2p, &weak_rpc, &[&strong_p2p], None);

    wait_for_status(&weak_rpc, |status| {
        status["tip_height"] == 3 && status["tip_hash"] == expected_tip
    });

    drop(weak_node);
    drop(strong_node);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn stronger_outbound_node_makes_weaker_inbound_node_reorg() {
    let root = temp_root("reverse-higher-work-reorg");
    let common = root.join("common");
    let weaker = root.join("weaker");
    let stronger = root.join("stronger");
    mine(&common, 1);
    copy_tree(&common, &weaker);
    copy_tree(&common, &stronger);
    mine(&weaker, 1);
    mine(&stronger, 2);

    let weak_p2p = free_address();
    let weak_rpc = free_address();
    let strong_p2p = free_address();
    let strong_rpc = free_address();
    let weak_node = start_node(&weaker, &weak_p2p, &weak_rpc, &[], None);
    wait_for_status(&weak_rpc, |status| status["tip_height"] == 2);
    let strong_node = start_node(&stronger, &strong_p2p, &strong_rpc, &[&weak_p2p], None);
    let expected = wait_for_status(&strong_rpc, |status| status["tip_height"] == 3);
    let expected_tip = expected["tip_hash"].clone();

    wait_for_status(&weak_rpc, |status| {
        status["tip_height"] == 3 && status["tip_hash"] == expected_tip
    });

    drop(strong_node);
    drop(weak_node);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn signed_wallet_transaction_gossips_is_mined_and_survives_restart() {
    let root = temp_root("signed-transaction");
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");

    let a_p2p = free_address();
    let a_rpc = free_address();
    let b_p2p = free_address();
    let b_rpc = free_address();
    let c_p2p = free_address();
    let c_rpc = free_address();
    mine(&a, 2);
    let mut a_node = start_node(&a, &a_p2p, &a_rpc, &[], None);
    wait_for_status(&a_rpc, |status| status["tip_height"] == 2);
    let b_node = start_node(&b, &b_p2p, &b_rpc, &[&a_p2p], None);
    wait_for_status(&b_rpc, |status| status["tip_height"] == 2);
    let mut c_node = start_node(&c, &c_p2p, &c_rpc, &[&b_p2p], None);
    wait_for_status(&c_rpc, |status| status["tip_height"] == 2);

    let sender = sender_wallet();
    let sender_address = address_to_string(&sender.address);
    let recipient_keys = SigningSeed::new(Signature::MlDsa44, Box::new([43; 32]));
    let recipient = address_from_public_key(&recipient_keys.public_key());
    let recipient_address = address_to_string(&recipient);
    let sender_account = account(&a_rpc, &sender_address).unwrap();
    let available = sender_account["utxos"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|utxo| !utxo["reserved"].as_bool().unwrap_or(false))
        .collect::<Vec<_>>();
    let input = available
        .first()
        .expect("available miner reward is missing");
    let input_id: CoinShare = input["id"].as_str().unwrap().parse().unwrap();
    let input_amount = input["amount"].as_u64().unwrap();
    let sent = Zeno::from_zeno(1);
    let state_burn = kernel::consensus::StateTransitionWeight {
        created_coin_utxos: 3,
        consumed_coin_utxos: 1,
        ..kernel::consensus::StateTransitionWeight::default()
    }
    .state_growth_burn()
    .unwrap();
    // Both canonical history burn and relay fee depend on the signed size.
    let mut archival_bytes = 0;
    let transaction = loop {
        let burn = state_burn.as_zeno() + archival_bytes;
        let intent = CoinTransition::coin_with_charges(
            sender.address,
            vec![input_id],
            vec![
                CoinOutput::new(recipient, sent),
                CoinOutput::new(
                    sender.address,
                    Zeno::from_zeno(input_amount - sent.as_zeno() - burn - archival_bytes.max(1)),
                ),
            ],
            kernel::transaction::CoinCharges::new(Zeno::from_zeno(archival_bytes.max(1))),
        )
        .unwrap();
        let transaction =
            AuthorizedTransaction::Program(Box::new(sender.sign_xpq_transfer(intent).unwrap()));
        let required = (canonical_bytes(&transaction).unwrap().len() as u64)
            .checked_mul(8)
            .unwrap();
        if required == archival_bytes {
            break transaction;
        }
        archival_bytes = required;
    };
    let transaction_hash = hex::encode(transaction.id().unwrap());
    let submitted = post_transaction(&a_rpc, &transaction);
    assert_eq!(submitted["hash"], transaction_hash);

    wait_for_status(&c_rpc, |_| {
        account(&c_rpc, &sender_address).is_ok_and(|account| {
            account["utxos"]
                .as_array()
                .is_some_and(|utxos| utxos.iter().any(|utxo| utxo["reserved"] == true))
        })
    });

    drop(a_node);
    mine(&a, 1);
    a_node = start_node(&a, &a_p2p, &a_rpc, &[], None);
    wait_for_status(&c_rpc, |_| {
        account(&c_rpc, &recipient_address).is_ok_and(|account| {
            account["total"]
                .as_u64()
                .is_some_and(|total| u128::from(total) >= u128::from(sent.as_zeno()))
        })
    });
    let included_balance = account(&c_rpc, &recipient_address).unwrap()["total"].clone();

    drop(c_node);
    c_node = start_node(&c, &c_p2p, &c_rpc, &[&b_p2p], None);
    wait_for_status(&c_rpc, |_| {
        account(&c_rpc, &recipient_address)
            .is_ok_and(|account| account["total"] == included_balance)
    });

    let transaction_response =
        http_get(&c_rpc, &format!("/explorer/transaction/{transaction_hash}"))
            .expect("transaction explorer lookup after restart");

    assert_eq!(transaction_response["hash"], transaction_hash);
    assert_eq!(transaction_response["status"], "confirmed");
    assert_eq!(transaction_response["height"], 3);

    let address_response = http_get(&c_rpc, &format!("/explorer/address/{recipient_address}"))
        .expect("address explorer lookup after restart");

    let activities = address_response["activities"]
        .as_array()
        .expect("address activities");

    assert!(
        activities.iter().any(|activity| {
            activity["hash"] == transaction_hash
                && activity["direction"] == "in"
                && activity["amount"] == sent.as_zeno()
        }),
        "recipient transaction activity was not rebuilt after restart",
    );

    drop(c_node);
    drop(b_node);
    drop(a_node);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn program_call_is_accepted_mined_and_replayed_after_redb_restart() {
    use extension::{
        asset_program::{
            asset::Unit,
            opcode::AssetOpcode,
            state::{AssetState, ExecutionContext},
            type_::{AssetCall, Register},
        },
        script::call::{ProgramCall, ProgramId},
    };
    use kernel::{
        consensus::{ProtocolBurn, StateTransitionWeight},
        transaction::{
            AccountAuthorization, AuthorizedProgramTransaction, CoinCharges,
            program_transaction_commitment,
        },
    };
    let root = temp_root("program-call");
    let keys = SigningSeed::new(Signature::MlDsa44, Box::new([51; 32]));
    let signer = address_from_public_key(&keys.public_key());
    let address = address_to_string(&signer);
    let mine_program = || {
        let result = Command::new(node_binary())
            .args(["mine-block", root.to_str().unwrap(), &address])
            .stdout(Stdio::null())
            .status()
            .unwrap();
        assert!(result.success());
    };
    mine_program();
    let rpc = free_address();
    let p2p = free_address();
    let node = start_node(&root, &p2p, &rpc, &[], None);
    wait_for_status(&rpc, |s| s["tip_height"] == 1);
    let account = account(&rpc, &address).unwrap();
    let input = &account["utxos"].as_array().unwrap()[0];
    let id: CoinShare = input["id"].as_str().unwrap().parse().unwrap();
    let amount = input["amount"].as_u64().unwrap();
    let register = Register {
        name: "LIVEPROGRAM".into(),
        max_supply: Unit::from_units(100),
        initial_mint: Unit::from_units(10),
        mint_authority: signer,
        nonce: 1,
    };
    let call = ProgramCall {
        program: ProgramId::ASSET,
        opcode: AssetOpcode::Register as u8,
        payload: borsh::to_vec(&register).unwrap(),
    };
    let mut preview = AssetState::default();
    let before = canonical_bytes(&preview).unwrap().len();
    preview
        .apply(
            &AssetCall::Register(register),
            ExecutionContext {
                signer,
                commitment: [1; 32],
            },
        )
        .unwrap();
    let growth = (canonical_bytes(&preview).unwrap().len() - before) as u64;
    let chain = kernel::genesis::chain_context().unwrap();
    let mut size = 0;
    let transaction = loop {
        let fee = (size * 8).max(1);
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
        .unwrap()
        .as_zeno();
        let payment = CoinTransition::coin_with_charges(
            signer,
            vec![id],
            vec![CoinOutput::new(
                signer,
                Zeno::from_zeno(amount - burn - fee),
            )],
            CoinCharges::new(Zeno::from_zeno(fee)),
        )
        .unwrap();
        let commitment = program_transaction_commitment(signer, &call, &payment, chain).unwrap();
        let tx = AuthorizedTransaction::Program(Box::new(AuthorizedProgramTransaction {
            signer,
            call: call.clone(),
            payment,
            authorization: AccountAuthorization {
                public_key: keys.public_key(),
                signature: keys.sign(commitment.as_bytes()),
            },
        }));
        let actual = canonical_bytes(&tx).unwrap().len() as u64;
        if actual == size {
            break tx;
        }
        size = actual;
    };
    let hash = hex::encode(transaction.id().unwrap());
    assert_eq!(post_transaction(&rpc, &transaction)["hash"], hash);
    drop(node);
    mine_program();
    let node = start_node(&root, &p2p, &rpc, &[], None);
    let mined = wait_for_status(&rpc, |s| s["tip_height"] == 2);
    let block = http_get(&rpc, "/block/2").unwrap();
    assert_eq!(block["hash"], mined["tip_hash"]);
    assert_eq!(
        block["transaction_hashes"],
        serde_json::json!([hash.clone()])
    );
    assert_eq!(block["transaction_details"][0]["hash"], hash);
    let response = http_get(&rpc, &format!("/explorer/transaction/{hash}")).unwrap();
    assert_eq!(response["status"], "confirmed");
    assert_eq!(response["height"], 2);
    drop(node);
    let node = start_node(&root, &p2p, &rpc, &[], None);
    assert_eq!(
        wait_for_status(&rpc, |s| s["tip_height"] == 2)["tip_hash"],
        mined["tip_hash"]
    );
    drop(node);
    assert!(
        Command::new(node_binary())
            .args(["check", root.to_str().unwrap()])
            .stdout(Stdio::null())
            .status()
            .unwrap()
            .success()
    );
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn advertised_dns_is_discovered_and_persisted_across_restart() {
    use redb::{ReadableDatabase, TableDefinition};
    let root = temp_root("ddns-discovery");
    let a = root.join("a");
    let b = root.join("b");
    let a_p2p = free_address();
    let a_rpc = free_address();
    let server = NodeProcess(
        Command::new(node_binary())
            .args([
                "run",
                "--data",
                a.to_str().unwrap(),
                "--p2p",
                &a_p2p,
                "--rpc",
                &a_rpc,
                "--public-addr",
                "node.example.invalid:6677",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .unwrap(),
    );
    wait_for_status(&a_rpc, |_| true);
    let connected = Command::new(node_binary())
        .args(["peer", b.to_str().unwrap(), &a_p2p])
        .stdout(Stdio::null())
        .status()
        .unwrap();
    assert!(connected.success());
    drop(server);

    let check_stored = || {
        let db = redb::Database::open(b.join("xparq.redb")).unwrap();
        let read = db.begin_read().unwrap();
        let table = read
            .open_table(TableDefinition::<&str, &[u8]>::new("auxiliary"))
            .unwrap();
        let bytes = table.get("peers").unwrap().unwrap();
        let peers: Value = serde_json::from_slice(bytes.value()).unwrap();
        assert!(
            peers["peers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|record| record["address"] == "node.example.invalid:6677")
        );
    };
    check_stored();
    let b_rpc = free_address();
    let restarted = start_node(&b, &free_address(), &b_rpc, &[], None);
    wait_for_status(&b_rpc, |_| true);
    drop(restarted);
    check_stored();
    fs::remove_dir_all(root).unwrap();
}

mod program_network;
