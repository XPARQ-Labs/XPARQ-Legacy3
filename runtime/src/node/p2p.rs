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
    thread::spawn(move || {
        let mut active = BTreeSet::new();
        loop {
            let mut candidates = configured.clone();
            match PeerStore::load(&database) {
                Ok(store) => {
                    candidates.extend(store.addresses().into_iter().map(|peer| peer.to_string()))
                }
                Err(error) => eprintln!("node: load peer store: {error}"),
            }
            for peer in candidates {
                if !active.insert(peer.clone()) {
                    continue;
                }
                let database = database.clone();
                let sync_lock = Arc::clone(&sync_lock);
                thread::spawn(move || {
                    let mut consecutive_failures = 0_u32;
                    loop {
                        let result = match sync_lock.lock() {
                            Ok(_guard) => connect_peer_database(&database, &peer),
                            Err(_) => Err("outbound sync lock is poisoned".into()),
                        };
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
                                        consecutive_failures =
                                            consecutive_failures.saturating_add(1);
                                        let malicious = gossip_error_is_malicious(&error);
                                        if let Ok(address) = peer.parse() {
                                            let _ =
                                                record_peer_failure(&database, address, malicious);
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
                        thread::sleep(reconnect_after);
                    }
                });
            }
            thread::sleep(RECONNECT_INTERVAL);
        }
    });
}

pub(super) fn connect_peer(path: Option<&str>, peer: &str) -> Result<(), String> {
    let database = database_path(path);
    let mut connection = connect_peer_database(&database, peer)?;
    write_frame(&mut connection.stream, &[SYNC_COMPLETE_MESSAGE])
}

pub(super) fn connect_peer_database(database: &Path, peer: &str) -> Result<ConnectedPeer, String> {
    load_or_initialize(database)?;
    let mut stream = TcpStream::connect(peer).map_err(|error| format!("connect peer: {error}"))?;
    let connected_address = stream.peer_addr().ok();
    let handshake = exchange_handshake(database, &mut stream)?.peer;
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
    if let Some(address) = connected_address {
        record_peer_success(database, address)?;
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
                let mut peers = PeerStore::load(database)?.relay_addresses();
                if let Some(address) = advertised_peer()? {
                    let address = address.to_string();
                    if !peers.contains(&address) {
                        peers.push(address);
                    }
                }
                peers.truncate(MAX_DISCOVERED_PEERS);
                let encoded = canonical_bytes(&peers).map_err(|error| error.to_string())?;
                if encoded.len() > MAX_PEERS_RESPONSE_SIZE {
                    return Err("local peer response exceeds size limit".into());
                }
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

pub(super) fn record_discovered_peer(database: &Path, address: SocketAddr) -> Result<(), String> {
    let _guard = peer_store_lock()?
        .lock()
        .map_err(|_| "peer store lock is poisoned")?;
    let mut store = PeerStore::load(database)?;
    if store.insert_discovered(address) {
        store.save(database)?;
    }
    Ok(())
}

pub(super) fn record_peer_success(database: &Path, address: SocketAddr) -> Result<(), String> {
    let _guard = peer_store_lock()?
        .lock()
        .map_err(|_| "peer store lock is poisoned")?;
    let mut store = PeerStore::load(database)?;
    store.record_success(address);
    store.save(database)
}

pub(super) fn record_peer_failure(
    database: &Path,
    address: SocketAddr,
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
