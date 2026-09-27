use super::*;
use super::{gossip::*, mempool::*, protocol::*, state::*};

pub(super) fn synchronize_headers(
    database: &Path,
    stream: &mut TcpStream,
    peer: &Handshake,
) -> Result<HeaderSyncResult, String> {
    let (ledger, header_checkpoints, local_cumulative_work, local_cumulative_weight) =
        load_or_initialize_header_snapshot(database)?;

    let local_locator = ledger_header_locator(&ledger)?;

    let mut validation_state = None;
    let mut ancestor_height = None;
    let mut ancestor_hash = None;
    let mut downloaded = Vec::new();

    let mut request_locator = local_locator
        .iter()
        .map(|(hash, _)| *hash)
        .collect::<Vec<_>>();

    let mut verified_headers = 0_usize;
    let mut pow_memory = None;

    loop {
        write_frame(stream, &encode_locator(&request_locator)?)?;

        let response = read_frame(stream, 33 + MAX_HEADER_CHAIN_CHUNK_SIZE)?;

        let (&message, body) = response.split_first().ok_or("empty header response")?;

        let ancestor: [u8; 32] = body
            .get(..32)
            .ok_or("header response has no ancestor")?
            .try_into()
            .map_err(|_| "invalid ancestor hash")?;

        if message == HEADERS_COMPLETE_MESSAGE {
            let state = match validation_state.take() {
                Some(state) => state,

                None => {
                    let height = local_locator
                        .iter()
                        .find_map(|(hash, height)| (*hash == ancestor).then_some(*height))
                        .ok_or("common ancestor is not in the local locator")?;

                    ledger_header_state_at_height(&ledger, header_checkpoints.as_slice(), height)?
                }
            };

            if state.header.hash().map_err(|error| error.to_string())?.0 != ancestor {
                return Err("peer completion does not match verified header tip".into());
            }

            if state.header.hash().map_err(|error| error.to_string())?.0 != peer.tip_hash
                || state.cumulative_work.to_be_limbs() != peer.cumulative_work
                || state.cumulative_weight != peer.cumulative_weight
            {
                return Err("peer handshake tip/work does not match verified headers".into());
            }

            let local_hash = ledger.tip_hash().ok_or("local header chain has no tip")?.0;
            let peer_work = state.cumulative_work;
            let peer_weight = state.cumulative_weight;

            let preferred = compare_chain_tips(
                peer_work,
                peer_weight,
                BlockHash(peer.tip_hash),
                local_cumulative_work,
                local_cumulative_weight,
                BlockHash(local_hash),
            )
            .is_gt();

            return Ok(HeaderSyncResult {
                ancestor_height: ancestor_height.unwrap_or(state.height),
                ancestor_hash: BlockHash(ancestor_hash.unwrap_or(ancestor)),
                headers: downloaded,
                peer_work,
                peer_weight,
                preferred,
            });
        }

        if message != HEADERS_MESSAGE {
            return Err("unexpected P2P message during header sync".into());
        }

        let chunk = decode_header_chain_chunk(&body[32..]).map_err(|error| error.to_string())?;

        let current = match validation_state.take() {
            Some(current) => {
                if current.header.hash().map_err(|error| error.to_string())?.0 != ancestor {
                    return Err("peer changed common ancestor during header sync".into());
                }

                current
            }

            None => {
                let height = local_locator
                    .iter()
                    .find_map(|(hash, height)| (*hash == ancestor).then_some(*height))
                    .ok_or("peer response ancestor is not in the local locator")?;

                ledger_header_state_at_height(&ledger, header_checkpoints.as_slice(), height)?
            }
        };

        if ancestor_height.is_none() {
            ancestor_height = Some(current.height);
            ancestor_hash = Some(ancestor);
        }

        let advanced = kernel::consensus::advance_header_validation_state_with_memory(
            &current,
            &chunk.headers,
            pow_memory.get_or_insert_with(new_pow_memory),
        )
        .map_err(map_peer_header_error)?;

        verified_headers = verified_headers
            .checked_add(chunk.headers.len())
            .ok_or("verified header count overflow")?;

        if verified_headers > MAX_SYNC_HEADERS {
            return Err("header synchronization exceeds session limit".into());
        }

        if verified_headers.is_multiple_of(256) {
            println!(
                "sync_progress: verified_headers={verified_headers} peer_height={}",
                peer.tip_height.0
            );
        }

        let tip_hash = advanced.header.hash().map_err(|error| error.to_string())?.0;

        downloaded.extend(chunk.headers);
        validation_state = Some(advanced);

        request_locator = vec![tip_hash, EXPECTED_GENESIS_HASH.0];
    }
}

pub(super) fn map_peer_header_error(error: kernel::consensus::HeaderChainError) -> String {
    let invalid_pow = matches!(
        error,
        kernel::consensus::HeaderChainError::InvalidHeaderChain(
            kernel::consensus::ForkChoiceError::InvalidProofOfWork(_)
        )
    );
    if invalid_pow {
        format!("{INVALID_POW_ERROR_PREFIX} {error}")
    } else {
        format!("peer header extension is invalid: {error}")
    }
}

pub(super) fn reconnect_delay_for_error(error: &str, failures: u32, peer: &str) -> Duration {
    if error.starts_with(INVALID_POW_ERROR_PREFIX) {
        INVALID_POW_COOLDOWN
    } else {
        let exponent = failures.saturating_sub(1).min(5);
        let base = RECONNECT_INTERVAL.as_secs() * (1_u64 << exponent);
        let jitter = peer.bytes().fold(failures as u64, |value, byte| {
            value.wrapping_mul(33) ^ byte as u64
        }) % 10;
        Duration::from_secs((base + jitter).min(300))
    }
}

pub(super) fn synchronize_blocks(
    database: &Path,
    stream: &mut TcpStream,
    sync: HeaderSyncResult,
) -> Result<usize, String> {
    let mut blocks = Vec::with_capacity(sync.headers.len());
    for expected in &sync.headers {
        let expected_hash = expected.hash().map_err(|error| error.to_string())?;
        let mut request = Vec::with_capacity(33);
        request.push(GET_BLOCK_MESSAGE);
        request.extend_from_slice(&expected_hash.0);
        write_frame(stream, &request)?;
        let response = read_frame(stream, 1 + MAX_STORED_BLOCK_SIZE)?;
        if response.first() != Some(&BLOCK_MESSAGE) {
            return Err("peer returned an unexpected block response".into());
        }
        let block =
            decode_block(&response[1..]).map_err(|error| format!("invalid peer block: {error}"))?;
        if block.height() != expected.height
            || block.header != expected.header
            || block.hash().map_err(|error| error.to_string())? != expected_hash
        {
            return Err("peer block body does not match the verified header".into());
        }
        blocks.push(block);
    }
    apply_verified_branch(database, sync, blocks)
}

pub(super) fn apply_verified_branch(
    database: &Path,
    sync: HeaderSyncResult,
    blocks: Vec<Block>,
) -> Result<usize, String> {
    if blocks.len() != sync.headers.len() {
        return Err("downloaded block count does not match verified headers".into());
    }
    for (block, expected) in blocks.iter().zip(&sync.headers) {
        if block.height() != expected.height
            || block.header != expected.header
            || block.hash().map_err(|error| error.to_string())?
                != expected.hash().map_err(|error| error.to_string())?
        {
            return Err("downloaded block does not match verified header".into());
        }
    }
    let count = blocks.len();
    let included = blocks
        .iter()
        .flat_map(|block| block.transactions())
        .map(|transaction| transaction.id().map_err(|error| error.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    let _mutation = state_mutation_lock()?
        .lock()
        .map_err(|_| "state mutation lock is poisoned")?;
    let (cached_ledger, _header_checkpoints, current_cumulative_work, current_cumulative_weight) =
        load_or_initialize_header_snapshot(database)?;
    let mut staged = cached_ledger.as_ref().clone();
    let old_tip = staged.tip_hash();
    let new_tip = blocks
        .last()
        .map(Block::hash)
        .transpose()
        .map_err(|error| error.to_string())?
        .unwrap_or(sync.ancestor_hash);
    let current_tip = old_tip.ok_or("canonical chain has no tip during reorg")?;
    if !compare_chain_tips(
        sync.peer_work,
        sync.peer_weight,
        new_tip,
        current_cumulative_work,
        current_cumulative_weight,
        current_tip,
    )
    .is_gt()
    {
        println!("sync: downloaded peer branch is no longer preferred after local tip advanced");
        return Ok(0);
    }
    let mut disconnect = Vec::new();
    let mut height = staged.tip_height();
    while height.is_some_and(|height| height > sync.ancestor_height) {
        let current = height.expect("height was checked above");
        disconnect.push(
            staged
                .chain
                .block(&current)
                .cloned()
                .ok_or("reorg disconnect block is missing from canonical chain")?,
        );
        height = current.0.checked_sub(1).map(Height);
    }
    let ancestor = staged
        .chain
        .block(&sync.ancestor_height)
        .ok_or("reorg ancestor is missing from canonical chain")?;
    if ancestor.hash().map_err(|error| error.to_string())? != sync.ancestor_hash {
        return Err("reorg ancestor hash does not match canonical chain".into());
    }
    let plan = ReorgPlan::new(sync.ancestor_hash, old_tip, new_tip, disconnect, blocks)
        .map_err(|error| format!("invalid canonical reorg plan: {error}"))?;
    let ancestor = plan.ancestor();
    let (disconnect, apply) = plan.into_branches();
    let disconnected_blocks = disconnect.len();
    let disconnected_transactions = disconnect
        .iter()
        .rev()
        .flat_map(|block| block.transactions().iter().cloned())
        .collect::<Vec<_>>();
    let disconnected_hash = disconnected_transactions
        .iter()
        .map(|transaction| transaction.id().map_err(|error| error.to_string()))
        .collect::<Result<BTreeSet<_>, _>>()?;
    for expected in disconnect {
        let removed = staged
            .rollback_tip()
            .map_err(|error| format!("rollback canonical tip: {error}"))?;
        if removed.hash().map_err(|error| error.to_string())?
            != expected.hash().map_err(|error| error.to_string())?
        {
            return Err("rollback removed a block outside the reorg disconnect plan".into());
        }
    }
    if staged.tip_hash() != Some(ancestor) {
        return Err("rollback did not stop at the planned common ancestor".into());
    }
    for block in apply {
        apply_block(&mut staged, block)
            .map_err(|error| format!("apply synchronized block: {error}"))?;
    }
    let mut mempool_candidates = disconnected_transactions;
    mempool_candidates.extend(read_mempool(database)?);
    let mempool = reconcile_mempool(&staged, mempool_candidates, &included);
    let requeued_transactions = mempool
        .iter()
        .filter_map(|transaction| transaction.id().ok())
        .filter(|hash| disconnected_hash.contains(hash))
        .count();
    persist_chain_and_mempool(database, &staged, &mempool)?;
    let staged = update_ledger_cache(database, staged)?;
    if let Err(error) = crate::snapshot::write_after_large_sync(database, &staged, count) {
        eprintln!("node: post-sync snapshot write failed: {error}");
    }
    notify_gossip();
    if disconnected_blocks > 0 {
        println!(
            "reorg: disconnected_blocks={disconnected_blocks} disconnected_transactions={} requeued_transactions={requeued_transactions}",
            disconnected_hash.len(),
        );
    }
    Ok(count)
}

pub(super) fn ledger_header_locator(ledger: &Ledger) -> Result<Vec<([u8; 32], Height)>, String> {
    let tip = ledger.tip_height().ok_or("local header chain is empty")?;

    let mut locator = Vec::new();
    let mut height = tip.0;
    let mut step = 1_u64;

    loop {
        let current = Height(height);

        let block = ledger
            .chain
            .block(&current)
            .ok_or("canonical header is missing")?;

        let hash = block.header.hash().map_err(|error| error.to_string())?.0;

        locator.push((hash, current));

        if height == 0 || locator.len() == MAX_LOCATOR_HASHES - 1 {
            break;
        }

        height = height.saturating_sub(step);

        if locator.len() >= 10 {
            step = step.saturating_mul(2);
        }
    }

    if locator
        .last()
        .is_none_or(|(hash, _)| *hash != EXPECTED_GENESIS_HASH.0)
    {
        locator.push((EXPECTED_GENESIS_HASH.0, Height(0)));
    }

    Ok(locator)
}

pub(super) fn ledger_header_state_at_height(
    ledger: &Ledger,
    checkpoints: &[HeaderStateCheckpoint],
    target_height: Height,
) -> Result<kernel::consensus::HeaderValidationState, String> {
    let target_block = ledger
        .chain
        .block(&target_height)
        .ok_or("canonical ancestor height is missing")?;

    let checkpoint = checkpoints
        .iter()
        .rfind(|checkpoint| checkpoint.height <= target_height)
        .ok_or("no header state checkpoint covers target height")?;

    let checkpoint_block = ledger
        .chain
        .block(&checkpoint.height)
        .ok_or("checkpoint block is missing from canonical chain")?;

    let checkpoint_hash = checkpoint_block
        .hash()
        .map_err(|error| error.to_string())?
        .0;

    if checkpoint_hash != checkpoint.hash {
        return Err("header state checkpoint does not match canonical chain".into());
    }

    let mut cumulative_work = checkpoint.cumulative_work;
    let mut cumulative_weight = checkpoint.cumulative_weight;

    let mut next_height = checkpoint.height.0.checked_add(1);

    while let Some(value) = next_height {
        if value > target_height.0 {
            break;
        }

        let height = Height(value);

        let block = ledger
            .chain
            .block(&height)
            .ok_or("canonical block is missing after checkpoint")?;

        let block_work = kernel::consensus::block_work(block.target_bits()).ok_or_else(|| {
            format!(
                "invalid target bits {:08x} at height {}",
                block.target_bits(),
                height.0,
            )
        })?;

        cumulative_work = cumulative_work.saturating_add(block_work);

        cumulative_weight = cumulative_weight.saturating_add(u64::from(block.block_weight()));

        next_height = value.checked_add(1);
    }

    let difficulty_anchor_height = if target_height.0 == 0 {
        Height(0)
    } else {
        Height(1)
    };

    let difficulty_anchor_block = ledger
        .chain
        .block(&difficulty_anchor_height)
        .ok_or("difficulty anchor is missing from canonical chain")?;

    let difficulty_anchor = kernel::consensus::HeaderAtHeight::new(
        difficulty_anchor_height,
        difficulty_anchor_block.header.clone(),
    );

    let mut recent_headers = Vec::new();

    if kernel::consensus::RECENT_HEADER_WINDOW > 0 {
        let window = u64::try_from(kernel::consensus::RECENT_HEADER_WINDOW).unwrap_or(u64::MAX);

        let start = target_height.0.saturating_add(1).saturating_sub(window);

        let mut height = start;

        loop {
            let current = Height(height);

            let block = ledger
                .chain
                .block(&current)
                .ok_or("recent canonical header is missing")?;

            recent_headers.push(kernel::consensus::HeaderAtHeight::new(
                current,
                block.header.clone(),
            ));

            if height == target_height.0 {
                break;
            }

            height = height
                .checked_add(1)
                .ok_or("recent header height overflow")?;
        }
    }

    Ok(kernel::consensus::HeaderValidationState {
        height: target_height,
        header: target_block.header.clone(),
        cumulative_work,
        cumulative_weight,
        difficulty_anchor,
        recent_headers,
    })
}

pub(super) fn encode_locator(locator: &[[u8; 32]]) -> Result<Vec<u8>, String> {
    if locator.is_empty() || locator.len() > MAX_LOCATOR_HASHES {
        return Err("header locator count is outside allowed range".into());
    }
    let mut bytes = Vec::with_capacity(2 + locator.len() * 32);
    bytes.push(GET_HEADERS_MESSAGE);
    bytes.push(locator.len() as u8);
    for hash in locator {
        bytes.extend_from_slice(hash);
    }
    Ok(bytes)
}

pub(super) fn decode_locator(bytes: &[u8]) -> Result<Vec<[u8; 32]>, String> {
    if bytes.first() != Some(&GET_HEADERS_MESSAGE) {
        return Err("expected get-headers message".into());
    }
    let count = bytes.get(1).copied().ok_or("missing locator count")? as usize;
    if count == 0 || count > MAX_LOCATOR_HASHES || bytes.len() != 2 + count * 32 {
        return Err("invalid header locator size".into());
    }
    bytes[2..]
        .chunks_exact(32)
        .map(|chunk| {
            chunk
                .try_into()
                .map_err(|_| "invalid locator hash".to_string())
        })
        .collect()
}
