use super::cli::format_asset_amount;
use super::*;
use extension::asset_program::asset::ASSET_DECIMALS;

pub(super) fn print_balance(args: &[String]) -> Result<(), String> {
    let path = option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH);
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let bytes =
        Zeroizing::new(fs::read(path).map_err(|error| format!("failed to read {path}: {error}"))?);
    let address = kernel::crypto::address_to_string(&wallet_address_from_file_bytes(&bytes)?);
    let balance: BalanceResponse = http_get_json(rpc, &format!("/balance/{address}"))?;
    let burn: NodeBurnResponse = http_get_json(rpc, "/status")?;

    println!("Address: {address}");
    println!("Available: {}", format_amount(balance.total));
    println!("Reserved: {}", format_amount(balance.reserved));
    println!("UTXOs: {}", balance.utxo_count);
    println!("Total Mined: {}", format_amount(burn.total_mined));
    println!("Total Burned: {}", format_amount(burn.total_burned));
    println!("Supply: {}", format_amount(burn.supply));
    println!("Program assets: {}", balance.program_assets.len());
    for asset in &balance.program_assets {
        let owned = asset.shares.iter().try_fold(0_u128, |sum, s| {
            let amount = s
                .amount
                .parse::<u128>()
                .map_err(|_| "invalid program amount")?;
            sum.checked_add(amount).ok_or("program balance overflow")
        })?;
        println!(
            "  {} ({}) — {}",
            asset.name,
            asset.asset,
            format_asset_amount(&owned.to_string(), ASSET_DECIMALS)?
        );
    }
    Ok(())
}
