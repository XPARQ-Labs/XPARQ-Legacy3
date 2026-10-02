//! Run after `cargo build -p node --bins`; exercises the actual CLI and redb node.
use serde_json::Value;
use std::{
    fs,
    io::{Read, Write},
    net::{TcpListener, TcpStream},
    path::Path,
    process::{Child, Command, Stdio},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
struct Node(Child);
impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}
fn get(rpc: &str, route: &str) -> Value {
    let mut stream = TcpStream::connect(rpc).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .unwrap();
    write!(
        stream,
        "GET {route} HTTP/1.1\r\nHost: {rpc}\r\nConnection: close\r\n\r\n"
    )
    .unwrap();
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).unwrap();
    let start = bytes.windows(4).position(|s| s == b"\r\n\r\n").unwrap() + 4;
    let value: Value = serde_json::from_slice(&bytes[start..]).unwrap();
    assert!(bytes.starts_with(b"HTTP/1.1 200"), "{value}");
    value
}
fn free() -> String {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .to_string()
}
fn start(binary: &Path, db: &Path, rpc: &str, p2p: &str) -> Node {
    let child = Command::new(binary)
        .args([
            "run",
            "--data",
            db.to_str().unwrap(),
            "--rpc",
            rpc,
            "--p2p",
            p2p,
        ])
        .stdout(Stdio::null())
        .spawn()
        .unwrap();
    let mut node = Node(child);
    let deadline = Instant::now() + Duration::from_secs(30);
    while TcpStream::connect(rpc).is_err() {
        assert!(node.0.try_wait().unwrap().is_none(), "node exited");
        assert!(Instant::now() < deadline);
        thread::sleep(Duration::from_millis(50));
    }
    node
}
fn mine(binary: &Path, db: &Path, address: &str) {
    let result = Command::new(binary)
        .args(["mine-block", db.to_str().unwrap(), address])
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
}
fn cli(wallet: &Path, rpc: &str, command: &str, args: &[&str]) -> String {
    let result = Command::new(env!("CARGO_BIN_EXE_wallet"))
        .args([command, "--wallet", wallet.to_str().unwrap(), "--rpc", rpc])
        .args(args)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    String::from_utf8(result.stdout).unwrap()
}
#[test]
#[ignore = "requires node binary; cargo build -p node --bins then run explicitly"]
fn program_wallet_cli_lifecycle_and_recipient_history_survive_restart() {
    let node_binary = Path::new(env!("CARGO_BIN_EXE_wallet")).with_file_name(if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    });
    assert!(node_binary.exists(), "build the node binary first");
    let root = std::env::temp_dir().join(format!(
        "xparq-wallet-program-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let db = root.join("data");
    let file = root.join("wallet.json");
    let words = wallet::encode_bip39_mnemonic(&[63; 16]).unwrap();
    let mut owner =
        wallet::account_wallet_from_bip39_mnemonic(&words, kernel::crypto::Signature::MlDsa44)
            .unwrap();
    owner.mnemonic = Some(words.clone());
    fs::write(&file, &*wallet::account_wallet_file_bytes(&owner).unwrap()).unwrap();
    let address = kernel::crypto::address_to_string(&owner.address);
    let receiver = wallet::account_wallet_from_bip39_mnemonic(
        &wallet::encode_bip39_mnemonic(&[64; 16]).unwrap(),
        kernel::crypto::Signature::MlDsa44,
    )
    .unwrap();
    let to = kernel::crypto::address_to_string(&receiver.address);
    mine(&node_binary, &db, &address);
    let rpc = free();
    let p2p = free();
    let mut node = start(&node_binary, &db, &rpc, &p2p);
    let output = cli(
        &file,
        &rpc,
        "program-register",
        &[
            "--name",
            "CLIPROGRAM",
            "--max-supply",
            "100",
            "--initial-mint",
            "40",
        ],
    );
    let asset = output
        .lines()
        .find_map(|line| line.strip_prefix("program asset: "))
        .unwrap()
        .to_string();
    drop(node);
    mine(&node_binary, &db, &address);
    node = start(&node_binary, &db, &rpc, &p2p);
    for (command, args) in [
        (
            "program-mint",
            vec![
                "--asset",
                asset.as_str(),
                "--to",
                address.as_str(),
                "--amount",
                "20",
            ],
        ),
        (
            "program-transfer",
            vec![
                "--asset",
                asset.as_str(),
                "--to",
                to.as_str(),
                "--amount",
                "15",
            ],
        ),
        (
            "program-burn",
            vec!["--asset", asset.as_str(), "--amount", "5"],
        ),
        ("program-consolidate", vec!["--asset", asset.as_str()]),
    ] {
        cli(&file, &rpc, command, &args);
        drop(node);
        mine(&node_binary, &db, &address);
        node = start(&node_binary, &db, &rpc, &p2p);
    }
    let metadata = get(&rpc, &format!("/program/asset/{asset}"));
    assert_eq!(metadata["supply"], "5500000000");
    assert_eq!(metadata["total_minted"], "6000000000");
    assert_eq!(metadata["total_burned"], "500000000");
    let balance = get(&rpc, &format!("/program/asset/{asset}/balance/{address}"));
    assert_eq!(balance["balance"], "4000000000");
    assert_eq!(balance["shares"].as_array().unwrap().len(), 1);
    assert_eq!(
        get(&rpc, &format!("/program/asset/{asset}/balance/{to}"))["balance"],
        "1500000000"
    );
    let history = get(
        &rpc,
        &format!("/explorer/address/{to}?include_emissions=false"),
    );
    let activity = history["activities"]
        .as_array()
        .unwrap()
        .iter()
        .find(|v| v["program"]["operation"] == "transfer")
        .unwrap();
    assert_eq!(activity["program"]["amount"], "1500000000");
    assert_eq!(activity["direction"], "in");
    assert!(
        get(&rpc, &format!("/account/{to}"))["program_assets"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["asset"] == asset)
    );
    assert!(cli(&file, &rpc, "balance", &[]).contains("CLIPROGRAM"));
    assert!(cli(&file, &rpc, "program-info", &["--asset", &asset]).contains("CLIPROGRAM"));
    cli(&file, &rpc, "program-balance", &["--asset", &asset]);
    let offline = cli(
        &file,
        &rpc,
        "program-transfer",
        &["--asset", &asset, "--to", &to, "--amount", "1", "--offline"],
    );
    assert!(offline.contains("Transaction Hex:"));
    assert!(!offline.contains("Tx Hash:"));
    let recovered = get(&rpc, "/status")["tip_hash"].clone();
    drop(node);
    node = start(&node_binary, &db, &rpc, &p2p);
    assert_eq!(get(&rpc, "/status")["tip_hash"], recovered);
    drop(node);
    fs::remove_dir_all(root).unwrap();
}

#[test]
fn program_history_cli_displays_asset_units_as_decimal_amounts() {
    let root = std::env::temp_dir().join(format!("xparq-program-history-{}", std::process::id()));
    fs::create_dir_all(&root).unwrap();
    let file = root.join("wallet.json");
    let words = wallet::encode_bip39_mnemonic(&[65; 16]).unwrap();
    let mut owner =
        wallet::account_wallet_from_bip39_mnemonic(&words, kernel::crypto::Signature::MlDsa44)
            .unwrap();
    owner.mnemonic = Some(words);
    fs::write(&file, &*wallet::account_wallet_file_bytes(&owner).unwrap()).unwrap();
    let address = kernel::crypto::address_to_string(&owner.address);
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let rpc = listener.local_addr().unwrap().to_string();
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = [0; 4096];
        stream.read(&mut request).unwrap();
        let body=serde_json::json!({"address":address,"tip_height":5,"emission_count":0,"activities":[{"height":4,"block_hash":"11","hash":"22","type":"program","direction":"in","amount":0,"size_bytes":3000,"program":{"operation":"transfer","asset":"33","amount":"1500000000"}}]}).to_string();
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        )
        .unwrap();
    });
    let output = cli(&file, &rpc, "history", &[]);
    assert!(output.contains("Asset Operation: transfer"));
    assert!(output.contains("Asset Amount: 15.00000000"));
    server.join().unwrap();
    fs::remove_dir_all(root).unwrap();
}

#[test]
#[ignore = "requires node binary; cargo build -p node -p wallet --bins then run explicitly"]
fn xpq_program_wallet_cli_spend_and_consolidation() {
    let binary = Path::new(env!("CARGO_BIN_EXE_wallet")).with_file_name(if cfg!(windows) {
        "node.exe"
    } else {
        "node"
    });
    assert!(binary.exists(), "build the node binary first");
    let root = std::env::temp_dir().join(format!(
        "xparq-native-program-cli-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&root).unwrap();
    let db = root.join("data");
    let file = root.join("wallet.json");
    let words = wallet::encode_bip39_mnemonic(&[74; 16]).unwrap();
    let mut owner =
        wallet::account_wallet_from_bip39_mnemonic(&words, kernel::crypto::Signature::MlDsa44)
            .unwrap();
    owner.mnemonic = Some(words);
    fs::write(&file, &*wallet::account_wallet_file_bytes(&owner).unwrap()).unwrap();
    let address = kernel::crypto::address_to_string(&owner.address);
    let receiver = wallet::account_wallet_from_bip39_mnemonic(
        &wallet::encode_bip39_mnemonic(&[75; 16]).unwrap(),
        kernel::crypto::Signature::MlDsa44,
    )
    .unwrap();
    let to = kernel::crypto::address_to_string(&receiver.address);
    mine(&binary, &db, &address);
    let rpc = free();
    let p2p = free();
    let mut node = start(&binary, &db, &rpc, &p2p);
    let output = cli(&file, &rpc, "sign-spend", &["--to", &to, "--amount", "1"]);
    let hash = output
        .lines()
        .find_map(|line| line.strip_prefix("Tx Hash: "))
        .unwrap()
        .to_string();
    drop(node);
    mine(&binary, &db, &address);
    node = start(&binary, &db, &rpc, &p2p);
    let transaction = get(&rpc, &format!("/explorer/transaction/{hash}"));
    assert_eq!(transaction["transaction"]["program_id"], 0);
    assert_eq!(transaction["transaction"]["opcode"], 1);
    assert_eq!(transaction["transaction"]["type"], "transfer");
    assert_eq!(get(&rpc, &format!("/account/{to}"))["total"], 100_000_000);
    let output = cli(&file, &rpc, "consolidate", &[]);
    let hash = output
        .lines()
        .find_map(|line| line.strip_prefix("Tx Hash: "))
        .unwrap()
        .to_string();
    drop(node);
    mine(&binary, &db, &address);
    node = start(&binary, &db, &rpc, &p2p);
    let transaction = get(&rpc, &format!("/explorer/transaction/{hash}"));
    assert_eq!(transaction["transaction"]["program_id"], 0);
    assert!(
        transaction["transaction"]["inputs"]
            .as_array()
            .unwrap()
            .len()
            >= 2
    );
    assert_eq!(
        transaction["transaction"]["outputs"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    let tip = get(&rpc, "/status")["tip_hash"].clone();
    drop(node);
    node = start(&binary, &db, &rpc, &p2p);
    assert_eq!(get(&rpc, "/status")["tip_hash"], tip);
    assert_eq!(get(&rpc, &format!("/account/{to}"))["total"], 100_000_000);
    drop(node);
    fs::remove_dir_all(root).unwrap();
}
