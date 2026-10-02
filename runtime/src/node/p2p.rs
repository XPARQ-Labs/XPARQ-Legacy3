use std::{collections::BTreeMap, net::ToSocketAddrs, sync::mpsc, time::Instant};

use super::*;
use super::{chain_sync::*, config::*, gossip::*, mempool::*, protocol::*, state::*, util::*};

pub(super) fn serve_p2p(path: Option<&str>, listen: &str) -> Result<(), String> {
    let database = database_path(path);
    load_or_initialize(&database)?;
    serve_p2p_database(database, listen)
}

pub(super) fn serve_p2p_database(database: PathBuf, listen: &str) -> Result<(), String> {
    let listener = TcpListener::bind(listen).map_err(|error| format!("bind P2P: {error}"))?;
    println!("p2p: {listen}");
    let active = Arc::new(AtomicUsize::new(0));
    let active_by_ip = Arc::new(Mutex::new(std::collections::BTreeMap::new()));
    for connection in listener.incoming() {
        match connection {
            Ok(stream) => {
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_INBOUND_CONNECTIONS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    eprintln!("node: inbound peer limit reached");
                    continue;
                }
                let peer_ip = stream.peer_addr().ok().map(|address| address.ip());
                if let Some(ip) = peer_ip {
                    let mut by_ip = active_by_ip
                        .lock()
                        .map_err(|_| "inbound peer counter lock is poisoned")?;
                    let count = by_ip.entry(ip).or_insert(0_usize);
                    if *count >= MAX_INBOUND_CONNECTIONS_PER_IP {
                        active.fetch_sub(1, Ordering::AcqRel);
                        eprintln!("node: inbound per-IP limit reached address={ip}");
                        continue;
                    }
                    *count += 1;
                }
                let database = database.clone();
                let active = Arc::clone(&active);
                let active_by_ip = Arc::clone(&active_by_ip);
                thread::spawn(move || {
                    handle_inbound_peer(&database, stream);
                    active.fetch_sub(1, Ordering::AcqRel);
                    if let Some(ip) = peer_ip
                        && let Ok(mut by_ip) = active_by_ip.lock()
                        && let Some(count) = by_ip.get_mut(&ip)
                    {
                        *count = count.saturating_sub(1);
                        if *count == 0 {
                            by_ip.remove(&ip);
                        }
                    }
                });
            }
            Err(error) => eprintln!("node: accept P2P connection: {error}"),
        }
    }
    Ok(())
}

pub(super) fn handle_inbound_peer(database: &Path, mut stream: TcpStream) {
    let address = stream
        .peer_addr()
        .map_or_else(|_| "unknown".into(), |value| value.to_string());
    match exchange_handshake(database, &mut stream).and_then(|exchange| {
        let outcome = serve_peer_requests(database, &mut stream, &exchange.session_ledger)?;
        if let PeerSessionOutcome::ReverseSync(inventory) = outcome {
            let peer = handshake_from_inventory(&exchange.peer, &inventory);
            let sync = synchronize_headers(database, &mut stream, &peer)?;
            if sync.preferred {
                synchronize_blocks(database, &mut stream, sync)?;
            }
            write_frame(&mut stream, &[SYNC_COMPLETE_MESSAGE])?;
        }
        Ok(exchange.peer)
    }) {
        Ok(peer) => println!(
            "peer served address={address} height={} tip={} work={}",
            peer.tip_height.0,
            hex::encode(peer.tip_hash),
            format_work(peer.cumulative_work),
        ),
        Err(error) => eprintln!("node: peer rejected address={address} reason={error}"),
    }
}

pub(super) fn run_network(
    path: Option<&str>,
    listen: &str,
    peers: &[String],
) -> Result<(), String> {
    let database = database_path(path);
    load_or_initialize(&database)?;
    let sync_lock = Arc::new(Mutex::new(()));
    start_peer_supervisor(database.clone(), peers.to_vec(), sync_lock);
    println!("outbound_peers: {}", peers.len());
    serve_p2p_database(database, listen)
}

pub(super) fn start_peer_supervisor(
    database: PathBuf,
    configured: Vec<String>,
    sync_lock: Arc<Mutex<()>>,
) {
    let configured: Vec<_> = configured
        .into_iter()
        .map(|endpoint| {
            endpoint
                .parse::<PeerAddress>()
                .map(|address| address.to_string())
                .unwrap_or(endpoint)
        })
        .collect();
    thread::spawn(move || {
        let mut active = BTreeSet::new();
        let mut retry = BTreeMap::new();
        let (events, completed) = mpsc::channel();
        let mut dialing = 0_usize;
        loop {
            while let Ok((peer, outcome)) = completed.try_recv() {
                match outcome {
                    None => dialing -= 1,
                    Some((failures, delay)) => {
                        active.remove(&peer);
                        retry.insert(peer, (failures, Instant::now() + delay));
                    }
                }
            }
            let mut candidates = configured.clone();
            match PeerStore::load(&database) {
                Ok(store) => {
                    candidates.extend(store.addresses().into_iter().map(|peer| peer.to_string()))
                }
                Err(error) => eprintln!("node: load peer store: {error}"),
            }
            retry.retain(|peer, (_, until)| {
                active.contains(peer) || candidates.contains(peer) || *until > Instant::now()
            });
            let selected = select_outbound_candidates(&candidates, &active, &retry, dialing);
            for peer in selected {
                active.insert(peer.clone());
                dialing += 1;
                let database = database.clone();
                let sync_lock = Arc::clone(&sync_lock);
                let events = events.clone();
                let public_only = !configured.contains(&peer);
                let mut consecutive_failures: u32 =
                    retry.get(&peer).map_or(0, |(failures, _)| *failures);
                thread::spawn(move || {
                    let dialed = dial_peer_database_with_policy(&database, &peer, public_only);
                    let _ = events.send((peer.clone(), None));
                    let result = dialed.and_then(|connection| match sync_lock.lock() {
                        Ok(_guard) => synchronize_peer_database(&database, &peer, connection),
                        Err(_) => Err("outbound sync lock is poisoned".into()),
                    });
                    let reconnect_after = match result {
                        Ok(mut connection) => {
                            consecutive_failures = 0;
                            let gossip = gossip_outbound_session(
                                &database,
                                &mut connection.stream,
                                &connection.handshake,
                            );
                            match gossip {
                                Ok(()) => RECONNECT_INTERVAL,
                                Err(error) if error.starts_with(GOSSIP_RESYNC_PREFIX) => {
                                    eprintln!("node: peer={peer} requires header resync");
                                    Duration::from_secs(1)
                                }
                                Err(error) => {
                                    consecutive_failures = consecutive_failures.saturating_add(1);
                                    let malicious = gossip_error_is_malicious(&error);
                                    if let Ok(address) = peer.parse() {
                                        let _ = record_peer_failure(&database, address, malicious);
                                    }
                                    let cooldown = if malicious {
                                        INVALID_POW_COOLDOWN
                                    } else {
                                        reconnect_delay_for_error(
                                            &error,
                                            consecutive_failures,
                                            &peer,
                                        )
                                    };
                                    eprintln!(
                                        "node: peer={peer} gossip session failed: {error} reconnect_after_secs={}",
                                        cooldown.as_secs()
                                    );
                                    cooldown
                                }
                            }
                        }
                        Err(error) => {
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            let malicious = error.starts_with(INVALID_POW_ERROR_PREFIX);
                            if let Ok(address) = peer.parse() {
                                let _ = record_peer_failure(&database, address, malicious);
                            }
                            let cooldown =
                                reconnect_delay_for_error(&error, consecutive_failures, &peer);
                            eprintln!(
                                "node: outbound peer={peer} sync failed: {error} reconnect_after_secs={}",
                                cooldown.as_secs()
                            );
                            cooldown
                        }
                    };
                    let _ = events.send((peer, Some((consecutive_failures, reconnect_after))));
                });
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn select_outbound_candidates(
    candidates: &[String],
    active: &BTreeSet<String>,
    retry: &BTreeMap<String, (u32, Instant)>,
    dialing: usize,
) -> Vec<String> {
    let slots = MAX_OUTBOUND_CONNECTIONS
        .saturating_sub(active.len())
        .min(MAX_CONCURRENT_DIALS.saturating_sub(dialing));
    let now = Instant::now();
    let mut seen = BTreeSet::new();
    candidates
        .iter()
        .filter(|peer| {
            !active.contains(*peer)
                && !retry.get(*peer).is_some_and(|(_, until)| *until > now)
                && seen.insert((*peer).clone())
        })
        .take(slots)
        .cloned()
        .collect()
}

pub(super) fn connect_peer(path: Option<&str>, peer: &str) -> Result<(), String> {
    let database = database_path(path);
    let mut connection = connect_peer_database(&database, peer)?;
    write_frame(&mut connection.stream, &[SYNC_COMPLETE_MESSAGE])
}

pub(super) fn connect_peer_database(database: &Path, peer: &str) -> Result<ConnectedPeer, String> {
    let connection = dial_peer_database(database, peer)?;
    synchronize_peer_database(database, peer, connection)
}

#[cfg(test)]
fn connect_with_timeout(peer: &str, timeout: Duration) -> Result<TcpStream, String> {
    connect_with_policy(peer, timeout, false)
}

fn connect_with_policy(
    peer: &str,
    timeout: Duration,
    public_only: bool,
) -> Result<TcpStream, String> {
    // Resolve on every attempt so DDNS changes are picked up on reconnect.
    // The system resolver has its own timeout; this budget covers TCP dialing.
    let addresses = peer
        .to_socket_addrs()
        .map_err(|error| format!("resolve peer: {error}"))?;
    let deadline = Instant::now() + timeout;
    let mut last_error = "peer resolved to no admissible addresses".to_string();
    for address in addresses {
        if public_only && !is_admissible_discovered_peer(&address) {
            continue;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err("connect peer: dial timeout".into());
        }
        match TcpStream::connect_timeout(&address, remaining) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = error.to_string(),
        }
    }
    Err(format!("connect peer: {last_error}"))
}

fn dial_peer_database(database: &Path, peer: &str) -> Result<ConnectedPeer, String> {
    dial_peer_database_with_policy(database, peer, false)
}

fn dial_peer_database_with_policy(
    database: &Path,
    peer: &str,
    public_only: bool,
) -> Result<ConnectedPeer, String> {
    load_or_initialize(database)?;
    let mut stream = connect_with_policy(peer, DIAL_TIMEOUT, public_only)?;
    let handshake = exchange_handshake(database, &mut stream)?.peer;
    Ok(ConnectedPeer { handshake, stream })
}

fn synchronize_peer_database(
    database: &Path,
    peer: &str,
    connection: ConnectedPeer,
) -> Result<ConnectedPeer, String> {
    let ConnectedPeer {
        handshake,
        mut stream,
    } = connection;
    let connected_address = stream.peer_addr().ok();
    let sync = synchronize_headers(database, &mut stream, &handshake)?;
    let verified = sync.headers.len();
    let preferred = sync.preferred;
    let applied = if preferred {
        synchronize_blocks(database, &mut stream, sync)?
    } else {
        0
    };
    let discovered = if handshake.capabilities & CAPABILITY_PEER_DISCOVERY != 0 {
        request_discovered_peers(&mut stream)?
    } else {
        Vec::new()
    };
    if handshake.capabilities & CAPABILITY_RELAY != 0 {
        let local = cached_handshake(database)?;
        let same_tip = local.tip_hash == handshake.tip_hash;
        let extended_peer = relay_next_block(
            database,
            &mut stream,
            handshake.tip_height,
            handshake.tip_hash,
        )?;
        if same_tip || extended_peer {
            relay_mempool(database, &mut stream)?;
        }
    }
    if let Ok(endpoint) = peer.parse::<PeerAddress>() {
        // Preserve DNS identity rather than pinning its transient resolved IP.
        record_peer_success(database, endpoint)?;
    } else if let Some(address) = connected_address {
        record_peer_success(database, address.into())?;
    }
    for address in discovered {
        record_discovered_peer(database, address)?;
    }
    println!(
        "peer accepted address={peer} height={} tip={} work={} verified_headers={verified} preferred={preferred} applied_blocks={applied}",
        handshake.tip_height.0,
        hex::encode(handshake.tip_hash),
        format_work(handshake.cumulative_work),
    );
    Ok(ConnectedPeer { handshake, stream })
}

pub(super) fn serve_peer_requests(
    database: &Path,
    stream: &mut TcpStream,
    session_ledger: &Ledger,
) -> Result<PeerSessionOutcome, String> {
    let tip_height = session_ledger
        .tip_height()
        .ok_or("canonical chain has no tip")?;

    serve_peer_requests_through(database, stream, session_ledger, tip_height)
}

pub(super) fn serve_peer_requests_through(
    database: &Path,
    stream: &mut TcpStream,
    session_ledger: &Ledger,
    tip_height: Height,
) -> Result<PeerSessionOutcome, String> {
    serve_header_requests(stream, session_ledger, tip_height)?;
    serve_block_requests(database, stream)
}

pub(super) fn serve_header_requests(
    stream: &mut TcpStream,
    ledger: &Ledger,
    tip_height: Height,
) -> Result<(), String> {
    let mut requests = 0_usize;

    loop {
        requests += 1;

        if requests > MAX_HEADER_REQUESTS_PER_SESSION {
            return Err("peer exceeded the header request limit".into());
        }

        let request = read_frame(stream, 2 + MAX_LOCATOR_HASHES * 32)?;
        let locator = decode_locator(&request)?;
        let mut height = tip_height;
        let (ancestor_height, ancestor_hash) = loop {
            let block = ledger
                .chain
                .block(&height)
                .ok_or("canonical block is missing")?;

            let hash = block.header.hash().map_err(|error| error.to_string())?.0;

            if locator.contains(&hash) {
                break (height, hash);
            }

            let Some(previous) = height.0.checked_sub(1) else {
                return Err("peer locator has no canonical common ancestor".into());
            };

            height = Height(previous);
        };

        let mut extension = Vec::with_capacity(MAX_HEADER_CHAIN_CHUNK_HEADERS);

        let mut next_height = ancestor_height.0.checked_add(1);

        while let Some(value) = next_height {
            if value > tip_height.0 || extension.len() >= MAX_HEADER_CHAIN_CHUNK_HEADERS {
                break;
            }

            let height = Height(value);

            let block = ledger
                .chain
                .block(&height)
                .ok_or("canonical block is missing")?;

            extension.push(kernel::consensus::HeaderAtHeight::new(
                height,
                block.header.clone(),
            ));

            next_height = value.checked_add(1);
        }

        if extension.is_empty() {
            let mut response = Vec::with_capacity(33);
            response.push(HEADERS_COMPLETE_MESSAGE);
            response.extend_from_slice(&ancestor_hash);
            write_frame(stream, &response)?;
            return Ok(());
        }

        let chunk = HeaderChainChunk::new(extension).map_err(|error| error.to_string())?;

        let chunk = canonical_bytes(&chunk).map_err(|error| error.to_string())?;

        let mut response = Vec::with_capacity(33 + chunk.len());
        response.push(HEADERS_MESSAGE);
        response.extend_from_slice(&ancestor_hash);
        response.extend_from_slice(&chunk);

        write_frame(stream, &response)?;
    }
}

pub(super) fn serve_block_requests(
    database: &Path,
    stream: &mut TcpStream,
) -> Result<PeerSessionOutcome, String> {
    let mut block_requests = 0_usize;
    let mut discovery_requests = 0_usize;
    let mut relayed_transactions = 0_usize;
    let mut relayed_blocks = 0_usize;
    let mut transaction_requests = 0_usize;
    loop {
        let request = read_block_session_frame(stream)?;
        let (&message, body) = request.split_first().ok_or("empty block request")?;
        match message {
            SYNC_COMPLETE_MESSAGE if body.is_empty() => return Ok(PeerSessionOutcome::Complete),
            GET_PEERS_MESSAGE if body.is_empty() => {
                discovery_requests += 1;
                if discovery_requests > 1 {
                    return Err("peer exceeded the discovery request limit".into());
                }
                let mut peers = advertised_peer_addresses()?;
                for peer in PeerStore::load(database)?.relay_addresses() {
                    if !peers.contains(&peer) {
                        peers.push(peer);
                    }
                }
                let encoded = encode_discovery_addresses(peers)?;
                let mut response = Vec::with_capacity(1 + encoded.len());
                response.push(PEERS_MESSAGE);
                response.extend_from_slice(&encoded);
                write_frame(stream, &response)?;
                continue;
            }
            INVENTORY_MESSAGE => {
                let peer_inventory = decode_gossip_inventory(body)?;
                relayed_transactions = 0;
                relayed_blocks = 0;
                transaction_requests = 0;
                let inventory = gossip_inventory(database)?;
                let encoded = canonical_bytes(&inventory).map_err(|error| error.to_string())?;
                if encoded.len() > MAX_GOSSIP_INVENTORY_SIZE {
                    return Err("local gossip inventory exceeds size limit".into());
                }
                let mut response = Vec::with_capacity(1 + encoded.len());
                response.push(INVENTORY_MESSAGE);
                response.extend_from_slice(&encoded);
                write_frame(stream, &response)?;
                if inventory_preferred(&peer_inventory, &inventory) {
                    return Ok(PeerSessionOutcome::ReverseSync(peer_inventory));
                }
                continue;
            }
            GET_TRANSACTION_MESSAGE if body.len() == 32 => {
                transaction_requests += 1;
                if transaction_requests > MAX_RELAY_ITEMS_PER_SESSION {
                    return Err("peer exceeded the transaction request limit".into());
                }
                let requested: [u8; 32] =
                    body.try_into().map_err(|_| "invalid requested Tx Hash")?;
                let transaction = read_mempool(database)?
                    .into_iter()
                    .find(|transaction| transaction.id().ok() == Some(requested))
                    .ok_or("requested transaction is not in the mempool")?;
                let encoded = canonical_bytes(&transaction).map_err(|error| error.to_string())?;
                let mut response = Vec::with_capacity(1 + encoded.len());
                response.push(TRANSACTION_MESSAGE);
                response.extend_from_slice(&encoded);
                write_frame(stream, &response)?;
                continue;
            }
            SUBMIT_TRANSACTION_MESSAGE => {
                relayed_transactions += 1;
                if relayed_transactions > MAX_RELAY_ITEMS_PER_SESSION {
                    return Err("peer exceeded the transaction relay limit".into());
                }
                let result = accept_relayed_transaction(database, body);
                let rejection = result.as_ref().err().cloned();
                write_relay_result(stream, result)?;
                if let Some(error) = rejection {
                    return Err(format!("invalid relayed transaction: {error}"));
                }
                continue;
            }
            SUBMIT_BLOCK_MESSAGE => {
                relayed_blocks += 1;
                if relayed_blocks > MAX_RELAYED_BLOCKS_PER_SESSION {
                    return Err("peer exceeded the block relay limit".into());
                }
                let result = accept_relayed_block(database, body);
                let rejection = result.as_ref().err().cloned();
                write_relay_result(stream, result)?;
                if let Some(error) = rejection {
                    return if error.starts_with(GOSSIP_RESYNC_PREFIX) {
                        Err(error)
                    } else {
                        Err(format!("invalid relayed block: {error}"))
                    };
                }
                continue;
            }
            GET_BLOCK_MESSAGE if body.len() == 32 => {
                block_requests += 1;
                if block_requests > MAX_SYNC_HEADERS {
                    return Err("peer exceeded the block request limit".into());
                }
            }
            _ => return Err("invalid block-session message".into()),
        }
        let requested: [u8; 32] = body
            .try_into()
            .map_err(|_| "invalid requested block hash")?;
        let encoded = cached_canonical_block_bytes(database, requested)?
            .ok_or("requested block is not canonical")?;

        let mut response = Vec::with_capacity(1 + encoded.len());
        response.push(BLOCK_MESSAGE);
        response.extend_from_slice(&encoded);
        write_frame(stream, &response)?;
    }
}

fn encode_discovery_addresses(mut peers: Vec<String>) -> Result<Vec<u8>, String> {
    peers.truncate(MAX_DISCOVERED_PEERS);
    // Borsh Vec<String>: count plus each string's length prefix and bytes.
    let mut size = 4;
    let count = peers
        .iter()
        .take_while(|peer| {
            size += 4 + peer.len();
            size <= MAX_PEERS_RESPONSE_SIZE
        })
        .count();
    peers.truncate(count);
    canonical_bytes(&peers).map_err(|error| error.to_string())
}

pub(super) fn record_discovered_peer(database: &Path, address: PeerAddress) -> Result<(), String> {
    let _guard = peer_store_lock()?
        .lock()
        .map_err(|_| "peer store lock is poisoned")?;
    let mut store = PeerStore::load(database)?;
    if store.insert_discovered(address) {
        store.save(database)?;
    }
    Ok(())
}

pub(super) fn record_peer_success(database: &Path, address: PeerAddress) -> Result<(), String> {
    let _guard = peer_store_lock()?
        .lock()
        .map_err(|_| "peer store lock is poisoned")?;
    let mut store = PeerStore::load(database)?;
    store.record_success(address);
    store.save(database)
}

pub(super) fn record_peer_failure(
    database: &Path,
    address: PeerAddress,
    malicious: bool,
) -> Result<(), String> {
    let _guard = peer_store_lock()?
        .lock()
        .map_err(|_| "peer store lock is poisoned")?;
    let mut store = PeerStore::load(database)?;
    store.record_failure(address, malicious);
    store.save(database)
}

pub(super) fn peer_store_lock() -> Result<&'static Mutex<()>, String> {
    Ok(PEER_STORE_LOCK.get_or_init(|| Mutex::new(())))
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn outbound_slots_skip_duplicates_active_and_cooling_peers() {
        let candidates: Vec<String> = ["active", "cooling", "ready", "ready", "other", "extra"]
            .into_iter()
            .map(str::to_string)
            .collect();
        let mut active = BTreeSet::from(["active".to_string()]);
        let retry = BTreeMap::from([(
            "cooling".to_string(),
            (1, Instant::now() + Duration::from_secs(60)),
        )]);
        assert_eq!(
            select_outbound_candidates(&candidates, &active, &retry, 0),
            ["ready", "other"]
        );
        assert_eq!(
            select_outbound_candidates(&candidates, &active, &retry, 1),
            ["ready"]
        );
        assert!(
            select_outbound_candidates(&candidates, &active, &retry, MAX_CONCURRENT_DIALS)
                .is_empty()
        );
        for i in 1..MAX_OUTBOUND_CONNECTIONS {
            active.insert(format!("connected-{i}"));
        }
        assert!(select_outbound_candidates(&candidates, &active, &retry, 0).is_empty());
        active.remove("connected-1");
        assert_eq!(
            select_outbound_candidates(&candidates, &active, &retry, 0),
            ["ready"]
        );
    }

    #[test]
    fn discovered_dns_cannot_dial_private_resolved_addresses() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("localhost:{}", listener.local_addr().unwrap().port());
        assert!(connect_with_policy(&endpoint, Duration::from_secs(1), true).is_err());
        assert!(connect_with_policy(&endpoint, Duration::from_secs(1), false).is_ok());
    }

    #[test]
    fn long_dns_names_do_not_exceed_discovery_frame_budget() {
        let name = [
            "a".repeat(63),
            "b".repeat(63),
            "c".repeat(63),
            "d".repeat(61),
        ]
        .join(".");
        let endpoint = format!("{name}:6677");
        assert!(endpoint.parse::<PeerAddress>().unwrap().is_admissible());
        let encoded = encode_discovery_addresses(vec![endpoint; MAX_DISCOVERED_PEERS]).unwrap();
        assert!(encoded.len() <= MAX_PEERS_RESPONSE_SIZE);
        let decoded: Vec<String> = canonical_decode(&encoded).unwrap();
        assert!(!decoded.is_empty());
        assert!(decoded.len() < MAX_DISCOVERED_PEERS);
    }

    #[test]
    fn zero_dial_budget_does_not_connect() {
        assert!(
            connect_with_timeout("[::1]:6677", Duration::ZERO)
                .unwrap_err()
                .contains("dial timeout")
        );
    }

    #[test]
    fn dial_accepts_ipv4_ipv6_and_hostname() {
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let listener = TcpListener::bind(bind).unwrap();
            let address = listener.local_addr().unwrap();
            let stream =
                connect_with_timeout(&address.to_string(), Duration::from_secs(2)).unwrap();
            assert_eq!(stream.peer_addr().unwrap(), address);
            let _ = listener.accept().unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let stream = connect_with_timeout(
            &format!("localhost:{}", address.port()),
            Duration::from_secs(2),
        )
        .unwrap();
        assert_eq!(stream.peer_addr().unwrap(), address);
    }
}
