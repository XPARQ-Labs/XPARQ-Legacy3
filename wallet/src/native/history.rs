use super::*;

pub(super) fn print_history(args: &[String]) -> Result<(), String> {
    let path = option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH);
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let limit = option(args, "--limit")
        .map(parse_history_limit)
        .transpose()?
        .unwrap_or(DEFAULT_HISTORY_LIMIT);
    let before = option(args, "--before")
        .map(validate_history_cursor)
        .transpose()?;
    let bytes =
        Zeroizing::new(fs::read(path).map_err(|error| format!("failed to read {path}: {error}"))?);
    let address = kernel::crypto::address_to_string(&wallet_address_from_file_bytes(&bytes)?);
    let history = fetch_address_history(rpc, &address, limit, before)?;
    let next_cursor = print_history_page(history);

    if let Some(cursor) = next_cursor {
        println!("Next Cursor: {cursor}");
    }
    Ok(())
}

pub(super) fn parse_history_limit(value: &str) -> Result<usize, String> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| format!("invalid --limit `{value}`; use 1..={MAX_HISTORY_LIMIT}"))?;
    if !(1..=MAX_HISTORY_LIMIT).contains(&limit) {
        return Err(format!(
            "invalid --limit `{value}`; use 1..={MAX_HISTORY_LIMIT}"
        ));
    }
    Ok(limit)
}

pub(super) fn validate_history_cursor(value: &str) -> Result<&str, String> {
    if value.len() != HISTORY_CURSOR_HEX_LEN || !value.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(format!(
            "invalid --before cursor; expected {HISTORY_CURSOR_HEX_LEN} hexadecimal characters"
        ));
    }
    Ok(value)
}

pub(super) fn fetch_address_history(
    rpc: &str,
    address: &str,
    limit: usize,
    before: Option<&str>,
) -> Result<AddressHistoryResponse, String> {
    let mut path = format!("/explorer/address/{address}?include_emissions=false&limit={limit}");
    if let Some(cursor) = before {
        path.push_str("&before=");
        path.push_str(cursor);
    }
    http_get_json(rpc, &path)
}

pub(super) fn print_history_page(mut history: AddressHistoryResponse) -> Option<String> {
    println!("Address: {}", history.address);
    println!("Tip Height: {}", history.tip_height);

    let emission_count = history.emission_count;
    let next_cursor = history.next_cursor.take();
    history
        .activities
        .retain(|activity| activity.hash.is_some());

    println!("Transactions: {}", history.activities.len());
    println!("Emissions Hidden: {emission_count}");
    println!("More: {}", if next_cursor.is_some() { "yes" } else { "no" });

    if history.activities.is_empty() {
        println!("History: no canonical transactions on this page");
    } else {
        for (index, activity) in history.activities.into_iter().enumerate() {
            let confirmations = history
                .tip_height
                .saturating_sub(activity.height)
                .saturating_add(1);

            println!();
            println!("Transaction {}:", index + 1);
            println!("  Tx Hash: {}", activity.hash.as_deref().unwrap_or("-"));
            println!("  Block Hash: {}", activity.block_hash);
            println!("  Height: {}", activity.height);
            println!("  Confirmations: {confirmations}");
            println!("  Direction: {}", activity.direction);
            println!("  Type: {}", activity.activity_type);
            println!("  Amount: {}", format_amount(activity.amount));
            println!("  Size: {} bytes", activity.size_bytes.unwrap_or(0));
            if let Some(program) = activity.program {
                println!(
                    "  Asset Operation: {}",
                    program["operation"].as_str().unwrap_or("unknown")
                );
                println!(
                    "  Program Asset: {}",
                    program["asset"].as_str().unwrap_or("-")
                );
                let amount = program["amount"].as_str().unwrap_or("0");
                let display = super::cli::format_asset_amount(
                    amount,
                    extension::asset_program::asset::ASSET_DECIMALS,
                )
                .unwrap_or_else(|_| "invalid amount".into());
                println!("  Asset Amount: {display}");
            }
        }
    }

    next_cursor
}
