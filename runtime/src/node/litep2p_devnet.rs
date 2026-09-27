//! Opt-in devnet transport. Its protocol is separate from the legacy TCP wire format.

use futures::StreamExt;
use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
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
    Block, BlockHash, EXPECTED_GENESIS_HASH, Handshake, HeaderSyncResult, Height,
    MAX_STORED_BLOCK_SIZE, MAX_STORED_TRANSACTION_SIZE, MAX_SYNC_HEADERS, Work, block_bytes,
    canonical_bytes, canonical_decode, chain_spec_hash, compare_chain_tips, decode_block,
    new_pow_memory,
};
use super::{
    chain_sync::{
        apply_verified_branch, decode_locator, encode_locator, ledger_header_locator,
        ledger_header_state_at_height, map_peer_header_error,
    },
    config::database_path,
    gossip::accept_relayed_block,
    mempool::accept_relayed_transaction,
    protocol::validate_handshake,
    state::{
        cached_canonical_block_bytes, cached_handshake, load_or_initialize,
        load_or_initialize_header_snapshot,
    },
};
use crate::sync::{HeaderChainChunk, MAX_HEADER_CHAIN_CHUNK_HEADERS, decode_header_chain_chunk};
use kernel::{
    consensus::{
        HeaderAtHeight, HeaderValidationState, advance_header_validation_state_with_memory,
    },
    crypto::PoWMemory,
};

const REQUEST_PROTOCOL: &str = "/xparq/devnet/blocks/2";
const NOTIFY_PROTOCOL: &str = "/xparq/devnet/announce/2";
const REQUEST_TIP: u8 = 1;
const REQUEST_BLOCK: u8 = 2;
const REQUEST_HEADERS: u8 = 4;
const RESPONSE_HEADERS: u8 = 4;
const RESPONSE_COMPLETE: u8 = 5;
const ANNOUNCE_BLOCK: u8 = 1;
const ANNOUNCE_TRANSACTION: u8 = 2;
const IDENTITY_KEY: &str = "litep2p-devnet-identity";
const MAX_ACTIVE_HEADER_SYNCS: usize = 1;

#[derive(Clone, Copy)]
enum PendingKind {
    Tip,
    Headers,
    Block(usize),
}

struct PeerSync {
    claim: Handshake,
    ledger: Arc<super::Ledger>,
    checkpoints: Arc<Vec<super::state::HeaderStateCheckpoint>>,
    local_work: Work,
    local_weight: u64,
    local_locator: Vec<([u8; 32], Height)>,
    validation: Option<HeaderValidationState>,
    ancestor: Option<(Height, BlockHash)>,
    headers: Vec<HeaderAtHeight>,
    blocks: Vec<Block>,
    pow_memory: Option<PoWMemory>,
}

impl PeerSync {
    fn new(database: &Path, claim: Handshake) -> Result<Self, String> {
        let (ledger, checkpoints, local_work, local_weight) =
            load_or_initialize_header_snapshot(database)?;
        let local_locator = ledger_header_locator(&ledger)?;
        Ok(Self {
            claim,
            ledger,
            checkpoints,
            local_work,
            local_weight,
            local_locator,
            validation: None,
            ancestor: None,
            headers: Vec::new(),
            blocks: Vec::new(),
            pow_memory: None,
        })
    }

    fn locator_request(&self) -> Result<Vec<u8>, String> {
        let mut request = vec![REQUEST_HEADERS];
        let locator = if let Some(validation) = &self.validation {
            vec![
                validation
                    .header
                    .hash()
                    .map_err(|error| error.to_string())?
                    .0,
                EXPECTED_GENESIS_HASH.0,
            ]
        } else {
            self.local_locator.iter().map(|(hash, _)| *hash).collect()
        };
        request.extend_from_slice(&encode_locator(&locator)?);
        Ok(request)
    }

    fn ancestor_state(&self, hash: [u8; 32]) -> Result<HeaderValidationState, String> {
        let height = self
            .local_locator
            .iter()
            .find_map(|(candidate, height)| (*candidate == hash).then_some(*height))
            .ok_or("peer ancestor is not in the local locator")?;
        ledger_header_state_at_height(&self.ledger, &self.checkpoints, height)
    }

    fn accept_headers(&mut self, response: &[u8]) -> Result<bool, String> {
        let (&kind, body) = response.split_first().ok_or("empty header response")?;
        let ancestor: [u8; 32] = body
            .get(..32)
            .ok_or("header response has no ancestor")?
            .try_into()
            .map_err(|_| "invalid ancestor hash")?;
        if kind == RESPONSE_COMPLETE {
            if body.len() != 32 {
                return Err("invalid header completion length".into());
            }
            let state = match &self.validation {
                Some(state) => state.clone(),
                None => self.ancestor_state(ancestor)?,
            };
            let tip_hash = state.header.hash().map_err(|error| error.to_string())?;
            if tip_hash.0 != ancestor
                || tip_hash.0 != self.claim.tip_hash
                || state.height != self.claim.tip_height
                || state.cumulative_work.to_be_limbs() != self.claim.cumulative_work
                || state.cumulative_weight != self.claim.cumulative_weight
            {
                return Err("peer claim does not match verified header tip".into());
            }
            if self.ancestor.is_none() {
                self.ancestor = Some((state.height, tip_hash));
            }
            self.validation = Some(state);
            return Ok(true);
        }
        if kind != RESPONSE_HEADERS {
            return Err("unexpected header response".into());
        }
        let chunk = decode_header_chain_chunk(&body[32..]).map_err(|error| error.to_string())?;
        if self.headers.len().saturating_add(chunk.headers.len()) > MAX_SYNC_HEADERS {
            return Err("header synchronization exceeds session limit".into());
        }
        let current = match &self.validation {
            Some(current) => {
                if current.header.hash().map_err(|error| error.to_string())?.0 != ancestor {
                    return Err("peer changed header ancestor during sync".into());
                }
                current.clone()
            }
            None => self.ancestor_state(ancestor)?,
        };
        if self.ancestor.is_none() {
            self.ancestor = Some((current.height, BlockHash(ancestor)));
        }
        let advanced = advance_header_validation_state_with_memory(
            &current,
            &chunk.headers,
            self.pow_memory.get_or_insert_with(new_pow_memory),
        )
        .map_err(map_peer_header_error)?;
        self.headers.extend(chunk.headers);
        self.validation = Some(advanced);
        Ok(false)
    }

    fn preferred(&self) -> Result<bool, String> {
        let state = self
            .validation
            .as_ref()
            .ok_or("missing verified header state")?;
        let local_hash = self.ledger.tip_hash().ok_or("local chain has no tip")?;
        Ok(compare_chain_tips(
            state.cumulative_work,
            state.cumulative_weight,
            state.header.hash().map_err(|error| error.to_string())?,
            self.local_work,
            self.local_weight,
            local_hash,
        )
        .is_gt())
    }

    fn into_result(self) -> Result<(HeaderSyncResult, Vec<Block>), String> {
        let state = self.validation.ok_or("missing verified header state")?;
        let (ancestor_height, ancestor_hash) = self.ancestor.ok_or("missing common ancestor")?;
        Ok((
            HeaderSyncResult {
                ancestor_height,
                ancestor_hash,
                headers: self.headers,
                peer_work: state.cumulative_work,
                peer_weight: state.cumulative_weight,
                preferred: true,
            },
            self.blocks,
        ))
    }
}

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
    match request {
        [REQUEST_TIP] => {
            let mut response = vec![REQUEST_TIP];
            response.extend_from_slice(
                &canonical_bytes(&cached_handshake(database)?)
                    .map_err(|error| error.to_string())?,
            );
            Ok(response)
        }
        [REQUEST_BLOCK, hash @ ..] if hash.len() == 32 => {
            let hash: [u8; 32] = hash.try_into().map_err(|_| "invalid block hash")?;
            let Some(block) = cached_canonical_block_bytes(database, hash)? else {
                return Ok(vec![0]);
            };
            let mut response = vec![REQUEST_BLOCK];
            response.extend_from_slice(&block);
            Ok(response)
        }
        [REQUEST_HEADERS, locator @ ..] => answer_headers(database, locator),
        _ => Err("unknown litep2p request".into()),
    }
}

fn answer_headers(database: &Path, locator: &[u8]) -> Result<Vec<u8>, String> {
    let locator = decode_locator(locator)?;
    let ledger = load_or_initialize(database)?;
    let mut height = ledger.tip_height().ok_or("canonical chain has no tip")?;
    let (ancestor_height, ancestor_hash) = loop {
        let block = ledger
            .chain
            .block(&height)
            .ok_or("canonical block is missing")?;
        let hash = block.header.hash().map_err(|error| error.to_string())?.0;
        if locator.contains(&hash) {
            break (height, hash);
        }
        height = Height(height.0.checked_sub(1).ok_or("no common ancestor")?);
    };
    let mut extension = Vec::with_capacity(MAX_HEADER_CHAIN_CHUNK_HEADERS);
    let mut next = ancestor_height.0.checked_add(1);
    while let Some(value) = next {
        if value > ledger.tip_height().ok_or("canonical chain has no tip")?.0
            || extension.len() >= MAX_HEADER_CHAIN_CHUNK_HEADERS
        {
            break;
        }
        let height = Height(value);
        let block = ledger
            .chain
            .block(&height)
            .ok_or("canonical block is missing")?;
        extension.push(HeaderAtHeight::new(height, block.header.clone()));
        next = value.checked_add(1);
    }
    let mut response = vec![if extension.is_empty() {
        RESPONSE_COMPLETE
    } else {
        RESPONSE_HEADERS
    }];
    response.extend_from_slice(&ancestor_hash);
    if !extension.is_empty() {
        response.extend_from_slice(
            &canonical_bytes(&HeaderChainChunk::new(extension).map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?,
        );
    }
    Ok(response)
}

fn request_block(hash: [u8; 32]) -> Vec<u8> {
    let mut request = vec![REQUEST_BLOCK];
    request.extend_from_slice(&hash);
    request
}

fn process_response(
    database: &Path,
    peer: PeerId,
    kind: PendingKind,
    response: &[u8],
    sessions: &mut HashMap<PeerId, PeerSync>,
) -> Result<Option<(PendingKind, Vec<u8>)>, String> {
    match kind {
        PendingKind::Tip => {
            if response.first() != Some(&REQUEST_TIP) {
                return Err("unexpected tip response".into());
            }
            if sessions.len() >= MAX_ACTIVE_HEADER_SYNCS && !sessions.contains_key(&peer) {
                return Ok(None);
            }
            let claim: Handshake = canonical_decode(&response[1..])
                .map_err(|error| format!("decode peer tip: {error}"))?;
            validate_handshake(&claim)?;
            let session = PeerSync::new(database, claim)?;
            let local_hash = session.ledger.tip_hash().ok_or("local chain has no tip")?;
            let claimed_work = Work::from_be_limbs(session.claim.cumulative_work);
            if !compare_chain_tips(
                claimed_work,
                session.claim.cumulative_weight,
                BlockHash(session.claim.tip_hash),
                session.local_work,
                session.local_weight,
                local_hash,
            )
            .is_gt()
            {
                return Ok(None);
            }
            println!(
                "litep2p: peer={peer} claimed_height={}",
                session.claim.tip_height.0
            );
            let request = session.locator_request()?;
            sessions.insert(peer, session);
            Ok(Some((PendingKind::Headers, request)))
        }
        PendingKind::Headers => {
            let session = sessions
                .get_mut(&peer)
                .ok_or("missing header sync session")?;
            if !session.accept_headers(response)? {
                return Ok(Some((PendingKind::Headers, session.locator_request()?)));
            }
            if !session.preferred()? {
                sessions.remove(&peer);
                return Ok(None);
            }
            let Some(first) = session.headers.first() else {
                sessions.remove(&peer);
                return Ok(None);
            };
            let hash = first.hash().map_err(|error| error.to_string())?.0;
            Ok(Some((PendingKind::Block(0), request_block(hash))))
        }
        PendingKind::Block(index) => {
            let session = sessions
                .get_mut(&peer)
                .ok_or("missing block sync session")?;
            let expected = session.headers.get(index).ok_or("unexpected block index")?;
            if response.first() != Some(&REQUEST_BLOCK) {
                return Err("unexpected block response".into());
            }
            let block = decode_block(&response[1..])
                .map_err(|error| format!("decode peer block: {error}"))?;
            if block.height() != expected.height
                || block.header != expected.header
                || block.hash().map_err(|error| error.to_string())?
                    != expected.hash().map_err(|error| error.to_string())?
            {
                return Err("peer block does not match verified header".into());
            }
            session.blocks.push(block);
            let next = index + 1;
            if let Some(expected) = session.headers.get(next) {
                let hash = expected.hash().map_err(|error| error.to_string())?.0;
                return Ok(Some((PendingKind::Block(next), request_block(hash))));
            }
            let session = sessions
                .remove(&peer)
                .ok_or("missing completed sync session")?;
            let (sync, blocks) = session.into_result()?;
            let applied = apply_verified_branch(database, sync, blocks)?;
            println!("litep2p: peer={peer} applied_blocks={applied}");
            Ok(None)
        }
    }
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
    let mut pending = HashMap::<RequestId, (PeerId, PendingKind)>::new();
    let mut sessions = HashMap::<PeerId, PeerSync>::new();
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
                    sessions.remove(&peer);
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
                        pending.insert(id, (peer, PendingKind::Tip));
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
                        let next = match process_response(&database, peer, kind, &response, &mut sessions) {
                            Ok(next) => next,
                            Err(error) => {
                                eprintln!("litep2p: peer {peer} synchronization rejected: {error}");
                                sessions.remove(&peer);
                                None
                            }
                        };
                        if let Some((kind, payload)) = next {
                            if let Ok(id) = requests.try_send_request(peer, payload, DialOptions::Reject) {
                                pending.insert(id, (peer, kind));
                            } else {
                                sessions.remove(&peer);
                            }
                        }
                    }
                }
                Some(RequestResponseEvent::RequestFailed { peer, request_id, error, .. }) => {
                    pending.remove(&request_id);
                    sessions.remove(&peer);
                    eprintln!("litep2p: request failed: {error:?}");
                }
                None => return Err("litep2p request stream closed".into()),
            },
            _ = tick.tick() => {
                let height = local_tip(&database)?;
                for &peer in &accepted {
                    if !sessions.contains_key(&peer) && !pending.values().any(|(pending_peer, _)| *pending_peer == peer) {
                        if let Ok(id) = requests.try_send_request(peer, vec![REQUEST_TIP], DialOptions::Reject) {
                            pending.insert(id, (peer, PendingKind::Tip));
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
