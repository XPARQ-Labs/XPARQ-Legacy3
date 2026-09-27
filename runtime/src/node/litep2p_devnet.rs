//! Opt-in devnet transport. Its protocol is separate from the legacy TCP wire format.

use futures::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    time::Duration,
};

use litep2p::{
    Litep2p, Litep2pEvent, PeerId,
    config::ConfigBuilder,
    crypto::ed25519::Keypair,
    protocol::{
        notification::{
            ConfigBuilder as NotificationConfigBuilder, NotificationEvent, ValidationResult,
        },
        request_response::{
            ConfigBuilder as RequestConfigBuilder, DialOptions, RequestResponseEvent,
        },
    },
    transport::tcp::config::Config as TcpConfig,
    types::{RequestId, protocol::ProtocolName},
};

use super::{
    EXPECTED_GENESIS_HASH, Height, MAX_STORED_BLOCK_SIZE, MAX_STORED_TRANSACTION_SIZE, block_bytes,
    canonical_bytes, canonical_decode, chain_spec_hash, decode_block,
};
use super::{
    chain_sync::ledger_header_state_at_height,
    config::database_path,
    gossip::accept_relayed_block,
    mempool::accept_relayed_transaction,
    state::{load_or_initialize, load_or_initialize_header_snapshot},
};
use kernel::consensus::{HeaderAtHeight, verify_header_chain_extension};

const REQUEST_PROTOCOL: &str = "/xparq/devnet/blocks/1";
const NOTIFY_PROTOCOL: &str = "/xparq/devnet/announce/1";
const REQUEST_TIP: u8 = 1;
const REQUEST_BLOCK: u8 = 2;
const REQUEST_HEADER: u8 = 3;
const ANNOUNCE_BLOCK: u8 = 1;
const ANNOUNCE_TRANSACTION: u8 = 2;
const IDENTITY_KEY: &str = "litep2p-devnet-identity";

fn multiaddr(address: SocketAddr) -> Result<litep2p::types::multiaddr::Multiaddr, String> {
    let family = match address.ip() {
        IpAddr::V4(_) => "ip4",
        IpAddr::V6(_) => "ip6",
    };
    format!("/{family}/{}/tcp/{}", address.ip(), address.port())
        .parse()
        .map_err(|error| format!("invalid P2P address: {error}"))
}

fn chain_identity() -> Result<Vec<u8>, String> {
    let mut identity = Vec::with_capacity(64);
    identity.extend_from_slice(&EXPECTED_GENESIS_HASH.0);
    identity.extend_from_slice(&chain_spec_hash().map_err(|error| error.to_string())?.0);
    Ok(identity)
}

fn load_identity(database: &Path) -> Result<Keypair, String> {
    let mut bytes = match crate::storage::auxiliary_get(database, IDENTITY_KEY)? {
        Some(bytes) => bytes,
        None => {
            let generated = Keypair::generate();
            crate::storage::auxiliary_get_or_insert(database, IDENTITY_KEY, &generated.to_bytes())?
        }
    };
    Keypair::try_from_bytes(&mut bytes)
        .map_err(|error| format!("invalid stored litep2p identity: {error}"))
}

fn local_tip(database: &Path) -> Result<u64, String> {
    Ok(load_or_initialize(database)?
        .tip_height()
        .map_or(0, |height| height.0))
}

fn answer(database: &Path, request: &[u8]) -> Result<Vec<u8>, String> {
    let ledger = load_or_initialize(database)?;
    match request {
        [REQUEST_TIP] => {
            let mut response = vec![REQUEST_TIP];
            response.extend_from_slice(
                &ledger
                    .tip_height()
                    .map_or(0, |height| height.0)
                    .to_le_bytes(),
            );
            Ok(response)
        }
        [REQUEST_BLOCK, height @ ..] if height.len() == 8 => {
            let height = u64::from_le_bytes(height.try_into().map_err(|_| "invalid block height")?);
            let Some(block) = ledger.chain.block(&Height(height)) else {
                return Ok(vec![0]);
            };
            let mut response = vec![REQUEST_BLOCK];
            response.extend_from_slice(&block_bytes(block).map_err(|error| error.to_string())?);
            Ok(response)
        }
        [REQUEST_HEADER, height @ ..] if height.len() == 8 => {
            let height = Height(u64::from_le_bytes(
                height.try_into().map_err(|_| "invalid header height")?,
            ));
            let Some(header) = ledger.chain.header(&height) else {
                return Ok(vec![0]);
            };
            let mut response = vec![REQUEST_HEADER];
            response.extend_from_slice(
                &canonical_bytes(&HeaderAtHeight::new(height, header.clone()))
                    .map_err(|error| error.to_string())?,
            );
            Ok(response)
        }
        _ => Err("unknown litep2p request".into()),
    }
}

fn request_block(height: u64) -> Vec<u8> {
    let mut request = vec![REQUEST_BLOCK];
    request.extend_from_slice(&height.to_le_bytes());
    request
}

fn request_header(height: u64) -> Vec<u8> {
    let mut request = vec![REQUEST_HEADER];
    request.extend_from_slice(&height.to_le_bytes());
    request
}

fn verify_next_header(database: &Path, bytes: &[u8]) -> Result<[u8; 32], String> {
    let header: HeaderAtHeight = canonical_decode(bytes).map_err(|error| error.to_string())?;
    let (ledger, checkpoints, _, _) = load_or_initialize_header_snapshot(database)?;
    let tip = ledger.tip_height().ok_or("local chain has no tip")?;
    if header.height.0 != tip.0.saturating_add(1) {
        return Err("header does not extend the local tip".into());
    }
    let state = ledger_header_state_at_height(&ledger, &checkpoints, tip)?;
    let (hash, _) =
        verify_header_chain_extension(&state, &[header]).map_err(|error| error.to_string())?;
    Ok(hash.0)
}

pub(super) fn run(path: Option<&str>, listen: &str, peers: &[String]) -> Result<(), String> {
    run_database(database_path(path), listen, peers)
}

pub(super) fn run_database(
    database: PathBuf,
    listen: &str,
    peers: &[String],
) -> Result<(), String> {
    load_or_initialize(&database)?;
    let listen: SocketAddr = listen
        .parse()
        .map_err(|_| "litep2p listen address must be numeric")?;
    let listen = multiaddr(listen)?;
    let peers = peers
        .iter()
        .map(|peer| {
            let (socket, peer_id) = peer
                .split_once('@')
                .ok_or_else(|| format!("peer `{peer}` must use ADDRESS@PEER_ID"))?;
            let peer_id: PeerId = peer_id
                .parse()
                .map_err(|error| format!("invalid peer ID: {error}"))?;
            let address: SocketAddr = socket
                .parse()
                .map_err(|_| format!("invalid peer address `{peer}`"))?;
            format!("{}/p2p/{peer_id}", multiaddr(address)?)
                .parse()
                .map(|address| (peer_id, address))
                .map_err(|error| format!("invalid peer address: {error}"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| format!("start litep2p runtime: {error}"))?;
    runtime.block_on(run_async(database, listen, peers))
}

async fn run_async(
    database: PathBuf,
    listen: litep2p::types::multiaddr::Multiaddr,
    peers: Vec<(PeerId, litep2p::types::multiaddr::Multiaddr)>,
) -> Result<(), String> {
    let identity = chain_identity()?;
    let keypair = load_identity(&database)?;
    let (request_config, mut requests) =
        RequestConfigBuilder::new(ProtocolName::from(REQUEST_PROTOCOL))
            .with_max_size(MAX_STORED_BLOCK_SIZE + 1)
            .build();
    let (notification_config, mut notifications) =
        NotificationConfigBuilder::new(ProtocolName::from(NOTIFY_PROTOCOL))
            .with_max_size(MAX_STORED_BLOCK_SIZE + 1)
            .with_handshake(identity.clone())
            .build();
    let config = ConfigBuilder::new()
        .with_keypair(keypair)
        .with_tcp(TcpConfig {
            listen_addresses: vec![listen],
            ..Default::default()
        })
        .with_request_response_protocol(request_config)
        .with_notification_protocol(notification_config)
        .build();
    let mut network = Litep2p::new(config).map_err(|error| error.to_string())?;
    if network.listen_addresses().next().is_none() {
        return Err("litep2p did not bind the requested listen address".into());
    }
    println!(
        "litep2p: peer={} listen={:?}",
        network.local_peer_id(),
        network.listen_addresses().collect::<Vec<_>>()
    );
    for (_, peer) in &peers {
        network
            .dial_address(peer.clone())
            .await
            .map_err(|error| error.to_string())?;
    }

    let mut accepted = HashSet::<PeerId>::new();
    let mut pending = HashMap::<RequestId, (PeerId, u8)>::new();
    let mut remote_heights = HashMap::<PeerId, u64>::new();
    let mut verified_headers = HashMap::<PeerId, [u8; 32]>::new();
    let mut announced_transactions = HashSet::<[u8; 32]>::new();
    let mut last_announced = local_tip(&database)?;
    let mut tick = tokio::time::interval(Duration::from_secs(3));
    let mut redial = tokio::time::interval(Duration::from_secs(10));
    redial.tick().await;
    loop {
        tokio::select! {
            event = network.next_event() => match event {
                Some(Litep2pEvent::ConnectionEstablished { peer, .. }) => {
                    println!("litep2p: connected peer={peer}");
                    if let Err(error) = notifications.open_substream(peer).await {
                        eprintln!("litep2p: open announcement stream: {error}");
                    }
                }
                Some(Litep2pEvent::ConnectionClosed { peer, .. }) => {
                    accepted.remove(&peer);
                    remote_heights.remove(&peer);
                    verified_headers.remove(&peer);
                }
                Some(Litep2pEvent::DialFailure { address, error }) => eprintln!("litep2p: dial {address}: {error}"),
                Some(Litep2pEvent::ListDialFailures { errors }) => eprintln!("litep2p: dial failures: {errors:?}"),
                None => return Err("litep2p event stream closed".into()),
            },
            event = notifications.next() => match event {
                Some(NotificationEvent::ValidateSubstream { peer, handshake, .. }) => {
                    notifications.send_validation_result(peer, if handshake == identity { ValidationResult::Accept } else { ValidationResult::Reject });
                }
                Some(NotificationEvent::NotificationStreamOpened { peer, handshake, .. }) => {
                    if handshake == identity {
                        accepted.insert(peer);
                        println!("litep2p: chain accepted peer={peer}");
                        for transaction in crate::storage::read_mempool(&database).unwrap_or_default().into_iter().take(256) {
                            if transaction.len() <= MAX_STORED_TRANSACTION_SIZE {
                                let mut message = vec![ANNOUNCE_TRANSACTION];
                                message.extend_from_slice(&transaction);
                                let _ = notifications.send_sync_notification(peer, message);
                            }
                        }
                        let id = requests.try_send_request(peer, vec![REQUEST_TIP], DialOptions::Reject)
                            .map_err(|error| error.to_string())?;
                        pending.insert(id, (peer, REQUEST_TIP));
                    } else {
                        eprintln!("litep2p: rejected peer {peer}: chain identity mismatch");
                        accepted.remove(&peer);
                    }
                }
                Some(NotificationEvent::NotificationStreamClosed { peer }) => { accepted.remove(&peer); }
                Some(NotificationEvent::NotificationReceived { peer, notification }) if accepted.contains(&peer) => {
                    match notification.first().copied() {
                        Some(ANNOUNCE_BLOCK) if notification.len() <= MAX_STORED_BLOCK_SIZE + 1 => {
                            if let Err(error) = accept_relayed_block(&database, &notification[1..]) {
                                eprintln!("litep2p: block from {peer} rejected: {error}");
                            }
                        }
                        Some(ANNOUNCE_TRANSACTION) if notification.len() <= MAX_STORED_TRANSACTION_SIZE + 1 => {
                            if let Err(error) = accept_relayed_transaction(&database, &notification[1..]) {
                                eprintln!("litep2p: transaction from {peer} rejected: {error}");
                            }
                        }
                        _ => eprintln!("litep2p: invalid announcement from {peer}"),
                    }
                }
                Some(_) => {},
                None => return Err("litep2p notification stream closed".into()),
            },
            event = requests.next() => match event {
                Some(RequestResponseEvent::RequestReceived { peer, request_id, request, .. }) => {
                    if accepted.contains(&peer) {
                        let response = answer(&database, &request).unwrap_or_else(|_| vec![0]);
                        requests.send_response(request_id, response);
                    } else { requests.reject_request(request_id); }
                }
                Some(RequestResponseEvent::ResponseReceived { peer, request_id, response, .. }) => {
                    if let Some((expected_peer, kind)) = pending.remove(&request_id) {
                        if expected_peer != peer || !accepted.contains(&peer) { continue; }
                        let mut advance = false;
                        if kind == REQUEST_TIP && response.len() == 9 && response[0] == REQUEST_TIP {
                            let height = u64::from_le_bytes(response[1..9].try_into().map_err(|_| "invalid tip response")?);
                            if remote_heights.insert(peer, height).is_none() {
                                println!("litep2p: peer={peer} tip_height={height}");
                            }
                            advance = true;
                        } else if kind == REQUEST_HEADER && response.first() == Some(&REQUEST_HEADER) {
                            match verify_next_header(&database, &response[1..]) {
                                Ok(hash) => { verified_headers.insert(peer, hash); advance = true; }
                                Err(error) => {
                                    eprintln!("litep2p: header from {peer} rejected: {error}");
                                    remote_heights.remove(&peer);
                                }
                            }
                        } else if kind == REQUEST_BLOCK && response.first() == Some(&REQUEST_BLOCK) {
                            let expected = verified_headers.remove(&peer);
                            let actual = decode_block(&response[1..]).ok()
                                .and_then(|block| block.hash().ok()).map(|hash| hash.0);
                            if expected.is_none() || actual != expected {
                                eprintln!("litep2p: block from {peer} differs from verified header");
                                remote_heights.remove(&peer);
                                continue;
                            }
                            if let Err(error) = accept_relayed_block(&database, &response[1..]) {
                                eprintln!("litep2p: synced block from {peer} rejected: {error}");
                                remote_heights.remove(&peer);
                            } else {
                                println!("litep2p: accepted synced block from {peer}");
                                advance = true;
                            }
                        } else {
                            remote_heights.remove(&peer);
                        }
                        let height = local_tip(&database)?;
                        if advance && remote_heights.get(&peer).is_some_and(|remote| *remote > height) {
                            let (next_kind, payload) = if verified_headers.contains_key(&peer) {
                                (REQUEST_BLOCK, request_block(height + 1))
                            } else {
                                (REQUEST_HEADER, request_header(height + 1))
                            };
                            if let Ok(id) = requests.try_send_request(peer, payload, DialOptions::Reject) {
                                pending.insert(id, (peer, next_kind));
                            }
                        }
                    }
                }
                Some(RequestResponseEvent::RequestFailed { request_id, error, .. }) => {
                    pending.remove(&request_id);
                    eprintln!("litep2p: request failed: {error:?}");
                }
                None => return Err("litep2p request stream closed".into()),
            },
            _ = tick.tick() => {
                let height = local_tip(&database)?;
                for &peer in &accepted {
                    if !pending.values().any(|(pending_peer, _)| *pending_peer == peer) {
                        let (kind, payload) = if remote_heights.get(&peer).is_some_and(|remote| *remote > height) {
                            if verified_headers.contains_key(&peer) {
                                (REQUEST_BLOCK, request_block(height + 1))
                            } else {
                                (REQUEST_HEADER, request_header(height + 1))
                            }
                        } else { (REQUEST_TIP, vec![REQUEST_TIP]) };
                        if let Ok(id) = requests.try_send_request(peer, payload, DialOptions::Reject) {
                            pending.insert(id, (peer, kind));
                        }
                    }
                }
                if height > last_announced {
                    let ledger = load_or_initialize(&database)?;
                    if let Some(block) = ledger.chain.block(&Height(height)) {
                        let mut message = vec![ANNOUNCE_BLOCK];
                        message.extend_from_slice(&block_bytes(block).map_err(|error| error.to_string())?);
                        for &peer in &accepted { let _ = notifications.send_sync_notification(peer, message.clone()); }
                    }
                    last_announced = height;
                }
                for transaction in crate::storage::read_mempool(&database)?.into_iter().take(256) {
                    if transaction.len() > MAX_STORED_TRANSACTION_SIZE { continue; }
                    let id = kernel::crypto::hash_bytes(&transaction).0;
                    if !announced_transactions.insert(id) { continue; }
                    let mut message = vec![ANNOUNCE_TRANSACTION];
                    message.extend_from_slice(&transaction);
                    for &peer in &accepted { let _ = notifications.send_sync_notification(peer, message.clone()); }
                }
                if announced_transactions.len() > 1024 { announced_transactions.clear(); }
            },
            _ = redial.tick() => {
                for (peer, address) in &peers {
                    if !accepted.contains(peer) {
                        let _ = network.dial_address(address.clone()).await;
                    }
                }
            },
        }
    }
}
