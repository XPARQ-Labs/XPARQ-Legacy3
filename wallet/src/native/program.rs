use super::transaction::{
    automatic_fee_transaction, reject_manual_fee, select_account_inputs_with_state_burn,
    submit_or_print_transaction,
};
use super::*;
use extension::{
    asset_program::{
        asset::{ASSET_DECIMALS, AssetContract, AssetOutput, Metadata, Share, Unit},
        opcode::AssetOpcode,
        type_::{Burn, Mint, Register, Transfer},
    },
    script::call::{ProgramCall, ProgramId},
};

fn amount(args: &[String], name: &str) -> Result<Unit, String> {
    parse_asset_display_amount(
        option(args, name).ok_or_else(|| format!("missing {name}"))?,
        ASSET_DECIMALS,
    )
    .map(Unit::from_units)
}
fn asset(args: &[String]) -> Result<AssetContract, String> {
    option(args, "--asset")
        .ok_or("missing --asset")?
        .parse()
        .map_err(|_| "invalid program asset id".into())
}
fn encode<T: borsh::BorshSerialize>(opcode: AssetOpcode, value: &T) -> Result<ProgramCall, String> {
    Ok(ProgramCall {
        program: ProgramId::ASSET,
        opcode: opcode as u8,
        payload: borsh::to_vec(value).map_err(|e| e.to_string())?,
    })
}
fn recipient(args: &[String]) -> Result<Address, String> {
    address_from_string(option(args, "--to").ok_or("missing --to")?).map_err(|e| e.to_string())
}
fn shares(
    rpc: &str,
    owner: Address,
    asset: AssetContract,
    required: Option<Unit>,
) -> Result<(Vec<Share>, Unit), String> {
    let address = kernel::crypto::address_to_string(&owner);
    let response: serde_json::Value =
        http_get_json(rpc, &format!("/program/asset/{asset}/balance/{address}"))?;
    let mut inputs = Vec::new();
    let mut total = Unit::ZERO;
    for share in response["shares"]
        .as_array()
        .ok_or("invalid program shares response")?
    {
        if inputs.len() == 256 {
            break;
        }
        inputs.push(
            share["share_id"]
                .as_str()
                .ok_or("invalid program share id")?
                .parse()
                .map_err(|_| "invalid program share id")?,
        );
        let value = share["amount"]
            .as_str()
            .ok_or("invalid program share amount")?
            .parse::<u128>()
            .map_err(|_| "invalid program share amount")?;
        total = total
            .checked_add(Unit::from_units(value))
            .ok_or("program amount overflow")?;
        if required.is_some_and(|r| total >= r) {
            break;
        }
    }
    if inputs.is_empty() || required.is_some_and(|r| total < r) {
        return Err("insufficient program asset balance within 256 shares; consolidate or wait for pending transactions to confirm".into());
    }
    Ok((inputs, total))
}

fn submit(args: &[String], wallet: &LoadedWallet, call: ProgramCall) -> Result<(), String> {
    reject_manual_fee(args)?;
    extension::script::execute::decode_program(&call)
        .map_err(|e| format!("invalid ProgramCall: {e:?}"))?;
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    // Quote state growth using a signed envelope; the quote performs no mutation.
    let (inputs, _, _, change) = select_account_inputs_with_state_burn(rpc, wallet, 1, 1, 0, 0)?;
    let outputs = if change > 0 {
        vec![CoinOutput::new(wallet.address(), Zeno::from_zeno(change))]
    } else {
        vec![]
    };
    let payment = CoinTransition::coin_with_charges(
        wallet.address(),
        inputs,
        outputs,
        CoinCharges::new(Zeno::ONE),
    )
    .map_err(|e| e.to_string())?;
    let dummy = AuthorizedTransaction::Program(Box::new(
        wallet.0.sign_program_call(call.clone(), payment)?,
    ));
    let quote: serde_json::Value = http_post_bytes(
        rpc,
        "/program/quote",
        &canonical_bytes(&dummy).map_err(|e| e.to_string())?,
    )?;
    let growth = quote["created_state_weight"]
        .as_u64()
        .ok_or("invalid program quote response")?;
    let transaction = automatic_fee_transaction(|fee, archival| {
        let (inputs, _, _, change) =
            select_account_inputs_with_state_burn(rpc, wallet, fee, 1, growth, archival)?;
        let outputs = if change > 0 {
            vec![CoinOutput::new(wallet.address(), Zeno::from_zeno(change))]
        } else {
            vec![]
        };
        let payment = CoinTransition::coin_with_charges(
            wallet.address(),
            inputs,
            outputs,
            CoinCharges::new(Zeno::from_zeno(fee)),
        )
        .map_err(|e| e.to_string())?;
        Ok(AuthorizedTransaction::Program(Box::new(
            wallet.0.sign_program_call(call.clone(), payment)?,
        )))
    })?;
    submit_or_print_transaction(args, &transaction)
}

pub(super) fn command(command: &str, args: &[String]) -> Result<(), String> {
    let rpc = option(args, "--rpc").unwrap_or(DEFAULT_RPC_ADDR);
    if command == "program-info" {
        let value: serde_json::Value =
            http_get_json(rpc, &format!("/program/asset/{}", asset(args)?))?;
        super::cli::print_human_json(&value);
        return Ok(());
    }
    if command == "program-balance" {
        let owner = match option(args, "--address") {
            Some(v) => address_from_string(v).map_err(|e| e.to_string())?,
            None => load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?.address(),
        };
        let value: serde_json::Value = http_get_json(
            rpc,
            &format!(
                "/program/asset/{}/balance/{}",
                asset(args)?,
                kernel::crypto::address_to_string(&owner)
            ),
        )?;
        super::cli::print_human_json(&value);
        return Ok(());
    }
    if !matches!(
        command,
        "program-register"
            | "program-mint"
            | "program-transfer"
            | "program-burn"
            | "program-consolidate"
    ) {
        return Err(format!("unknown program command {command}"));
    }
    let wallet = load_wallet(option(args, "--wallet").unwrap_or(DEFAULT_WALLET_PATH))?;
    let call = match command {
        "program-register" => {
            let name = normalize_asset_name(option(args, "--name").ok_or("missing --name")?)?;
            let max_supply = amount(args, "--max-supply")?;
            let initial_mint = amount(args, "--initial-mint")?;
            let authority = if has_flag(args, "--fixed-supply") {
                Address::ZERO
            } else {
                wallet.address()
            };
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| e.to_string())?
                .as_nanos() as u64;
            let metadata = Metadata::new(name.clone(), max_supply, wallet.address(), authority)
                .map_err(|e| e.to_string())?;
            let id = AssetContract::derive(&metadata, nonce).map_err(|e| e.to_string())?;
            let call = encode(
                AssetOpcode::Register,
                &Register {
                    name,
                    max_supply,
                    initial_mint,
                    mint_authority: authority,
                    nonce,
                },
            )?;
            submit(args, &wallet, call)?;
            println!("program asset: {id}");
            return Ok(());
        }
        "program-mint" => {
            let id = asset(args)?;
            let info: serde_json::Value = http_get_json(rpc, &format!("/program/asset/{id}"))?;
            let nonce = info["mint_nonce"]
                .as_u64()
                .ok_or("invalid mint nonce")?
                .checked_add(1)
                .ok_or("mint nonce exhausted")?;
            encode(
                AssetOpcode::Mint,
                &Mint {
                    asset: id,
                    nonce,
                    recipient: recipient(args)?,
                    amount: amount(args, "--amount")?,
                },
            )?
        }
        "program-transfer" | "program-consolidate" => {
            let id = asset(args)?;
            let required = if command == "program-consolidate" {
                None
            } else {
                Some(amount(args, "--amount")?)
            };
            let (inputs, total) = shares(rpc, wallet.address(), id, required)?;
            if command == "program-consolidate" && inputs.len() < 2 {
                return Err("program consolidation requires at least two shares".into());
            }
            let sent = required.unwrap_or(total);
            let to = if command == "program-consolidate" {
                wallet.address()
            } else {
                recipient(args)?
            };
            let mut outputs = vec![AssetOutput::new(to, sent)];
            let change = total
                .checked_sub(sent)
                .ok_or("insufficient program balance")?;
            if !change.is_zero() {
                outputs.push(AssetOutput::new(wallet.address(), change));
            }
            encode(
                AssetOpcode::Transfer,
                &Transfer {
                    asset: id,
                    inputs,
                    outputs,
                },
            )?
        }
        "program-burn" => {
            let id = asset(args)?;
            let burn = amount(args, "--amount")?;
            let (inputs, total) = shares(rpc, wallet.address(), id, Some(burn))?;
            encode(
                AssetOpcode::Burn,
                &Burn {
                    asset: id,
                    inputs,
                    amount: burn,
                    output: total
                        .checked_sub(burn)
                        .ok_or("insufficient program balance")?,
                },
            )?
        }
        _ => unreachable!(),
    };
    submit(args, &wallet, call)
}

pub(super) fn normalize_asset_name(name: &str) -> Result<String, String> {
    let normalized = name.trim().to_string();
    if normalized.is_empty()
        || normalized.len() > extension::asset_program::asset::ASSET_NAME_MAX_LEN
        || !normalized
            .bytes()
            .all(|byte| byte == b' ' || byte.is_ascii_graphic())
    {
        return Err(format!(
            "invalid asset name; use 1-{} printable ASCII characters",
            extension::asset_program::asset::ASSET_NAME_MAX_LEN
        ));
    }
    Ok(normalized)
}

pub(super) fn parse_asset_display_amount(value: &str, decimals: u8) -> Result<u128, String> {
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
