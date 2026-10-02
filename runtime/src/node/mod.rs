use std::{
    collections::BTreeSet,
    fs,
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock, RwLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

use crate::peer::{MAX_DISCOVERED_PEERS, PeerAddress, PeerStore, is_admissible_discovered_peer};
use crate::sync::{
    HeaderChainChunk, MAX_HEADER_CHAIN_CHUNK_HEADERS, MAX_HEADER_CHAIN_CHUNK_SIZE,
    decode_header_chain_chunk,
};
use borsh::{BorshDeserialize, BorshSerialize};
use kernel::{
    block::{Block, Emission},
    codec::{block_bytes, decode_block},
    common::{Height, Nonce},
    consensus::{
        ReorgPlan, Work, apply_block, compare_chain_tips, expected_emission_for_height,
        expected_next_difficulty, new_pow_memory, validate_transaction,
    },
    crypto::{
        Address, BlockHash, PoWMemory, address_from_string, canonical_bytes, canonical_decode,
    },
    genesis::{EXPECTED_GENESIS_HASH, chain_spec_hash, genesis_block},
    ledger::Ledger,
    monetary::coin::{CoinOutput, Zeno},
    transaction::{AuthorizedTransaction, Transaction},
};

const NODE_ID_FILE: &str = "node-id";
const MAX_STORED_BLOCK_SIZE: usize = kernel::block::MAX_BLOCK_SIZE + 1024;
const MAX_STORED_TRANSACTION_SIZE: usize = kernel::transaction::MAX_TRANSACTION_SIZE;
const MAX_STORED_MEMPOOL_SIZE: u64 = 64 * 1024 * 1024;
const MAX_RPC_HEADER_SIZE: usize = 16 * 1024;
const MAX_RPC_CONNECTIONS: usize = 8;
const MAX_RPC_CONNECTIONS_PER_IP: usize = 4;
const MAX_ACCOUNT_UTXOS_PER_PAGE: usize = 1_000;
const RPC_TIMEOUT: Duration = Duration::from_secs(10);
const OPENAPI_JSON: &[u8] = include_bytes!("../../../docs/openapi.json");
const API_DOCS_HTML: &[u8] = br#"<!doctype html>
<html lang="en">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>kernel RPC API</title>
</head>
<body>
  <script id="api-reference" data-url="/openapi.json"></script>
  <script src="https://cdn.jsdelivr.net/npm/@scalar/api-reference"></script>
</body>
</html>
"#;
const P2P_MAGIC: [u8; 8] = *b"XPQP2P01";
const P2P_PROTOCOL_VERSION: u32 = 1;
const CAPABILITY_PEER_DISCOVERY: u64 = 1 << 0;
const CAPABILITY_RELAY: u64 = 1 << 1;
const LOCAL_CAPABILITIES: u64 = CAPABILITY_PEER_DISCOVERY | CAPABILITY_RELAY;
const MAX_HANDSHAKE_SIZE: usize = 1024;
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);
const MAX_LOCATOR_HASHES: usize = 64;
const MAX_SYNC_HEADERS: usize = 100_000;
const GET_HEADERS_MESSAGE: u8 = 1;
const HEADERS_MESSAGE: u8 = 2;
const HEADERS_COMPLETE_MESSAGE: u8 = 3;
const GET_BLOCK_MESSAGE: u8 = 4;
const BLOCK_MESSAGE: u8 = 5;
const SYNC_COMPLETE_MESSAGE: u8 = 6;
const GET_PEERS_MESSAGE: u8 = 7;
const PEERS_MESSAGE: u8 = 8;
const SUBMIT_TRANSACTION_MESSAGE: u8 = 9;
const SUBMIT_BLOCK_MESSAGE: u8 = 10;
const ACCEPTED_MESSAGE: u8 = 11;
const REJECTED_MESSAGE: u8 = 12;
const INVENTORY_MESSAGE: u8 = 13;
const GET_TRANSACTION_MESSAGE: u8 = 14;
const TRANSACTION_MESSAGE: u8 = 15;
const MAX_PEERS_RESPONSE_SIZE: usize = 16 * 1024;
const MAX_RELAY_ITEMS_PER_SESSION: usize = 256;
const MAX_RELAYED_BLOCKS_PER_SESSION: usize = 4;
const MAX_HEADER_REQUESTS_PER_SESSION: usize =
    MAX_SYNC_HEADERS.div_ceil(MAX_HEADER_CHAIN_CHUNK_HEADERS) + 1;
const MAX_GOSSIP_INVENTORY_ITEMS: usize = 1_024;
const MAX_MEMPOOL_TRANSACTIONS: usize = MAX_GOSSIP_INVENTORY_ITEMS;
const MIN_RELAY_FEE_ZENO_PER_BYTE: u64 = 8;
const MAX_GOSSIP_INVENTORY_SIZE: usize = 64 * 1024;
const GOSSIP_HEARTBEAT: Duration = Duration::from_secs(2);
const MAX_OUTBOUND_CONNECTIONS: usize = 8;
const MAX_CONCURRENT_DIALS: usize = 2;
const DIAL_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_INBOUND_CONNECTIONS: usize = 64;
const MAX_INBOUND_CONNECTIONS_PER_IP: usize = 4;
const RECONNECT_INTERVAL: Duration = Duration::from_secs(10);
const INVALID_POW_COOLDOWN: Duration = Duration::from_secs(10 * 60);
const INVALID_POW_ERROR_PREFIX: &str = "peer-invalid-pow:";
const GOSSIP_RESYNC_PREFIX: &str = "gossip-resync:";
const DEFAULT_NAT_LEASE: Duration = Duration::from_secs(3_600);

struct HeaderSyncResult {
    ancestor_height: Height,
    ancestor_hash: BlockHash,
    headers: Vec<kernel::consensus::HeaderAtHeight>,
    peer_work: Work,
    peer_weight: u64,
    preferred: bool,
}

#[derive(Clone)]
struct CachedLedger {
    database: PathBuf,
    ledger: Arc<Ledger>,
    header_checkpoints: Arc<Vec<state::HeaderStateCheckpoint>>,
    cumulative_work: Work,
    cumulative_weight: u64,
}

static LEDGER_CACHE: OnceLock<RwLock<Option<CachedLedger>>> = OnceLock::new();
static ADVERTISED_PEER: OnceLock<RwLock<Option<config::PublicAdvertisement>>> = OnceLock::new();
static STATE_MUTATION_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static PEER_STORE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
static GOSSIP_NOTIFIER: OnceLock<(Mutex<u64>, Condvar)> = OnceLock::new();

struct RunConfig {
    database: PathBuf,
    p2p_listen: String,
    rpc_listen: String,
    peers: Vec<String>,
    miner: Option<Address>,
    public_addr: Option<PeerAddress>,
    nat_traversal: bool,
    #[cfg(feature = "litep2p-devnet")]
    litep2p: bool,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
struct Handshake {
    magic: [u8; 8],
    protocol_version: u32,
    node_id: [u8; 32],
    genesis_hash: [u8; 32],
    chain_spec_hash: [u8; 32],
    capabilities: u64,
    tip_height: Height,
    tip_hash: [u8; 32],
    cumulative_work: [u64; 8],
    cumulative_weight: u64,
}

struct ConnectedPeer {
    handshake: Handshake,
    stream: TcpStream,
}

struct HandshakeExchange {
    peer: Handshake,
    session_ledger: Arc<Ledger>,
}

#[derive(BorshSerialize, BorshDeserialize, Clone, Debug)]
struct GossipInventory {
    tip_height: Height,
    tip_hash: [u8; 32],
    cumulative_work: [u64; 8],
    cumulative_weight: u64,
    hash: Vec<[u8; 32]>,
}

enum PeerSessionOutcome {
    Complete,
    ReverseSync(GossipInventory),
}

#[derive(Debug)]
struct HttpRequest {
    headers: String,
    body: Vec<u8>,
}

mod chain_sync;
mod config;
mod explorer;
mod gossip;
mod index;
#[cfg(feature = "litep2p-devnet")]
mod litep2p_devnet;
mod mempool;
mod mining;
mod p2p;
mod protocol;
mod rpc;
mod state;
mod util;

pub fn run(args: Vec<String>) -> Result<(), String> {
    match args.first().map(String::as_str) {
        None => run_automatic(&[]),
        Some("run") => run_automatic(&args[1..]),
        Some("info") => config::print_network_info(),
        Some("check") => state::check_database(args.get(1).map(String::as_str)),
        Some("submit-block") => state::submit_block(
            args.get(1).map(String::as_str),
            args.get(2).ok_or("missing block hex")?,
        ),
        Some("mine-block") => mining::mine_one_block(
            args.get(1).map(String::as_str),
            args.get(2).ok_or("missing miner address")?,
        ),
        Some("submit-transaction") => mempool::submit_transaction(
            args.get(1).map(String::as_str),
            args.get(2).ok_or("missing transaction hex")?,
        ),
        Some("mempool") => mempool::print_mempool(args.get(1).map(String::as_str)),
        Some("account") => explorer::print_account(
            args.get(1).map(String::as_str),
            args.get(2).ok_or("missing account address")?,
        ),
        Some("rpc") => rpc::serve_rpc(
            args.get(1).map(String::as_str),
            args.get(2).map_or("127.0.0.1:6666", String::as_str),
        ),
        Some("p2p-listen") => p2p::serve_p2p(
            args.get(1).map(String::as_str),
            args.get(2).map_or("0.0.0.0:6677", String::as_str),
        ),
        Some("network") => p2p::run_network(
            args.get(1).map(String::as_str),
            args.get(2)
                .map_or(config::default_p2p_listen(), String::as_str),
            args.get(3..).unwrap_or(&[]),
        ),
        #[cfg(feature = "litep2p-devnet")]
        Some("litep2p") => litep2p_devnet::run(
            args.get(1).map(String::as_str),
            args.get(2)
                .map_or(config::default_p2p_listen(), String::as_str),
            args.get(3..).unwrap_or(&[]),
        ),
        Some("peer") => p2p::connect_peer(
            args.get(1).map(String::as_str),
            args.get(2).ok_or("missing peer address")?,
        ),
        Some("version") | Some("--version") | Some("-V") => {
            println!("node {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            config::print_help();
            Ok(())
        }
        Some(command) => Err(format!("unknown command `{command}`")),
    }
}

fn run_automatic(args: &[String]) -> Result<(), String> {
    let config = RunConfig::parse(args)?;
    state::load_or_initialize(&config.database)?;
    config::configure_public_address(&config)?;
    let sync_lock = Arc::new(Mutex::new(()));

    let rpc_database = config.database.clone();
    let rpc_listen = config.rpc_listen.clone();
    thread::spawn(move || {
        if let Err(error) = rpc::serve_rpc_database(rpc_database, &rpc_listen) {
            eprintln!("node: RPC stopped: {error}");
        }
    });

    #[cfg(feature = "litep2p-devnet")]
    if !config.litep2p {
        p2p::start_peer_supervisor(
            config.database.clone(),
            config.peers.clone(),
            Arc::clone(&sync_lock),
        );
    }
    #[cfg(not(feature = "litep2p-devnet"))]
    p2p::start_peer_supervisor(
        config.database.clone(),
        config.peers.clone(),
        Arc::clone(&sync_lock),
    );

    if let Some(miner) = config.miner {
        let database = config.database.clone();
        thread::spawn(move || mining::mining_loop(database, miner));
    }

    println!("database: {}", config.database.display());
    println!("rpc: http://{}", config.rpc_listen);
    println!("outbound_peers: {}", config.peers.len());
    if config.miner.is_some() {
        println!("mining: enabled on the local canonical tip");
    } else {
        println!("mining: disabled");
    }
    #[cfg(feature = "litep2p-devnet")]
    if config.litep2p {
        return litep2p_devnet::run_database(config.database, &config.p2p_listen, &config.peers);
    }
    p2p::serve_p2p_database(config.database, &config.p2p_listen)
}

#[cfg(test)]
mod tests;
