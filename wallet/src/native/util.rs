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
    format!("{whole}.{fraction:0width$} CoinShare")
}

pub(super) fn print_help() {
    println!(
        "wallet [menu]\nwallet new [--wallet PATH] [--words 12|24] [--account account]\nwallet restore --mnemonic PHRASE [--wallet PATH] [--account ACCOUNT]\nwallet address [--wallet PATH]\nwallet balance [--wallet PATH] [--rpc ADDRESS]\nwallet history [--wallet PATH] [--rpc ADDRESS] [--limit 1..=250] [--before CURSOR]\nwallet utxos [--wallet PATH] [--rpc ADDRESS]\nwallet sign-spend [--input COIN_ID...] --to ADDRESS --amount CoinShare [--change CoinShare --change-to ADDRESS] [--rpc ADDRESS] [--wallet PATH] [--offline]\nwallet consolidate [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet version\n\nAll signature accounts are active from genesis. Signed transactions are submitted to node RPC automatically. Use --offline to print canonical transaction hex instead. The wallet automatically pays the node policy fee of 1 zeno per canonical transaction byte; manual --miner fee input is not supported. Consolidation merges selected CoinShare UTXOs into one self-owned output and remains subject to archival burn and miner fee. History reports canonical address activity; UTXO tracker reads the wallet account endpoint and follows paginated UTXOs.\nRunning without a command opens the interactive menu.\nWithout --input, spend selects active CoinShare inputs and calculates change through node RPC."
    );
    println!(
        "\nAsset commands:\nwallet asset-register --name NAME --max-supply AMOUNT --initial-mint AMOUNT [--fixed-supply] [--wallet PATH] [--rpc ADDRESS]\nwallet asset-mint --asset CONTRACT --to ADDRESS --amount AMOUNT [--wallet PATH] [--rpc ADDRESS]\nwallet asset-burn --asset CONTRACT --amount AMOUNT [--wallet PATH] [--rpc ADDRESS]\nwallet asset-transfer --asset CONTRACT --to ADDRESS --amount AMOUNT [--coin-to ADDRESS --coin-amount XPQ] [--wallet PATH] [--rpc ADDRESS]\nwallet asset-consolidate --asset CONTRACT [--wallet PATH] [--rpc ADDRESS] [--offline]\nwallet asset-info --asset CONTRACT [--rpc ADDRESS]\nwallet asset-balance --asset CONTRACT [--address ADDRESS | --wallet PATH] [--rpc ADDRESS]\n\nAsset amounts use the human decimal denomination declared by asset metadata. For decimals=8, 1.25 is encoded canonically as 125000000 Unit. Asset transfer can send XPQ to --coin-to in the same signed transaction; the same wallet must own both asset and XPQ inputs. Asset consolidation merges shares of one contract into one self-owned share and pays its fee and protocol burn with CoinShare. Registration atomically credits the initial mint to the signing creator address."
    );
}
