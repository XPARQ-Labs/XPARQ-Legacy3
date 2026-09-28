use super::state::*;
use super::*;

pub(super) fn write_frame(stream: &mut TcpStream, bytes: &[u8]) -> Result<(), String> {
    let length = u32::try_from(bytes.len()).map_err(|_| "P2P frame is too large")?;
    stream
        .write_all(&length.to_le_bytes())
        .and_then(|_| stream.write_all(bytes))
        .map_err(|error| format!("write P2P frame: {error}"))
}

pub(super) fn read_frame(stream: &mut TcpStream, maximum: usize) -> Result<Vec<u8>, String> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|error| format!("read P2P frame length: {error}"))?;
    let length = validate_frame_length(u32::from_le_bytes(length) as usize, maximum)?;
    let mut bytes = vec![0_u8; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("read P2P frame: {error}"))?;
    Ok(bytes)
}

pub(super) fn read_block_session_frame(stream: &mut TcpStream) -> Result<Vec<u8>, String> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|error| format!("read P2P frame length: {error}"))?;
    let length = validate_frame_length(
        u32::from_le_bytes(length) as usize,
        1 + MAX_STORED_BLOCK_SIZE,
    )?;
    let mut tag = [0_u8; 1];
    stream
        .read_exact(&mut tag)
        .map_err(|error| format!("read P2P frame tag: {error}"))?;
    validate_session_frame_length(length, tag[0])?;
    let mut bytes = vec![0_u8; length];
    bytes[0] = tag[0];
    stream
        .read_exact(&mut bytes[1..])
        .map_err(|error| format!("read P2P frame: {error}"))?;
    Ok(bytes)
}

pub(super) fn validate_session_frame_length(length: usize, tag: u8) -> Result<(), String> {
    if tag == SUBMIT_TRANSACTION_MESSAGE {
        validate_frame_length(length, 1 + MAX_STORED_TRANSACTION_SIZE)?;
    }
    Ok(())
}

pub(super) fn validate_frame_length(length: usize, maximum: usize) -> Result<usize, String> {
    if length == 0 || length > maximum {
        return Err("P2P frame size is outside allowed range".into());
    }
    Ok(length)
}

pub(super) fn exchange_handshake(
    database: &Path,
    stream: &mut TcpStream,
) -> Result<HandshakeExchange, String> {
    stream
        .set_read_timeout(Some(HANDSHAKE_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(HANDSHAKE_TIMEOUT)))
        .map_err(|error| format!("configure peer timeout: {error}"))?;
    let (ledger, _header_checkpoints, cumulative_work, cumulative_weight) =
        load_or_initialize_header_snapshot(database)?;
    let local = local_handshake(database, &ledger, cumulative_work, cumulative_weight)?;
    write_handshake(stream, &local)?;
    let peer = read_handshake(stream)?;
    validate_handshake(&peer)?;
    if peer.node_id == local.node_id {
        return Err("refusing self connection".into());
    }
    Ok(HandshakeExchange {
        peer,
        session_ledger: ledger,
    })
}

pub(super) fn local_handshake(
    database: &Path,
    ledger: &Ledger,
    cumulative_work: Work,
    cumulative_weight: u64,
) -> Result<Handshake, String> {
    Ok(Handshake {
        magic: P2P_MAGIC,
        protocol_version: P2P_PROTOCOL_VERSION,
        node_id: load_or_create_node_id(database)?,
        genesis_hash: EXPECTED_GENESIS_HASH.0,
        chain_spec_hash: chain_spec_hash().map_err(|error| error.to_string())?.0,
        capabilities: LOCAL_CAPABILITIES,
        tip_height: ledger.tip_height().unwrap_or(Height(0)),
        tip_hash: ledger
            .tip_hash()
            .ok_or("local chain has no canonical tip")?
            .0,
        cumulative_work: cumulative_work.to_be_limbs(),
        cumulative_weight,
    })
}

pub(super) fn write_handshake(stream: &mut TcpStream, handshake: &Handshake) -> Result<(), String> {
    let bytes = canonical_bytes(handshake).map_err(|error| error.to_string())?;
    if bytes.is_empty() || bytes.len() > MAX_HANDSHAKE_SIZE {
        return Err("local handshake exceeds size limit".into());
    }
    let length = u32::try_from(bytes.len()).map_err(|_| "handshake is too large")?;
    stream
        .write_all(&length.to_le_bytes())
        .and_then(|_| stream.write_all(&bytes))
        .map_err(|error| format!("write handshake: {error}"))
}

pub(super) fn read_handshake(stream: &mut TcpStream) -> Result<Handshake, String> {
    let mut length = [0_u8; 4];
    stream
        .read_exact(&mut length)
        .map_err(|error| format!("read handshake length: {error}"))?;
    let length = u32::from_le_bytes(length) as usize;
    if length == 0 || length > MAX_HANDSHAKE_SIZE {
        return Err("peer handshake size is outside allowed range".into());
    }
    let mut bytes = vec![0_u8; length];
    stream
        .read_exact(&mut bytes)
        .map_err(|error| format!("read handshake: {error}"))?;
    canonical_decode(&bytes).map_err(|error| format!("decode handshake: {error}"))
}

pub(super) fn validate_handshake(handshake: &Handshake) -> Result<(), String> {
    if handshake.magic != P2P_MAGIC {
        return Err("invalid P2P network magic".into());
    }
    if handshake.protocol_version != P2P_PROTOCOL_VERSION {
        return Err(format!(
            "unsupported P2P protocol version {}",
            handshake.protocol_version
        ));
    }
    if handshake.genesis_hash != EXPECTED_GENESIS_HASH.0 {
        return Err("peer genesis does not match this canonical chain".into());
    }
    if handshake.chain_spec_hash != chain_spec_hash().map_err(|error| error.to_string())?.0 {
        return Err("peer chain specification does not match this node".into());
    }
    if handshake.tip_height == Height(0) && handshake.tip_hash != EXPECTED_GENESIS_HASH.0 {
        return Err("peer reports an invalid genesis tip".into());
    }
    Ok(())
}
