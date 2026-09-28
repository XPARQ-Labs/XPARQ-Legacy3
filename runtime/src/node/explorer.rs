use super::*;
use super::{config::*, mempool::*, state::*, util::*};

pub(super) const DEFAULT_ADDRESS_ACTIVITY_LIMIT: usize = 50;
pub(super) const MAX_ADDRESS_ACTIVITY_LIMIT: usize = 250;

pub(super) fn print_account(path: Option<&str>, address: &str) -> Result<(), String> {
    let database = database_path(path);
    let ledger = load_or_initialize(&database)?;
    let address = parse_address(address)?;
    let response = account_response(&ledger, &read_mempool(&database)?, address, 0, None)?;
    println!(
        "{}",
        serde_json::to_string_pretty(&response).map_err(|error| error.to_string())?
    );
    Ok(())
}

pub(super) fn account_response(
    ledger: &Ledger,
    mempool: &[Transaction],
    address: Address,
    utxo_offset: usize,
    utxo_after: Option<kernel::monetary::coin::CoinShare>,
) -> Result<serde_json::Value, String> {
    let next_height = ledger
        .tip_height()
        .map_or(0, |height| height.0.saturating_add(1));
    let reserved = reserved_coin_inputs(mempool);
    let mut total = Zeno::from_zeno(0);
    let mut account_utxos = ledger
        .state()
        .utxos
        .coins()
        .filter(|(_, coin)| coin.owner == address)
        .collect::<Vec<_>>();
    account_utxos.sort_by_key(|(id, _)| *id);
    for (_, coin) in &account_utxos {
        total = total
            .checked_add(coin.amount)
            .ok_or("account balance overflow")?;
    }
    let page_start = utxo_after.map_or(utxo_offset, |cursor| {
        account_utxos.partition_point(|(id, _)| *id <= cursor)
    });
    let utxos = account_utxos
        .iter()
        .skip(page_start)
        .take(MAX_ACCOUNT_UTXOS_PER_PAGE)
        .map(|(id, coin)| {
            let is_reserved = reserved.contains(id);
            serde_json::json!({
                "id": id.to_string(),
                "amount": coin.amount.as_zeno(),
                "reserved": is_reserved,
            })
        })
        .collect::<Vec<_>>();
    let next_utxo_offset = page_start
        .checked_add(utxos.len())
        .filter(|offset| *offset < account_utxos.len());
    let next_utxo_cursor = next_utxo_offset
        .and_then(|_| account_utxos.get(page_start + utxos.len().saturating_sub(1)))
        .map(|(id, _)| id.to_string());
    let utxo_snapshot_entries = account_utxos
        .iter()
        .map(|(id, coin)| (*id, coin.amount, reserved.contains(id)))
        .collect::<Vec<_>>();
    let utxo_snapshot_bytes = kernel::crypto::canonical_bytes(&(address, utxo_snapshot_entries))
        .map_err(|error| format!("encode account UTXO snapshot: {error}"))?;
    let utxo_snapshot = kernel::crypto::domain_hash(
        kernel::crypto::HashDomain::AccountState,
        &utxo_snapshot_bytes,
    );
    let assets = account_asset_balances(ledger, address)?;
    Ok(serde_json::json!({
        "address": kernel::crypto::address_to_string(&address),
        "utxo_snapshot": hex::encode(utxo_snapshot.0),
        "tip_height": ledger.tip_height().map_or(0, |height| height.0),
        "next_height": next_height,
        "total": total.as_zeno(),
        "assets": assets,
        "utxos": utxos,
        "next_utxo_offset": next_utxo_offset,
        "next_utxo_cursor": next_utxo_cursor,
    }))
}

pub(super) fn balance_response(
    ledger: &Ledger,
    mempool: &[Transaction],
    address: Address,
) -> Result<serde_json::Value, String> {
    let reserved_ids = reserved_coin_inputs(mempool);
    let mut total = Zeno::from_zeno(0);
    let mut reserved = Zeno::from_zeno(0);
    let mut utxo_count = 0_usize;
    for utxo in ledger
        .state()
        .utxos
        .coins()
        .filter(|(_, coin)| coin.owner == address)
    {
        total = total
            .checked_add(utxo.1.amount)
            .ok_or("account balance overflow")?;
        if reserved_ids.contains(&utxo.0) {
            reserved = reserved
                .checked_add(utxo.1.amount)
                .ok_or("reserved account balance overflow")?;
        }
        utxo_count = utxo_count
            .checked_add(1)
            .ok_or("account UTXO count overflow")?;
    }
    let available = total
        .checked_sub(reserved)
        .ok_or("reserved account balance exceeds total")?;
    Ok(serde_json::json!({
        "address": kernel::crypto::address_to_string(&address),
        "tip_height": ledger.tip_height().map_or(0, |height| height.0),
        "total": total.as_zeno(),
        "available": available.as_zeno(),
        "reserved": reserved.as_zeno(),
        "utxo_count": utxo_count,
        "assets": account_asset_balances(ledger, address)?,
    }))
}

pub(super) fn account_asset_balances(
    ledger: &Ledger,
    address: Address,
) -> Result<Vec<serde_json::Value>, String> {
    let mut assets = std::collections::BTreeSet::new();
    for (asset, metadata) in ledger.state().assets.metadata_entries() {
        if metadata.creator == address || metadata.mint_authority == address {
            assets.insert(asset);
        }
    }
    for (_, utxo) in ledger.state().utxos.assets() {
        if utxo.owner != address || utxo.amount.is_zero() {
            continue;
        }
        assets.insert(utxo.asset);
    }
    let mut response = Vec::with_capacity(assets.len());
    for asset in assets {
        let metadata = ledger
            .state()
            .assets
            .metadata(asset)
            .ok_or("asset balance references missing metadata")?;
        let mint = ledger.state().assets.supply(asset);
        let shares = account_asset_shares(ledger, asset, address);
        response.push(serde_json::json!({
            "asset": asset.to_string(),
            "name": metadata.name,
            "max_supply": metadata.max_supply.to_string(),
            "mint": mint.to_string(),
            "shares": shares,
        }));
    }
    Ok(response)
}

pub(super) fn account_asset_shares(
    ledger: &Ledger,
    asset: kernel::monetary::asset::AssetContract,
    address: Address,
) -> Vec<serde_json::Value> {
    ledger
        .state()
        .utxos
        .assets()
        .filter(|(_, share)| share.asset == asset && share.owner == address)
        .map(|(share_id, share)| {
            serde_json::json!({
                "share_id": share_id.to_string(),
                "amount": share.amount.to_string(),
                "owner": asset_owner_response(address),
            })
        })
        .collect()
}

pub(super) fn explorer_address_response(
    database: &Path,
    ledger: &Ledger,
    mempool: &[Transaction],
    address: Address,
    include_emissions: bool,
    limit: usize,
    before: Option<[u8; crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE]>,
) -> Result<serde_json::Value, String> {
    let reserved_ids = reserved_coin_inputs(mempool);
    let mut total = Zeno::from_zeno(0);
    let mut reserved = Zeno::from_zeno(0);
    for utxo in ledger
        .state()
        .utxos
        .coins()
        .filter(|(_, coin)| coin.owner == address)
    {
        total = total
            .checked_add(utxo.1.amount)
            .ok_or("explorer balance overflow")?;
        if reserved_ids.contains(&utxo.0) {
            reserved = reserved
                .checked_add(utxo.1.amount)
                .ok_or("explorer reserved balance overflow")?;
        }
    }
    let page = index::address_activity_page(database, ledger, address, before, limit)?;
    let mut activities = Vec::new();
    let mut emission_count = 0usize;
    for location in &page.locations {
        match *location {
            index::ActivityLocation::Emission { height } => {
                emission_count = emission_count.saturating_add(1);
                if !include_emissions {
                    continue;
                }

                let block = ledger
                    .chain
                    .block(&height)
                    .ok_or("indexed emission block is missing from the canonical chain")?;

                let emission = block
                    .emission()
                    .filter(|emission| emission.to == address)
                    .ok_or("indexed emission does not match the canonical chain")?;

                let protocol_burn = kernel::consensus::MINER_PROTOCOL_BURN;

                let miner_emission = emission
                    .subsidy
                    .checked_sub(protocol_burn)
                    .ok_or("block emission is below its protocol burn")?;

                activities.push(serde_json::json!({
                    "height": block.height().0,
                    "block_hash": hex::encode(
                        block.hash().map_err(|error| error.to_string())?.0
                    ),
                    "hash": serde_json::Value::Null,
                    "type": "emission",
                    "direction": "in",
                    "amount": miner_emission.as_zeno(),
                    "gross_subsidy": emission.subsidy.as_zeno(),
                    "protocol_burn": protocol_burn.as_zeno(),
                    "size_bytes": serde_json::Value::Null,
                }));
            }

            index::ActivityLocation::Transaction {
                height,
                transaction_index,
            } => {
                let block = ledger
                    .chain
                    .block(&height)
                    .ok_or("indexed activity block is missing from the canonical chain")?;

                let transaction = block
                    .transactions()
                    .get(transaction_index)
                    .ok_or("indexed activity transaction is missing from its block")?;

                if let Some(activity) = address_transaction_activity(transaction, address, block)? {
                    activities.push(activity);
                }
            }
        }
    }

    let next_cursor = page.next_cursor.map(hex::encode);

    Ok(serde_json::json!({
        "address": kernel::crypto::address_to_string(&address),
        "tip_height": ledger.tip_height().map_or(0, |height| height.0),
        "balance": {
            "total": total.as_zeno(),
            "reserved": reserved.as_zeno(),
        },
        "activity_count": activities.len(),
        "emission_count": emission_count,
        "activities": activities,
        "next_cursor": next_cursor,
    }))
}

pub(super) fn address_transaction_activity(
    transaction: &Transaction,
    address: Address,
    block: &Block,
) -> Result<Option<serde_json::Value>, String> {
    let authorized = transaction;
    let miner = block.miner_address();
    let (sender, outputs, extra_sent) = match authorized {
        AuthorizedTransaction::Spend(tx) => {
            let coin = &tx.intent;
            coin.coin_parts().ok_or("spend payment is not coin")?;
            (
                Some(coin.signer),
                coin_outputs_with_charges(&coin, miner),
                Zeno::ZERO,
            )
        }
        AuthorizedTransaction::Asset(tx) => (
            Some(tx.payment.intent.signer),
            coin_outputs_with_charges(&tx.payment.intent, miner),
            coin_burn(&tx.payment.intent),
        ),
    };
    let received = checked_output_sum(
        outputs
            .iter()
            .filter(|output| output_recipient(output) == address)
            .map(|output| output.amount),
    )?;
    let (direction, amount) = if sender == Some(address) {
        let external = checked_output_sum(
            outputs
                .iter()
                .filter(|output| output_recipient(output) != address)
                .map(|output| output.amount),
        )?
        .checked_add(extra_sent)
        .ok_or("explorer transaction amount overflow")?;
        (
            if external.as_zeno() == 0 {
                "self"
            } else {
                "out"
            },
            external,
        )
    } else if received.as_zeno() > 0 {
        ("in", received)
    } else {
        return Ok(None);
    };
    Ok(Some(serde_json::json!({
        "height": block.height().0,
        "block_hash": hex::encode(block.hash().map_err(|error| error.to_string())?.0),
        "hash": hex::encode(transaction.id().map_err(|error| error.to_string())?),
        "type": transaction_kind(transaction),
        "direction": direction,
        "amount": amount.as_zeno(),
        "size_bytes": canonical_bytes(transaction).map_err(|error| error.to_string())?.len(),
    })))
}

pub(super) fn explorer_transaction_response(
    database: &Path,
    ledger: &Ledger,
    hash: [u8; 32],
) -> Result<serde_json::Value, String> {
    let tip_height = ledger.tip_height().map_or(0, |height| height.0);

    let location = index::transaction_location(database, ledger, hash)?
        .ok_or("transaction was not found in the canonical chain")?;

    let block = ledger
        .chain
        .block(&location.height)
        .ok_or("indexed transaction block is missing from the canonical chain")?;

    let transaction = block
        .transactions()
        .get(location.transaction_index)
        .ok_or("indexed transaction position is missing from its block")?;

    if transaction.id().map_err(|error| error.to_string())? != hash {
        return Err("transaction index does not match the canonical chain".into());
    }

    let burns = ledger
        .transaction_protocol_burns(location.height)
        .ok_or("transaction execution receipts are missing")?;

    let protocol_burn = burns
        .get(location.transaction_index)
        .copied()
        .ok_or("transaction execution receipt is missing")?;

    Ok(serde_json::json!({
        "hash": hex::encode(hash),
        "type": transaction_kind(transaction),
        "status": "confirmed",
        "height": block.height().0,
        "block_hash": hex::encode(
            block.hash().map_err(|error| error.to_string())?.0
        ),
        "confirmations": tip_height
            .saturating_sub(block.height().0)
            .saturating_add(1),
        "size_bytes": canonical_bytes(transaction)
            .map_err(|error| error.to_string())?
            .len(),
        "transaction": transaction_response(
            transaction,
            protocol_burn,
        ),
    }))
}

pub(super) fn transaction_response(
    transaction: &Transaction,
    protocol_burn: Zeno,
) -> serde_json::Value {
    match transaction {
        AuthorizedTransaction::Spend(spend) => spend_transaction_response(spend, protocol_burn),
        AuthorizedTransaction::Asset(asset) => asset_transaction_response(asset, protocol_burn),
    }
}

pub(super) fn coin_outputs(intent: &kernel::transaction::SpendIntent) -> &[CoinOutput] {
    intent.coin_parts().map_or(&[], |(_, outputs)| outputs)
}

pub(super) fn coin_outputs_with_charges(
    intent: &kernel::transaction::SpendIntent,
    miner: Address,
) -> Vec<CoinOutput> {
    let mut outputs = coin_outputs(intent).to_vec();
    if !intent.charges.miner_fee.is_zero() {
        outputs.push(CoinOutput::new(miner, intent.charges.miner_fee));
    }
    outputs
}

pub(super) fn coin_burn(_intent: &kernel::transaction::SpendIntent) -> Zeno {
    Zeno::ZERO
}

pub(super) fn spend_transaction_response(
    transaction: &kernel::transaction::AuthorizedAccountIntent<kernel::transaction::SpendIntent>,
    protocol_burn: Zeno,
) -> serde_json::Value {
    match &transaction.intent.spend {
        kernel::transaction::Spend::Coin { inputs, outputs } => serde_json::json!({
            "type": "coin", "signer": kernel::crypto::address_to_string(&transaction.intent.signer),
            "inputs": inputs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "outputs": public_outputs_response(&outputs, Some(transaction.intent.signer)),
            "miner_fee": transaction.intent.charges.miner_fee.as_zeno(),
            "protocol_burn": protocol_burn.as_zeno(),
        }),
        kernel::transaction::Spend::Combined {
            coin_inputs,
            coin_outputs,
            asset,
            asset_inputs,
            asset_outputs,
        } => serde_json::json!({
            "type": "combined", "asset": asset.to_string(),
            "signer": kernel::crypto::address_to_string(&transaction.intent.signer),
            "coin_inputs": coin_inputs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "coin_outputs": public_outputs_response(&coin_outputs, Some(transaction.intent.signer)),
            "asset_inputs": asset_inputs.iter().map(ToString::to_string).collect::<Vec<_>>(),
            "asset_outputs": asset_outputs.iter().map(|output| serde_json::json!({
                "owner": asset_owner_response(output.recipient), "amount": output.amount.to_string(),
            })).collect::<Vec<_>>(),
            "miner_fee": transaction.intent.charges.miner_fee.as_zeno(),
            "protocol_burn": protocol_burn.as_zeno(),
        }),
    }
}

pub(super) fn asset_transaction_response(
    transaction: &kernel::transaction::AuthorizedAssetTransaction,
    protocol_burn: Zeno,
) -> serde_json::Value {
    let call = &transaction.call.intent;
    let instruction = match &call.instruction {
        kernel::transaction::AssetInstruction::Register {
            name,
            max_supply,
            initial_mint,
            mint_authority,
            nonce,
        } => serde_json::json!({
            "type": "register", "name": name,
            "max_supply": max_supply.to_string(), "initial_mint": initial_mint.to_string(),
            "mint_authority": asset_authority_response(*mint_authority),
            "recipient": kernel::crypto::address_to_string(&call.signer),
            "nonce": nonce,
        }),
        kernel::transaction::AssetInstruction::Mint {
            recipient, amount, ..
        } => serde_json::json!({
            "type": "mint", "recipient": asset_owner_response(*recipient), "amount": amount.to_string(),
        }),
        kernel::transaction::AssetInstruction::Burn { inputs, .. } => {
            serde_json::json!({ "type": "burn", "inputs": inputs.iter().map(ToString::to_string).collect::<Vec<_>>() })
        }
    };
    serde_json::json!({
        "asset": call.asset().map(|id| id.to_string()).unwrap_or_else(|_| "invalid".into()),
        "signer": kernel::crypto::address_to_string(&call.signer),
        "asset_instruction": instruction,
        "payment_sender": kernel::crypto::address_to_string(&transaction.payment.intent.signer),
        "payment_outputs": public_outputs_response(&coin_outputs(&transaction.payment.intent), Some(transaction.payment.intent.signer)),
        "miner_fee": transaction.payment.intent.charges.miner_fee.as_zeno(),
        "protocol_burn": protocol_burn.as_zeno(),
    })
}

pub(super) fn asset_owner_response(owner: Address) -> serde_json::Value {
    serde_json::json!({
        "type": "account",
        "address": kernel::crypto::address_to_string(&owner),
    })
}

pub(super) fn public_outputs_response(
    outputs: &[CoinOutput],
    sender: Option<Address>,
) -> Vec<serde_json::Value> {
    outputs
        .iter()
        .map(|output| {
            let address = output.output;
            let role = if sender == Some(address) {
                "change"
            } else {
                "recipient"
            };
            serde_json::json!({
                "address": kernel::crypto::address_to_string(&address),
                "amount": output.amount.as_zeno(),
                "unit": "zeno",
                "type": "address",
                "role": role,
            })
        })
        .collect()
}

pub(super) fn output_recipient(output: &CoinOutput) -> Address {
    output.output
}

pub(super) fn checked_output_sum(amounts: impl IntoIterator<Item = Zeno>) -> Result<Zeno, String> {
    amounts
        .into_iter()
        .try_fold(Zeno::from_zeno(0), |total, amount| {
            total
                .checked_add(amount)
                .ok_or_else(|| "explorer amount overflow".to_string())
        })
}

pub(super) fn transaction_kind(transaction: &Transaction) -> &'static str {
    match transaction {
        AuthorizedTransaction::Spend(spend) => match &spend.intent.spend {
            kernel::transaction::Spend::Coin { .. } => "transfer",
            kernel::transaction::Spend::Combined { .. } => "asset-transfer",
        },
        AuthorizedTransaction::Asset(_) => "asset",
    }
}

pub(super) fn asset_response(ledger: &Ledger, route: &str) -> Result<serde_json::Value, String> {
    let path = route.trim_start_matches("/asset/");
    let parts = path.split('/').collect::<Vec<_>>();
    let asset = parts
        .first()
        .ok_or("missing asset id")?
        .parse::<kernel::monetary::asset::AssetContract>()
        .map_err(|_| "invalid asset id")?;
    if parts.len() == 1 {
        let metadata = ledger
            .state()
            .assets
            .metadata(asset)
            .ok_or("asset was not found")?;
        let supply = ledger.state().assets.supply(asset);
        let mint_nonce = ledger.state().assets.mint_nonce(asset);
        let total_minted = ledger.state().assets.total_minted(asset);
        return Ok(serde_json::json!({
            "asset": asset.to_string(),
            "name": metadata.name,
            "max_supply": metadata.max_supply.to_string(),
            "supply": supply.to_string(),
            "total_minted": total_minted.map(|amount| amount.to_string()),
            "creator": kernel::crypto::address_to_string(&metadata.creator),
            "mint_authority": asset_authority_response(metadata.mint_authority),
            "mint_nonce": mint_nonce,
        }));
    }
    if parts.len() == 3 && parts[1] == "balance" {
        let address = parse_address(parts[2])?;
        let balance = ledger
            .state()
            .utxos
            .assets()
            .filter(|(_, share)| share.asset == asset && share.owner == address)
            .try_fold(kernel::monetary::asset::Unit::ZERO, |total, (_, share)| {
                total
                    .checked_add(share.amount)
                    .ok_or("asset balance overflow")
            })?;
        return Ok(serde_json::json!({
            "asset": asset.to_string(),
            "address": kernel::crypto::address_to_string(&address),
            "balance": balance.to_string(),
            "shares": account_asset_shares(ledger, asset, address),
        }));
    }
    Err("invalid asset route".into())
}

pub(super) fn asset_authority_response(authority: Address) -> serde_json::Value {
    match authority {
        Address::ZERO => serde_json::Value::Null,
        address => serde_json::json!({
            "type": "account",
            "address": kernel::crypto::address_to_string(&address),
        }),
    }
}

pub(super) fn status_response(
    ledger: &Ledger,
    cumulative_work: Work,
    cumulative_weight: u64,
) -> Result<serde_json::Value, String> {
    let tip_height = ledger.tip_height().ok_or("canonical genesis is missing")?;
    let tip_hash = ledger.tip_hash().ok_or("canonical genesis is missing")?;
    let next_difficulty =
        expected_next_difficulty(&ledger.chain).map_err(|error| error.to_string())?;

    Ok(serde_json::json!({
        "tip_height": tip_height.0,
        "next_height": tip_height.0.saturating_add(1),
        "tip_hash": hex::encode(tip_hash.0),
        "next_difficulty": next_difficulty,
        "cumulative_work": format_work(cumulative_work.to_be_limbs()),
        "cumulative_weight": cumulative_weight.to_string(),
        "total_mined": ledger.state().coin.total_mined.as_zeno(),
        "total_burned": ledger.state().coin.total_burned.as_zeno(),
        "supply": ledger
              .state()
              .coin
              .supply()
              .map(|value| value.as_zeno())
              .unwrap_or(0),
    }))
}

pub(super) fn latest_blocks_response(ledger: &Ledger) -> Result<serde_json::Value, String> {
    let blocks = ledger
        .chain
        .blocks()
        .rev()
        .take(20)
        .map(|block| block_response(ledger, block))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({ "blocks": blocks }))
}

pub(super) fn block_response(ledger: &Ledger, block: &Block) -> Result<serde_json::Value, String> {
    let gross_subsidy = block
        .emission()
        .map_or(Zeno::from_zeno(0), |emission| emission.subsidy);
    let state_burn = if block.emission().is_some() {
        kernel::consensus::MINER_PROTOCOL_BURN
    } else {
        Zeno::from_zeno(0)
    };
    let miner_emission = gross_subsidy
        .checked_sub(state_burn)
        .ok_or("block emission is below its created-state burn")?;
    let burns = ledger
        .transaction_protocol_burns(block.height())
        .ok_or("transaction execution receipts are missing")?;
    let transaction_details = block
        .transactions()
        .iter()
        .zip(burns)
        .map(|(transaction, protocol_burn)| {
            Ok(serde_json::json!({
                "hash": hex::encode(
                    transaction.id().map_err(|error| error.to_string())?
                ),
                "type": transaction_kind(transaction),
                "size_bytes": canonical_bytes(transaction)
                    .map_err(|error| error.to_string())?
                    .len(),
                "transaction": transaction_response(
                    transaction,
                    protocol_burn,
                ),
            }))
        })
        .collect::<Result<Vec<_>, String>>()?;
    let hash = transaction_details
        .iter()
        .filter_map(|transaction| transaction.get("hash").cloned())
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "height": block.height().0,
        "hash": hex::encode(block.hash().map_err(|error| error.to_string())?.0),
        "previous_hash": hex::encode(block.previous_hash().0),
        "difficulty": block.target_bits(),
        "block_weight": block.block_weight(),
        "nonce": block.header.nonce.0,
        "transactions": block.transaction_count(),
        "hash": hash,
        "transaction_details": transaction_details,
        "miner": kernel::crypto::address_to_string(&block.miner_address()),
        "subsidy": gross_subsidy.as_zeno(),
        "state_burn": state_burn.as_zeno(),
        "miner_emission": miner_emission.as_zeno(),
    }))
}
