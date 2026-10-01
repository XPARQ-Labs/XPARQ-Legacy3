use super::*;

pub(super) fn option<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.windows(2)
        .find(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
}

pub(super) fn repeated_options<'a>(args: &'a [String], name: &str) -> Vec<&'a str> {
    args.windows(2)
        .filter(|pair| pair[0] == name)
        .map(|pair| pair[1].as_str())
        .collect()
}

pub(super) fn has_flag(args: &[String], name: &str) -> bool {
    args.iter().any(|argument| argument == name)
}

pub(super) fn parse_amount(value: &str) -> Result<Zeno, String> {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > DECIMALS as usize || whole.is_empty() {
        return Err(format!("invalid CoinShare amount `{value}`"));
    }
    let whole = whole
        .parse::<u64>()
        .map_err(|_| format!("invalid CoinShare amount `{value}`"))?;
    let mut fraction_text = fraction.to_string();
    fraction_text.extend(std::iter::repeat_n('0', DECIMALS as usize - fraction.len()));
    let fraction = fraction_text
        .parse::<u64>()
        .map_err(|_| format!("invalid CoinShare amount `{value}`"))?;
    let units = whole
        .checked_mul(CoinShare::ZENO_PER_COIN)
        .and_then(|units| units.checked_add(fraction))
        .ok_or_else(|| "CoinShare amount overflow".to_string())?;
    if units == 0 {
        return Err("CoinShare amount must be positive".to_string());
    }
    Ok(Zeno::from_zeno(units))
}

pub(super) fn format_amount(units: u64) -> String {
    let whole = units / CoinShare::ZENO_PER_COIN;
    let fraction = units % CoinShare::ZENO_PER_COIN;
    let width = DECIMALS as usize;
    format!("{whole}.{fraction:0width$} XPQ")
}

pub(super) fn print_help() {
    println!(
        "wallet [menu]\nwallet new [--wallet PATH] [--words 12|24] [--account account]\nwallet restore --mnemonic PHRASE [--wallet PATH] [--account ACCOUNT]\nwallet address [--wallet PATH]\nwallet balance [--wallet PATH] [--rpc ADDRESS]\nwallet history [--wallet PATH] [--rpc ADDRESS] [--limit 1..=250] [--before CURSOR]\nwallet utxos [--wallet PATH] [--rpc ADDRESS]\nwallet sign-spend [--input COIN_ID...] --to ADDRESS --amount CoinShare [--change CoinShare --change-to ADDRESS] [--rpc ADDRESS] [--wallet PATH] [--offline]\nwallet consolidate [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet version\n\nAll signature accounts are active from genesis. Signed transactions are submitted to node RPC automatically. Use --offline to print canonical transaction hex instead. The wallet automatically calculates the miner fee from canonical transaction bytes; manual --miner fee input is not supported. Consolidation merges selected CoinShare UTXOs into one self-owned output and remains subject to archival burn and miner fee. History reports canonical address activity; UTXO tracker reads the wallet account endpoint and follows paginated UTXOs.\nRunning without a command opens the interactive menu.\nWithout --input, spend selects active CoinShare inputs and calculates change through node RPC."
    );
    println!(
        "\nProgram asset commands:\nwallet program-register --name NAME --max-supply AMOUNT --initial-mint AMOUNT [--fixed-supply] [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet program-mint --asset CONTRACT --to ADDRESS --amount AMOUNT [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet program-transfer --asset CONTRACT --to ADDRESS --amount AMOUNT [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet program-burn --asset CONTRACT --amount AMOUNT [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet program-consolidate --asset CONTRACT [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet program-info --asset CONTRACT [--rpc ADDRESS]\nwallet program-balance --asset CONTRACT [--address ADDRESS | --wallet PATH] [--rpc ADDRESS]\n\nAsset amounts have 8 decimals. XPQ inputs, change, miner fee and protocol burn are calculated automatically. --offline prints a signed transaction but still needs RPC for inputs and fee quote. Wait for confirmation before dependent operations."
    );
}
