use super::cli::print_human_json;
use super::transaction::{
    automatic_fee_transaction, reject_manual_fee, select_account_inputs_with_state_burn,
    submit_or_print_transaction,
};
use super::*;
use kernel::monetary::asset::ASSET_DECIMALS;

pub(super) fn asset_register(args: &[String]) -> Result<(), String> {
    let name = normalize_asset_name(option(args, "--name").ok_or("missing --name")?)?;
    let max_supply = parse_asset_amount(args, "--max-supply")?;
    let initial_mint = parse_asset_amount(args, "--initial-mint")?;
    let authority = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?.address();
    let mint_authority = if has_flag(args, "--fixed-supply") {
        Address::ZERO
    } else {
        authority
    };

    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?
        .as_nanos() as u64;

    let asset = AssetContract::derive(
        &kernel::monetary::asset::Metadata::new(
            name.clone(),
            max_supply,
            authority,
            mint_authority,
        )
        .map_err(|error| error.to_string())?,
        nonce,
    )
    .map_err(|error| error.to_string())?;
    submit_asset_instruction(
        args,
        AssetInstruction::Register {
            name,
            max_supply,
            initial_mint,
            mint_authority,
            nonce,
        },
    )?;
    println!("asset: {asset}");
    Ok(())
}

fn normalize_asset_name(name: &str) -> Result<String, String> {
    let normalized = name.trim().to_string();
    if normalized.is_empty()
        || normalized.len() > kernel::monetary::asset::ASSET_NAME_MAX_LEN
        || !normalized
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(format!(
            "invalid asset name; use 1-{} printable ASCII characters",
            kernel::monetary::asset::ASSET_NAME_MAX_LEN
        ));
    }
    Ok(normalized)
}

pub(super) fn asset_mint(args: &[String]) -> Result<(), String> {
    let asset = parse_asset(args)?;
    let metadata = asset_metadata(args, asset)?;
    let nonce = metadata
        .mint_nonce
        .checked_add(1)
        .ok_or("asset mint nonce is exhausted")?;
    submit_asset_instruction(
        args,
        AssetInstruction::Mint {
            asset,
            nonce,
            recipient: asset_recipient(args)?,
            amount: parse_asset_amount(args, "--amount")?,
        },
    )
}

pub(super) fn asset_burn(args: &[String]) -> Result<(), String> {
    let wallet = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?;
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let asset = parse_asset(args)?;

    let amount = parse_asset_amount(args, "--amount")?;

    let (inputs, total) = select_asset_inputs(rpc, wallet.address(), asset, amount.as_units())?;

    let output = total
        .checked_sub(amount.as_units())
        .ok_or("asset burn exceeds selected shares")?;

    submit_asset_instruction(
        args,
        AssetInstruction::Burn {
            asset,
            inputs,
            amount,
            output: Unit::from_units(output),
        },
    )
}

pub(super) fn asset_transfer(args: &[String]) -> Result<(), String> {
    submit_asset_spend(args, asset_recipient(args)?)
}

pub(super) fn consolidate_asset_shares(args: &[String]) -> Result<(), String> {
    reject_manual_fee(args)?;
    let wallet = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?;
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let asset = parse_asset(args)?;
    let address = kernel::crypto::address_to_string(&wallet.address());
    let balance: BalanceResponse = http_get_json(rpc, &format!("/balance/{address}"))?;
    let mut shares = balance
        .assets
        .into_iter()
        .find(|entry| entry.asset == asset.to_string())
        .ok_or("wallet has no shares for this asset")?
        .shares;
    if shares.len() < 2 {
        return Err("asset consolidation requires at least two shares for this asset".into());
    }
    shares.sort_by(|left, right| {
        let left_amount = left.amount.parse::<u128>().unwrap_or(u128::MAX);
        let right_amount = right.amount.parse::<u128>().unwrap_or(u128::MAX);
        left_amount
            .cmp(&right_amount)
            .then_with(|| left.share_id.cmp(&right.share_id))
    });
    shares.truncate(MAX_CONSOLIDATION_INPUTS);

    let inputs = shares
        .iter()
        .map(|share| {
            share
                .share_id
                .parse::<kernel::monetary::asset::Share>()
                .map_err(|_| "node returned an invalid asset share id".to_string())
        })
        .collect::<Result<Vec<_>, _>>()?;
    let total = shares.iter().try_fold(0_u128, |total, share| {
        let amount = share
            .amount
            .parse::<u128>()
            .map_err(|_| "node returned an invalid asset share amount".to_string())?;
        total
            .checked_add(amount)
            .ok_or_else(|| "asset consolidation amount overflow".to_string())
    })?;
    let output = kernel::monetary::asset::AssetOutput {
        recipient: wallet.address(),
        amount: kernel::monetary::asset::Unit::from_units(total),
    };
    let asset_outputs = vec![output.clone()];
    let asset_weight = kernel::monetary::asset::checked_asset_entry_weight(
        0,
        32,
        &kernel::monetary::asset::AssetShare {
            asset,
            amount: output.amount,
            owner: output.recipient,
        },
    )
    .map_err(|error| format!("calculate asset state weight: {error:?}"))?;

    let transaction = automatic_fee_transaction(|fee, archival_burn| {
        let (coin_inputs, _, _state_burn, change) = select_account_inputs_with_state_burn(
            rpc,
            &wallet,
            fee,
            1,
            asset_weight,
            archival_burn,
        )?;
        let mut fee_outputs = Vec::new();
        if change > 0 {
            fee_outputs.push(CoinOutput::new(wallet.address(), Zeno::from_zeno(change)));
        }
        let intent = SpendIntent::combined_with_charges(
            wallet.address(),
            coin_inputs,
            fee_outputs,
            asset,
            inputs.clone(),
            asset_outputs.clone(),
            SpendCharges::new(Zeno::from_zeno(fee)),
        )
        .map_err(|error| error.to_string())?;
        let spend = wallet.sign_onchain_spend(intent)?;

        Ok(AuthorizedTransaction::Spend(Box::new(spend)))
    })?;
    submit_or_print_transaction(args, &transaction)
}

pub(super) fn asset_recipient(args: &[String]) -> Result<Address, String> {
    address_from_string(option(args, "--to").ok_or("missing --to")?)
        .map_err(|error| error.to_string())
}

fn submit_asset_spend(args: &[String], recipient: Address) -> Result<(), String> {
    reject_manual_fee(args)?;
    let wallet = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?;
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let asset = parse_asset(args)?;
    let amount = parse_asset_amount(args, "--amount")?;
    let coin_recipient = option(args, "--coin-to")
        .map(|value| address_from_string(value).map_err(|error| error.to_string()))
        .transpose()?;
    let coin_amount = option(args, "--coin-amount")
        .map(parse_amount)
        .transpose()?;
    if coin_recipient.is_some() != coin_amount.is_some() {
        return Err("--coin-to and --coin-amount must be provided together".into());
    }
    if coin_amount.is_some_and(|value| value == Zeno::ZERO) {
        return Err("--coin-amount must be positive".into());
    }
    let (inputs, total) = select_asset_inputs(rpc, wallet.address(), asset, amount.as_units())?;
    let mut outputs = vec![kernel::monetary::asset::AssetOutput { recipient, amount }];
    if total > amount.as_units() {
        outputs.push(kernel::monetary::asset::AssetOutput {
            recipient: wallet.address(),
            amount: kernel::monetary::asset::Unit::from_units(total - amount.as_units()),
        });
    }
    let asset_weight = outputs.iter().try_fold(0_u64, |weight, output| {
        kernel::monetary::asset::checked_asset_entry_weight(
            weight,
            32,
            &kernel::monetary::asset::AssetShare {
                asset: asset,
                amount: output.amount,
                owner: output.recipient,
            },
        )
        .map_err(|e| format!("calculate asset state weight: {e:?}"))
    })?;
    let transaction = automatic_fee_transaction(|fee, archival_burn| {
        let coin_transfer = coin_amount.unwrap_or(Zeno::ZERO);
        let required = fee
            .checked_add(coin_transfer.as_zeno())
            .ok_or("coin transfer plus fee overflow")?;
        let (coin_inputs, _, _state_burn, change) = select_account_inputs_with_state_burn(
            rpc,
            &wallet,
            required,
            1 + u64::from(coin_recipient.is_some()),
            asset_weight,
            archival_burn,
        )?;
        let mut fee_outputs = Vec::new();
        if let Some(to) = coin_recipient {
            fee_outputs.push(CoinOutput::new(to, coin_transfer));
        }
        if change > 0 {
            fee_outputs.push(CoinOutput::new(wallet.address(), Zeno::from_zeno(change)));
        }
        let intent = SpendIntent::combined_with_charges(
            wallet.address(),
            coin_inputs,
            fee_outputs,
            asset,
            inputs.clone(),
            outputs.clone(),
            SpendCharges::new(Zeno::from_zeno(fee)),
        )
        .map_err(|error| error.to_string())?;
        let spend = wallet.sign_onchain_spend(intent)?;
        Ok(AuthorizedTransaction::Spend(Box::new(spend)))
    })?;
    submit_or_print_transaction(args, &transaction)
}

fn select_asset_inputs(
    rpc: &str,
    owner: Address,
    asset: AssetContract,
    required: u128,
) -> Result<(Vec<kernel::monetary::asset::Share>, u128), String> {
    let address = kernel::crypto::address_to_string(&owner);
    let balance: BalanceResponse = http_get_json(rpc, &format!("/balance/{address}"))?;
    let entry = balance
        .assets
        .into_iter()
        .find(|entry| entry.asset == asset.to_string())
        .ok_or("wallet has no shares for this asset")?;
    let mut inputs = Vec::new();
    let mut total = 0_u128;
    for share in entry.shares {
        inputs.push(
            share
                .share_id
                .parse()
                .map_err(|_| "node returned an invalid asset share id")?,
        );
        total = total
            .checked_add(
                share
                    .amount
                    .parse::<u128>()
                    .map_err(|_| "node returned an invalid asset share amount")?,
            )
            .ok_or("asset share amount overflow")?;
        if total >= required {
            break;
        }
    }
    if total < required {
        return Err("insufficient asset balance".into());
    }
    Ok((inputs, total))
}

pub(super) fn asset_info(args: &[String]) -> Result<(), String> {
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let response: serde_json::Value =
        http_get_json(rpc, &format!("/asset/{}", parse_asset(args)?))?;
    print_human_json(&response);
    Ok(())
}

pub(super) fn asset_balance(args: &[String]) -> Result<(), String> {
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let address = match option(args, "--address") {
        Some(address) => address_from_string(address).map_err(|error| error.to_string())?,
        None => load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?.address(),
    };
    let response: serde_json::Value = http_get_json(
        rpc,
        &format!(
            "/asset/{}/balance/{}",
            parse_asset(args)?,
            kernel::crypto::address_to_string(&address)
        ),
    )?;
    print_human_json(&response);
    Ok(())
}

fn submit_asset_instruction(args: &[String], instruction: AssetInstruction) -> Result<(), String> {
    reject_manual_fee(args)?;
    let wallet = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?;
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    let call = wallet.0.sign_asset_intent(instruction)?;
    let created_state_weight = call
        .intent
        .created_state_weight()
        .map_err(|error| format!("calculate asset state weight: {error:?}"))?;
    let transaction = automatic_fee_transaction(|fee, archival_burn| {
        let (inputs, _total, _state_burn, change) = select_account_inputs_with_state_burn(
            rpc,
            &wallet,
            fee,
            1,
            created_state_weight,
            archival_burn,
        )?;
        let mut outputs = Vec::new();
        if change > 0 {
            outputs.push(CoinOutput::new(wallet.address(), Zeno::from_zeno(change)));
        }
        let fee_intent = SpendIntent::coin_with_charges(
            wallet.address(),
            inputs,
            outputs,
            SpendCharges::new(Zeno::from_zeno(fee)),
        )
        .map_err(|error| error.to_string())?;
        let fee = wallet.0.sign_asset_call_payment(&call.intent, fee_intent)?;
        Ok(AuthorizedTransaction::Asset(Box::new(
            AuthorizedAssetTransaction {
                call: call.clone(),
                payment: fee,
            },
        )))
    })?;
    submit_or_print_transaction(args, &transaction)
}

fn parse_asset(args: &[String]) -> Result<AssetContract, String> {
    option(args, "--asset")
        .ok_or_else(|| "missing --asset".to_string())?
        .parse::<AssetContract>()
        .map_err(|_| "invalid --asset id".to_string())
}

fn asset_metadata(args: &[String], asset: AssetContract) -> Result<AssetMetadataResponse, String> {
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    http_get_json(rpc, &format!("/asset/{asset}"))
}

fn parse_asset_amount(
    args: &[String],
    option_name: &str,
) -> Result<kernel::monetary::asset::Unit, String> {
    let value = option(args, option_name).ok_or_else(|| format!("missing {option_name}"))?;
    parse_asset_display_amount(value, ASSET_DECIMALS)
        .map(kernel::monetary::asset::Unit::from_units)
        .map_err(|error| format!("invalid {option_name}: {error}"))
}

fn parse_asset_display_amount(value: &str, decimals: u8) -> Result<u128, String> {
    if value.is_empty() || value.starts_with('+') || value.starts_with('-') {
        return Err("use a non-negative decimal amount".into());
    }
    let mut parts = value.split('.');
    let whole = parts.next().unwrap_or_default();
    let fraction = parts.next();
    if parts.next().is_some()
        || whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err("use digits with at most one decimal point".into());
    }
    let fraction = fraction.unwrap_or_default();
    if fraction.len() > decimals as usize || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(format!("at most {decimals} fractional digits are allowed"));
    }
    let scale = 10_u128
        .checked_pow(decimals as u32)
        .ok_or_else(|| "decimal scale overflow".to_string())?;
    let whole = whole
        .parse::<u128>()
        .map_err(|_| "amount exceeds the u128 range".to_string())?;
    let fractional_units = if fraction.is_empty() {
        0
    } else {
        let fraction_value = fraction
            .parse::<u128>()
            .map_err(|_| "invalid fractional amount".to_string())?;
        fraction_value
            .checked_mul(10_u128.pow(decimals as u32 - fraction.len() as u32))
            .ok_or_else(|| "amount exceeds the u128 range".to_string())?
    };
    whole
        .checked_mul(scale)
        .and_then(|units| units.checked_add(fractional_units))
        .ok_or_else(|| "amount exceeds the u128 range".to_string())
}
