use super::*;
use super::{config::*, explorer::*, mempool::*, mining::*, state::*, util::*};

pub(super) fn serve_rpc(path: Option<&str>, listen: &str) -> Result<(), String> {
    let database = database_path(path);
    serve_rpc_database(database, listen)
}

pub(super) fn serve_rpc_database(database: PathBuf, listen: &str) -> Result<(), String> {
    load_or_initialize(&database)?;
    let listener = TcpListener::bind(listen).map_err(|error| format!("bind RPC: {error}"))?;
    println!("rpc: http://{listen}");

    let active = Arc::new(AtomicUsize::new(0));
    let active_by_ip = Arc::new(Mutex::new(std::collections::BTreeMap::new()));

    for connection in listener.incoming() {
        match connection {
            Ok(mut stream) => {
                if active.fetch_add(1, Ordering::AcqRel) >= MAX_RPC_CONNECTIONS {
                    active.fetch_sub(1, Ordering::AcqRel);
                    let _ = write_http_response(
                        &mut stream,
                        503,
                        &serde_json::json!({"error": "RPC connection limit reached"}),
                    );
                    continue;
                }

                let peer_ip = stream.peer_addr().ok().map(|address| address.ip());
                if let Some(ip) = peer_ip {
                    let mut by_ip = match active_by_ip.lock() {
                        Ok(by_ip) => by_ip,
                        Err(_) => {
                            active.fetch_sub(1, Ordering::AcqRel);
                            let _ = write_http_response(
                                &mut stream,
                                503,
                                &serde_json::json!({"error": "RPC connection accounting unavailable"}),
                            );
                            continue;
                        }
                    };
                    let count = by_ip.entry(ip).or_insert(0_usize);
                    if *count >= MAX_RPC_CONNECTIONS_PER_IP {
                        active.fetch_sub(1, Ordering::AcqRel);
                        let _ = write_http_response(
                            &mut stream,
                            429,
                            &serde_json::json!({"error": "RPC per-IP connection limit reached"}),
                        );
                        continue;
                    }
                    *count += 1;
                }

                let database = database.clone();
                let active = Arc::clone(&active);
                let active_by_ip = Arc::clone(&active_by_ip);
                thread::spawn(move || {
                    if let Err(error) = handle_rpc_connection(&database, &mut stream) {
                        let _ = write_http_response(
                            &mut stream,
                            400,
                            &serde_json::json!({"error": error}),
                        );
                    }

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
            Err(error) => eprintln!("node: accept RPC connection: {error}"),
        }
    }
    Ok(())
}

pub(super) fn handle_rpc_connection(database: &Path, stream: &mut TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(RPC_TIMEOUT))
        .and_then(|_| stream.set_write_timeout(Some(RPC_TIMEOUT)))
        .map_err(|error| format!("configure RPC timeout: {error}"))?;
    let request = read_http_request(stream)?;
    let request_line = request.headers.lines().next().ok_or("empty RPC request")?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or("missing RPC method")?;
    let route = parts.next().ok_or("missing RPC route")?;
    if method == "POST" && route == "/transaction" {
        let transaction: Transaction = canonical_decode(&request.body)
            .map_err(|error| format!("invalid submitted transaction: {error}"))?;
        let hash = insert_mempool_transaction(database, transaction, false)?;
        return write_http_response(stream, 200, &serde_json::json!({"hash": hex::encode(hash)}));
    }
    if method != "GET" {
        return Err("unsupported RPC method".into());
    }
    if !request.body.is_empty() {
        return Err("GET request body is not allowed".into());
    }
    match route {
        "/openapi.json" => {
            return write_http_bytes(stream, 200, "application/json", OPENAPI_JSON);
        }
        "/docs" | "/docs/" => {
            return write_http_bytes(stream, 200, "text/html; charset=utf-8", API_DOCS_HTML);
        }
        _ => {}
    }

    let (ledger, _header_checkpoints, cumulative_work, cumulative_weight) =
        load_or_initialize_header_snapshot(database)?;

    let response = match route {
        "/status" => status_response(&ledger, cumulative_work, cumulative_weight)?,
        "/fee-policy" => {
            let emission = expected_next_emission(&ledger)?;
            serde_json::json!({
                "minimum_fee_rate_zeno_per_byte": MIN_RELAY_FEE_ZENO_PER_BYTE,
                "state_burn_algorithm": kernel::consensus::STATE_BURN_ALGORITHM,
                "next_block_emission": emission.as_zeno(),
                "state_burn_rate_zeno_per_weight": kernel::consensus::STATE_BURN_RATE_ZENO_PER_WEIGHT,
                "empty_block_archival_bytes": kernel::consensus::EMPTY_BLOCK_ARCHIVAL_BYTES,
                "coin_utxo_state_weight": kernel::consensus::COIN_UTXO_STATE_WEIGHT,
                "empty_block_archival_burn": kernel::consensus::EMPTY_BLOCK_ARCHIVAL_BURN.as_zeno(),
                "emission_utxo_state_growth_burn": kernel::consensus::EMISSION_UTXO_STATE_GROWTH_BURN.as_zeno(),
                "miner_protocol_burn": kernel::consensus::MINER_PROTOCOL_BURN.as_zeno(),
            })
        }
        "/blocks/latest" => latest_blocks_response(&ledger)?,
        route if route.starts_with("/coin-origin/") => {
            let share = route.trim_start_matches("/coin-origin/");
            if share.is_empty() || share.contains(['/', '?', '#']) {
                return Err("invalid coin origin route".into());
            }
            let share = share
                .parse::<kernel::monetary::coin::CoinShare>()
                .map_err(|_| "invalid coin share")?;
            let origin = index::coin_origin(database, &ledger, share)?;
            serde_json::json!({
                "coin_share": share.to_string(),
                "origin": origin.map(|origin| serde_json::json!({
                    "created_at": origin.created_at.map(|id| hex::encode(id.into_bytes())),
                    "created_in": origin.created_in.0,
                })),
            })
        }
        route if route.starts_with("/asset/") => asset_response(&ledger, route)?,
        route if route.starts_with("/balance/") => {
            let address = route.trim_start_matches("/balance/");
            if address.is_empty() || address.contains(['/', '?', '#']) {
                return Err("invalid balance route".into());
            }
            balance_response(&ledger, &read_mempool(database)?, parse_address(address)?)?
        }
        route if route.starts_with("/block/") => {
            let height = route
                .trim_start_matches("/block/")
                .parse::<u64>()
                .map_err(|_| "invalid block height")?;
            block_response(
                &ledger,
                ledger
                    .chain
                    .block(&Height(height))
                    .ok_or("block was not found")?,
            )?
        }
        route if route.starts_with("/account/") => {
            let account_route = route.trim_start_matches("/account/");
            let (address, query) = account_route.split_once('?').unwrap_or((account_route, ""));
            if address.is_empty() || address.contains(['/', '#']) {
                return Err("invalid account route".into());
            }
            let mut utxo_offset = 0;
            let mut utxo_after = None;
            if !query.is_empty() {
                let (name, value) = query.split_once('=').ok_or("invalid account query")?;
                if value.is_empty() || value.contains('&') {
                    return Err("invalid account query".into());
                }
                match name {
                    "utxo_offset" => {
                        utxo_offset = value
                            .parse::<usize>()
                            .map_err(|_| "invalid account UTXO offset")?;
                    }
                    "utxo_after" => {
                        utxo_after = Some(
                            value
                                .parse::<kernel::monetary::coin::CoinShare>()
                                .map_err(|_| "invalid account UTXO cursor")?,
                        );
                    }
                    _ => return Err("invalid account query".into()),
                }
            }
            account_response(
                &ledger,
                &read_mempool(database)?,
                parse_address(address)?,
                utxo_offset,
                utxo_after,
            )?
        }
        route if route.starts_with("/explorer/address/") => {
            let value = route.trim_start_matches("/explorer/address/");
            let (address, query) = value.split_once('?').unwrap_or((value, ""));

            if address.is_empty() || address.contains(['/', '#']) {
                return Err("invalid explorer address route".into());
            }

            let mut include_emissions = true;
            let mut limit = DEFAULT_ADDRESS_ACTIVITY_LIMIT;
            let mut before = None;

            let mut seen_include_emissions = false;
            let mut seen_limit = false;
            let mut seen_before = false;

            if !query.is_empty() {
                for parameter in query.split('&') {
                    let (name, value) = parameter
                        .split_once('=')
                        .ok_or("invalid explorer address query")?;

                    match name {
                        "include_emissions" => {
                            if seen_include_emissions {
                                return Err("duplicate include_emissions query".into());
                            }

                            seen_include_emissions = true;

                            include_emissions = match value {
                                "true" => true,
                                "false" => false,
                                _ => return Err("invalid include_emissions query".into()),
                            };
                        }

                        "limit" => {
                            if seen_limit {
                                return Err("duplicate limit query".into());
                            }

                            seen_limit = true;

                            limit = value
                                .parse::<usize>()
                                .map_err(|_| "invalid explorer address limit")?;

                            if limit == 0 || limit > MAX_ADDRESS_ACTIVITY_LIMIT {
                                return Err("explorer address limit is out of range".into());
                            }
                        }

                        "before" => {
                            if seen_before {
                                return Err("duplicate before query".into());
                            }

                            seen_before = true;

                            if value.len() != crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE * 2 {
                                return Err("invalid explorer address cursor".into());
                            }

                            let bytes = hex::decode(value)
                                .map_err(|_| "invalid explorer address cursor")?;

                            let cursor: [u8; crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE] = bytes
                                .try_into()
                                .map_err(|_| "invalid explorer address cursor")?;

                            before = Some(cursor);
                        }

                        _ => return Err("invalid explorer address query".into()),
                    }
                }
            }

            explorer_address_response(
                database,
                &ledger,
                &read_mempool(database)?,
                parse_address(address)?,
                include_emissions,
                limit,
                before,
            )?
        }
        route if route.starts_with("/explorer/transaction/") => {
            let hash = route.trim_start_matches("/explorer/transaction/");
            explorer_transaction_response(database, &ledger, parse_hash(hash)?)?
        }
        _ => return Err("unknown RPC route".into()),
    };
    write_http_response(stream, 200, &response)
}

pub(super) fn read_http_request(stream: &mut impl Read) -> Result<HttpRequest, String> {
    let mut bytes = Vec::new();
    let header_end = loop {
        if bytes.len() >= MAX_RPC_HEADER_SIZE {
            return Err("RPC headers exceed size limit".into());
        }
        let mut chunk = [0_u8; 1024];
        let length = stream
            .read(&mut chunk)
            .map_err(|error| format!("read RPC: {error}"))?;
        if length == 0 {
            return Err("RPC connection closed before headers completed".into());
        }
        bytes.extend_from_slice(&chunk[..length]);
        if let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let headers = std::str::from_utf8(&bytes[..header_end])
        .map_err(|_| "RPC headers are not UTF-8")?
        .to_string();
    let mut content_length = None;
    for line in headers.lines().skip(1) {
        if line.trim().is_empty() {
            continue;
        }
        let Some((name, value)) = line.split_once(':') else {
            return Err("malformed RPC header".into());
        };
        if name.eq_ignore_ascii_case("transfer-encoding") {
            return Err("RPC Transfer-Encoding is not supported".into());
        }
        if name.eq_ignore_ascii_case("content-length") {
            if content_length.is_some() {
                return Err("duplicate RPC Content-Length".into());
            }
            content_length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|_| "invalid RPC Content-Length")?,
            );
        }
    }
    let content_length = content_length.unwrap_or(0);
    if content_length > MAX_STORED_TRANSACTION_SIZE {
        return Err("RPC request body exceeds transaction size limit".into());
    }
    let total = header_end
        .checked_add(content_length)
        .ok_or("RPC request size overflow")?;
    if bytes.len() > total {
        bytes.truncate(total);
    }
    while bytes.len() < total {
        let remaining = total - bytes.len();
        let mut chunk = vec![0_u8; remaining.min(8 * 1024)];
        let length = stream
            .read(&mut chunk)
            .map_err(|error| format!("read RPC body: {error}"))?;
        if length == 0 {
            return Err("RPC body is truncated".into());
        }
        bytes.extend_from_slice(&chunk[..length]);
    }
    Ok(HttpRequest {
        headers,
        body: bytes[header_end..].to_vec(),
    })
}

pub(super) fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    value: &serde_json::Value,
) -> Result<(), String> {
    let body = serde_json::to_vec(value).map_err(|error| error.to_string())?;
    write_http_bytes(stream, status, "application/json", &body)
}

pub(super) fn write_http_bytes(
    stream: &mut TcpStream,
    status: u16,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let reason = match status {
        200 => "OK",
        400 => "Bad Request",
        429 => "Too Many Requests",
        503 => "Service Unavailable",
        _ => "Error",
    };
    write!(
        stream,
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\nX-Content-Type-Options: nosniff\r\n\r\n",
        body.len()
    )
    .and_then(|_| stream.write_all(body))
    .map_err(|error| format!("write RPC: {error}"))
}
