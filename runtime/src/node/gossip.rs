use super::*;
use super::{mempool::*, p2p::*, protocol::*, state::*};

pub(super) fn gossip_inventory(database: &Path) -> Result<GossipInventory, String> {
    let (ledger, _header_checkpoints, cumulative_work, cumulative_weight) =
        load_or_initialize_header_snapshot(database)?;
    let tip_height = ledger.tip_height().ok_or("canonical genesis is missing")?;
    let tip_hash = ledger.tip_hash().ok_or("canonical genesis is missing")?.0;
    let transactions = read_mempool(database)?;
    let start = transactions
        .len()
        .saturating_sub(MAX_GOSSIP_INVENTORY_ITEMS);
    let hash = transactions[start..]
        .iter()
        .map(|transaction| transaction.id().map_err(|error| error.to_string()))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(GossipInventory {
        tip_height,
        tip_hash,
        cumulative_work: cumulative_work.to_be_limbs(),
        cumulative_weight,
        hash,
    })
}

pub(super) fn inventory_preferred(candidate: &GossipInventory, current: &GossipInventory) -> bool {
    compare_chain_tips(
        Work::from_be_limbs(candidate.cumulative_work),
        candidate.cumulative_weight,
        BlockHash(candidate.tip_hash),
        Work::from_be_limbs(current.cumulative_work),
        current.cumulative_weight,
        BlockHash(current.tip_hash),
    )
    .is_gt()
}

pub(super) fn handshake_from_inventory(
    handshake: &Handshake,
    inventory: &GossipInventory,
) -> Handshake {
    let mut current = handshake.clone();
    current.tip_height = inventory.tip_height;
    current.tip_hash = inventory.tip_hash;
    current.cumulative_work = inventory.cumulative_work;
    current.cumulative_weight = inventory.cumulative_weight;
    current
}

pub(super) fn decode_gossip_inventory(bytes: &[u8]) -> Result<GossipInventory, String> {
    if bytes.len() > MAX_GOSSIP_INVENTORY_SIZE {
        return Err("gossip inventory exceeds size limit".into());
    }
    let declared = bytes
        .get(112..116)
        .and_then(|count| count.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or("gossip inventory is truncated")? as usize;
    if declared > MAX_GOSSIP_INVENTORY_ITEMS {
        return Err("gossip inventory item count exceeds limit".into());
    }
    let inventory: GossipInventory =
        canonical_decode(bytes).map_err(|error| format!("decode gossip inventory: {error}"))?;
    if inventory.hash.len() > MAX_GOSSIP_INVENTORY_ITEMS {
        return Err("gossip inventory item count exceeds limit".into());
    }
    Ok(inventory)
}

pub(super) fn request_discovered_peers(stream: &mut TcpStream) -> Result<Vec<PeerAddress>, String> {
    write_frame(stream, &[GET_PEERS_MESSAGE])?;
    let response = read_frame(stream, 1 + MAX_PEERS_RESPONSE_SIZE)?;
    if response.first() != Some(&PEERS_MESSAGE) {
        return Err("peer returned an unexpected discovery response".into());
    }
    let declared_peers = response
        .get(1..5)
        .and_then(|bytes| bytes.try_into().ok())
        .map(u32::from_le_bytes)
        .ok_or("peer discovery response is truncated")? as usize;
    if declared_peers > MAX_DISCOVERED_PEERS {
        return Err("peer discovery response exceeds peer limit".into());
    }
    let peers: Vec<String> = canonical_decode(&response[1..])
        .map_err(|error| format!("decode discovered peers: {error}"))?;
    if peers.len() > MAX_DISCOVERED_PEERS {
        return Err("peer discovery response exceeds peer limit".into());
    }
    Ok(peers
        .into_iter()
        .filter_map(|peer| peer.parse().ok())
        .filter(PeerAddress::is_admissible)
        .collect())
}

pub(super) fn relay_mempool(database: &Path, stream: &mut TcpStream) -> Result<(), String> {
    for transaction in read_mempool(database)?
        .into_iter()
        .take(MAX_RELAY_ITEMS_PER_SESSION)
    {
        let encoded = canonical_bytes(&transaction).map_err(|error| error.to_string())?;
        let mut message = Vec::with_capacity(1 + encoded.len());
        message.push(SUBMIT_TRANSACTION_MESSAGE);
        message.extend_from_slice(&encoded);
        write_frame(stream, &message)?;
        read_relay_result(stream)?;
    }
    Ok(())
}

pub(super) fn gossip_outbound_session(
    database: &Path,
    stream: &mut TcpStream,
    peer: &Handshake,
) -> Result<(), String> {
    if peer.capabilities & CAPABILITY_RELAY == 0 {
        write_frame(stream, &[SYNC_COMPLETE_MESSAGE])?;
        return Ok(());
    }

    let mut generation = gossip_generation()?;

    loop {
        let local_before = gossip_inventory(database)?;
        let encoded = canonical_bytes(&local_before).map_err(|error| error.to_string())?;

        let mut message = Vec::with_capacity(1 + encoded.len());
        message.push(INVENTORY_MESSAGE);
        message.extend_from_slice(&encoded);
        write_frame(stream, &message)?;

        let response = read_frame(stream, 1 + MAX_GOSSIP_INVENTORY_SIZE)?;

        if response.first() != Some(&INVENTORY_MESSAGE) {
            return Err("peer returned an unexpected gossip inventory response".into());
        }

        let remote = decode_gossip_inventory(&response[1..])?;

        if remote.tip_hash != local_before.tip_hash {
            if inventory_preferred(&remote, &local_before) {
                if remote.tip_height.0 == local_before.tip_height.0.saturating_add(1) {
                    request_and_accept_gossip_block(database, stream, remote.tip_hash)?;
                    generation = gossip_generation()?;
                    continue;
                }

                return Err(format!(
                    "{GOSSIP_RESYNC_PREFIX} peer announced a preferred chain"
                ));
            }

            if inventory_preferred(&local_before, &remote) {
                let ledger = load_or_initialize(database)?;
                let tip_height = local_before.tip_height;

                let advertised_tip_matches = ledger
                    .chain
                    .block(&tip_height)
                    .and_then(|block| block.hash().ok())
                    .is_some_and(|hash| hash.0 == local_before.tip_hash);

                if !advertised_tip_matches {
                    return Err(format!(
                        "{GOSSIP_RESYNC_PREFIX} advertised local tip changed before reverse sync"
                    ));
                }

                match serve_peer_requests_through(database, stream, &ledger, tip_height)? {
                    PeerSessionOutcome::Complete => return Ok(()),
                    PeerSessionOutcome::ReverseSync(_) => {
                        return Err("peer requested nested reverse synchronization".into());
                    }
                }
            }

            return Err(format!(
                "{GOSSIP_RESYNC_PREFIX} tips diverged; reconnecting for verified header sync"
            ));
        }

        exchange_gossip_transactions(database, stream, &remote)?;
        generation = wait_for_gossip(generation)?;
    }
}

pub(super) fn request_and_accept_gossip_block(
    database: &Path,
    stream: &mut TcpStream,
    block_hash: [u8; 32],
) -> Result<(), String> {
    let mut request = Vec::with_capacity(33);
    request.push(GET_BLOCK_MESSAGE);
    request.extend_from_slice(&block_hash);
    write_frame(stream, &request)?;
    let response = read_frame(stream, 1 + MAX_STORED_BLOCK_SIZE)?;
    if response.first() != Some(&BLOCK_MESSAGE) {
        return Err("peer returned an unexpected gossip block response".into());
    }
    let block = decode_block(&response[1..])
        .map_err(|error| format!("invalid gossip block response: {error}"))?;
    if block.hash().map_err(|error| error.to_string())?.0 != block_hash {
        return Err("gossip block body does not match announced hash".into());
    }
    accept_relayed_block(database, &response[1..])
}

pub(super) fn exchange_gossip_transactions(
    database: &Path,
    stream: &mut TcpStream,
    remote: &GossipInventory,
) -> Result<(), String> {
    let local = gossip_inventory(database)?;
    let local_ids = local.hash.iter().copied().collect::<BTreeSet<_>>();
    for hash in remote
        .hash
        .iter()
        .filter(|hash| !local_ids.contains(*hash))
        .take(MAX_RELAY_ITEMS_PER_SESSION)
    {
        let mut request = Vec::with_capacity(33);
        request.push(GET_TRANSACTION_MESSAGE);
        request.extend_from_slice(hash);
        write_frame(stream, &request)?;
        let response = read_frame(stream, 1 + MAX_STORED_TRANSACTION_SIZE)?;
        if response.first() != Some(&TRANSACTION_MESSAGE) {
            return Err("peer returned an unexpected transaction response".into());
        }
        let transaction: Transaction = canonical_decode(&response[1..])
            .map_err(|error| format!("invalid gossip transaction: {error}"))?;
        if transaction.id().map_err(|error| error.to_string())? != *hash {
            return Err("gossip transaction body does not match announced ID".into());
        }
        accept_relayed_transaction(database, &response[1..])?;
    }

    let remote_ids = remote.hash.iter().copied().collect::<BTreeSet<_>>();
    for transaction in read_mempool(database)?
        .into_iter()
        .filter(|transaction| {
            transaction
                .id()
                .is_ok_and(|hash| !remote_ids.contains(&hash))
        })
        .take(MAX_RELAY_ITEMS_PER_SESSION)
    {
        let encoded = canonical_bytes(&transaction).map_err(|error| error.to_string())?;
        let mut message = Vec::with_capacity(1 + encoded.len());
        message.push(SUBMIT_TRANSACTION_MESSAGE);
        message.extend_from_slice(&encoded);
        write_frame(stream, &message)?;
        read_relay_result(stream)?;
    }
    Ok(())
}

pub(super) fn gossip_notifier() -> &'static (Mutex<u64>, Condvar) {
    GOSSIP_NOTIFIER.get_or_init(|| (Mutex::new(0), Condvar::new()))
}

pub(super) fn gossip_generation() -> Result<u64, String> {
    gossip_notifier()
        .0
        .lock()
        .map(|generation| *generation)
        .map_err(|_| "gossip notifier lock is poisoned".into())
}

pub(super) fn notify_gossip() {
    let (generation, wake) = gossip_notifier();
    if let Ok(mut generation) = generation.lock() {
        *generation = generation.wrapping_add(1);
        wake.notify_all();
    }
}

pub(super) fn wait_for_gossip(previous: u64) -> Result<u64, String> {
    let (generation, wake) = gossip_notifier();
    let generation = generation
        .lock()
        .map_err(|_| "gossip notifier lock is poisoned")?;
    if *generation != previous {
        return Ok(*generation);
    }
    let (generation, _) = wake
        .wait_timeout(generation, GOSSIP_HEARTBEAT)
        .map_err(|_| "gossip notifier lock is poisoned")?;
    Ok(*generation)
}

pub(super) fn gossip_error_is_malicious(error: &str) -> bool {
    error.contains("invalid gossip")
        || error.contains("does not match announced")
        || error.contains("exceeds")
        || error.contains("invalid relayed")
        || error.contains("unexpected gossip")
}

pub(super) fn relay_next_block(
    database: &Path,
    stream: &mut TcpStream,
    peer_height: Height,
    peer_tip_hash: [u8; 32],
) -> Result<bool, String> {
    let ledger = load_or_initialize(database)?;
    let Some(local_height) = ledger.tip_height() else {
        return Ok(false);
    };
    if local_height.0 != peer_height.0.saturating_add(1) {
        return Ok(false);
    }
    let block = ledger
        .chain
        .block(&local_height)
        .ok_or("local relay tip block is missing")?;
    if block.previous_hash().0 != peer_tip_hash {
        return Ok(false);
    }
    let encoded = block_bytes(block).map_err(|error| error.to_string())?;
    let mut message = Vec::with_capacity(1 + encoded.len());
    message.push(SUBMIT_BLOCK_MESSAGE);
    message.extend_from_slice(&encoded);
    write_frame(stream, &message)?;
    read_relay_result(stream)?;
    Ok(true)
}

pub(super) fn read_relay_result(stream: &mut TcpStream) -> Result<(), String> {
    let response = read_frame(stream, 1024)?;
    match response.first() {
        Some(&ACCEPTED_MESSAGE) => Ok(()),
        Some(&REJECTED_MESSAGE) => Err(String::from_utf8_lossy(&response[1..]).into_owned()),
        _ => Err("peer returned an invalid relay response".into()),
    }
}

pub(super) fn write_relay_result(
    stream: &mut TcpStream,
    result: Result<(), String>,
) -> Result<(), String> {
    match result {
        Ok(()) => write_frame(stream, &[ACCEPTED_MESSAGE]),
        Err(error) => {
            let mut response = Vec::with_capacity(1 + error.len().min(1023));
            response.push(REJECTED_MESSAGE);
            response.extend_from_slice(&error.as_bytes()[..error.len().min(1023)]);
            write_frame(stream, &response)
        }
    }
}

pub(super) fn accept_relayed_block(database: &Path, bytes: &[u8]) -> Result<(), String> {
    let block = decode_block(bytes).map_err(|error| format!("invalid relayed block: {error}"))?;
    let hash = block.hash().map_err(|error| error.to_string())?;
    let _mutation = state_mutation_lock()?
        .lock()
        .map_err(|_| "state mutation lock is poisoned")?;
    let mut ledger = load_or_initialize_owned(database)?;
    if ledger.tip_hash() == Some(hash) {
        return Ok(());
    }
    if ledger.tip_hash().map(|tip| tip.0) != Some(block.previous_hash().0) {
        return Err(format!(
            "{GOSSIP_RESYNC_PREFIX} relayed block does not directly extend the canonical tip"
        ));
    }
    apply_block(&mut ledger, block.clone())
        .map_err(|error| format!("invalid relayed block: {error}"))?;
    let included = block
        .transactions()
        .iter()
        .map(|transaction| transaction.id().map_err(|error| error.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let mempool = reconcile_mempool(&ledger, read_mempool(database)?, &included);
    persist_block_and_mempool(database, &block, &mempool)?;
    let _ = update_ledger_cache(database, ledger)?;
    notify_gossip();
    Ok(())
}

#[cfg(test)]
mod discovery_tests {
    use super::*;

    #[test]
    fn discovery_accepts_dns_strings_without_resolving_and_filters_private_ips() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            assert_eq!(read_frame(&mut stream, 1).unwrap(), [GET_PEERS_MESSAGE]);
            let endpoints = vec![
                "node.example.invalid:6677",
                "[2606:4700::1111]:6677",
                "127.0.0.1:6677",
                "node.local:6677",
                "bad host:6677",
            ];
            let mut response = vec![PEERS_MESSAGE];
            response.extend(canonical_bytes(&endpoints).unwrap());
            write_frame(&mut stream, &response).unwrap();
        });
        let mut stream = TcpStream::connect(address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let peers = request_discovered_peers(&mut stream).unwrap();
        assert_eq!(
            peers.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["node.example.invalid:6677", "[2606:4700::1111]:6677"]
        );
        server.join().unwrap();
    }
}
