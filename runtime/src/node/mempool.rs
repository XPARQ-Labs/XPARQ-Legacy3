use super::*;
use super::{config::*, gossip::*, state::*};

pub(super) fn submit_transaction(path: Option<&str>, encoded: &str) -> Result<(), String> {
    let database = database_path(path);
    let bytes =
        hex::decode(encoded).map_err(|error| format!("invalid transaction hex: {error}"))?;
    if bytes.is_empty() || bytes.len() > MAX_STORED_TRANSACTION_SIZE {
        return Err("transaction size is outside allowed range".into());
    }
    let transaction: Transaction =
        canonical_decode(&bytes).map_err(|error| format!("invalid transaction: {error}"))?;
    let hash = insert_mempool_transaction(&database, transaction, false)?;
    println!("accepted transaction={}", hex::encode(hash));
    Ok(())
}

pub(super) fn print_mempool(path: Option<&str>) -> Result<(), String> {
    let database = database_path(path);
    let transactions = read_mempool(&database)?;
    println!("transactions: {}", transactions.len());
    for transaction in transactions {
        println!(
            "{}",
            hex::encode(transaction.id().map_err(|error| error.to_string())?)
        );
    }
    Ok(())
}

pub(super) fn accept_relayed_transaction(database: &Path, bytes: &[u8]) -> Result<(), String> {
    if bytes.is_empty() || bytes.len() > MAX_STORED_TRANSACTION_SIZE {
        return Err("relayed transaction size is outside allowed range".into());
    }
    let transaction: Transaction =
        canonical_decode(bytes).map_err(|error| format!("invalid relayed transaction: {error}"))?;
    insert_mempool_transaction(database, transaction, true).map(|_| ())
}

pub(super) fn insert_mempool_transaction(
    database: &Path,
    transaction: Transaction,
    duplicate_is_ok: bool,
) -> Result<[u8; 32], String> {
    let encoded = canonical_bytes(&transaction).map_err(|error| error.to_string())?;
    if encoded.len() > MAX_STORED_TRANSACTION_SIZE {
        return Err("transaction exceeds consensus size limit".into());
    }
    transaction
        .validate_structure()
        .map_err(|error| error.to_string())?;
    let _mutation = state_mutation_lock()?
        .lock()
        .map_err(|_| "state mutation lock is poisoned")?;
    let hash = transaction.id().map_err(|error| error.to_string())?;
    let ledger = load_or_initialize(database)?;
    let mut transactions = read_mempool(database)?;
    if transactions
        .iter()
        .any(|existing| existing.id().ok() == Some(hash))
    {
        return if duplicate_is_ok {
            Ok(hash)
        } else {
            Err("transaction is already in mempool".into())
        };
    }
    transactions.push(transaction);
    validate_mempool(&ledger, &transactions)?;
    write_mempool(database, &transactions)?;
    notify_gossip();
    Ok(hash)
}

pub(super) fn reconcile_mempool(
    ledger: &Ledger,
    transactions: Vec<Transaction>,
    included: &BTreeSet<[u8; 32]>,
) -> Vec<Transaction> {
    let height = Height(
        ledger
            .tip_height()
            .map_or(0, |height| height.0.saturating_add(1)),
    );
    let Ok(chain) = kernel::genesis::chain_context() else {
        return Vec::new();
    };
    let mut state = ledger.state().clone();
    let mut retained = Vec::new();
    let mut seen = BTreeSet::new();
    for transaction in transactions {
        if retained.len() >= MAX_MEMPOOL_TRANSACTIONS {
            break;
        }
        let Ok(hash) = transaction.id() else {
            continue;
        };
        if included.contains(&hash) || !seen.insert(hash) {
            continue;
        }
        let Ok(encoded) = canonical_bytes(&transaction) else {
            continue;
        };
        if encoded.len() > MAX_STORED_TRANSACTION_SIZE {
            continue;
        }
        if !meets_minimum_relay_fee(&transaction, encoded.len()) {
            continue;
        }
        let Ok(validated) = validate_transaction(transaction.clone(), chain, height.0, &state)
        else {
            continue;
        };
        if state
            .apply_validated_transaction(&validated, Address::ZERO, chain)
            .is_err()
        {
            continue;
        }
        retained.push(transaction);
    }
    retained
}

pub(super) fn reserved_coin_inputs(
    transactions: &[Transaction],
) -> BTreeSet<kernel::monetary::coin::CoinShare> {
    transactions
        .iter()
        .flat_map(|transaction| match transaction {
            AuthorizedTransaction::Spend(transaction) => transaction
                .intent
                .coin_parts()
                .map_or_else(Vec::new, |(inputs, _)| inputs.to_vec()),
            AuthorizedTransaction::Program(transaction) => transaction
                .payment
                .coin_parts()
                .map_or_else(Vec::new, |(inputs, _)| inputs.to_vec()),
        })
        .collect()
}

pub(super) fn validate_mempool(
    ledger: &Ledger,
    transactions: &[Transaction],
) -> Result<(), String> {
    if transactions.len() > MAX_MEMPOOL_TRANSACTIONS {
        return Err("mempool transaction count exceeds limit".into());
    }
    let height = Height(
        ledger
            .tip_height()
            .map_or(0, |height| height.0.saturating_add(1)),
    );
    let chain = kernel::genesis::chain_context().map_err(|error| error.to_string())?;
    let mut state = ledger.state().clone();
    for transaction in transactions {
        let encoded = canonical_bytes(transaction).map_err(|error| error.to_string())?;
        if encoded.len() > MAX_STORED_TRANSACTION_SIZE {
            return Err("transaction cannot fit in a block".into());
        }
        let required_fee = minimum_relay_fee(encoded.len())?;
        let paid_fee = transaction_miner_fee(transaction)?;
        if paid_fee < required_fee {
            return Err(format!(
                "mempool transaction fee is too low: paid {paid_fee} zeno, required {required_fee} zeno for {} bytes",
                encoded.len()
            ));
        }
        let validated = validate_transaction(transaction.clone(), chain, height.0, &state)
            .map_err(|error| format!("mempool transaction is invalid: {error}"))?;
        state
            .apply_validated_transaction(&validated, Address::ZERO, chain)
            .map_err(|error| format!("mempool state transition is invalid: {error}"))?;
    }
    Ok(())
}

pub(super) fn minimum_relay_fee(encoded_size: usize) -> Result<u64, String> {
    u64::try_from(encoded_size)
        .ok()
        .and_then(|size| size.checked_mul(MIN_RELAY_FEE_ZENO_PER_BYTE))
        .ok_or("minimum relay fee overflow".into())
}

pub(super) fn meets_minimum_relay_fee(transaction: &Transaction, encoded_size: usize) -> bool {
    minimum_relay_fee(encoded_size)
        .and_then(|required| transaction_miner_fee(transaction).map(|paid| paid >= required))
        .unwrap_or(false)
}

pub(super) fn transaction_miner_fee(transaction: &Transaction) -> Result<u64, String> {
    match transaction {
        AuthorizedTransaction::Spend(transaction) => {
            Ok(transaction.intent.charges.miner_fee.as_zeno())
        }
        AuthorizedTransaction::Program(transaction) => {
            Ok(transaction.payment.charges.miner_fee.as_zeno())
        }
    }
}

pub(super) fn read_mempool(path: &Path) -> Result<Vec<Transaction>, String> {
    let encoded = crate::storage::read_mempool(path)?;
    let length = encoded
        .iter()
        .try_fold(0_u64, |total, transaction| {
            total.checked_add(transaction.len() as u64)
        })
        .ok_or("stored mempool size overflow")?;
    if length > MAX_STORED_MEMPOOL_SIZE {
        return Err("stored mempool exceeds size limit".into());
    }
    if encoded.len() > MAX_MEMPOOL_TRANSACTIONS {
        return Err("stored mempool transaction count exceeds limit".into());
    }
    encoded
        .into_iter()
        .map(|bytes| {
            if bytes.is_empty() || bytes.len() > MAX_STORED_TRANSACTION_SIZE {
                return Err("stored transaction size is outside allowed range".into());
            }
            canonical_decode(&bytes).map_err(|error| format!("decode mempool transaction: {error}"))
        })
        .collect()
}

pub(super) fn write_mempool(path: &Path, transactions: &[Transaction]) -> Result<(), String> {
    let encoded = encode_mempool(transactions)?;
    let length = encoded
        .iter()
        .try_fold(0_u64, |total, transaction| {
            total.checked_add(transaction.len() as u64)
        })
        .ok_or("mempool size overflow")?;
    if length > MAX_STORED_MEMPOOL_SIZE {
        return Err("mempool exceeds persistence size limit".into());
    }
    crate::storage::replace_mempool(path, &encoded)
}

pub(super) fn encode_mempool(transactions: &[Transaction]) -> Result<Vec<Vec<u8>>, String> {
    transactions
        .iter()
        .map(|transaction| canonical_bytes(transaction).map_err(|error| error.to_string()))
        .collect()
}

pub(super) fn persist_block_and_mempool(
    path: &Path,
    block: &Block,
    mempool: &[Transaction],
) -> Result<(), String> {
    let stored = crate::storage::StoredCanonicalBlock {
        height: block.height().0,

        hash: block.hash().map_err(|error| error.to_string())?.0,

        bytes: block_bytes(block).map_err(|error| error.to_string())?,

        transactions: block
            .transactions()
            .iter()
            .map(|transaction| transaction.id().map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?,

        activities: super::index::stored_address_activities(block)?,
    };

    crate::storage::append_block_and_replace_mempool(path, &stored, &encode_mempool(mempool)?)
}

pub(super) fn persist_chain_and_mempool(
    path: &Path,
    ledger: &Ledger,
    mempool: &[Transaction],
) -> Result<(), String> {
    let blocks = ledger
        .chain
        .blocks()
        .map(|block| {
            Ok(crate::storage::StoredCanonicalBlock {
                height: block.height().0,

                hash: block.hash().map_err(|error| error.to_string())?.0,

                bytes: block_bytes(block).map_err(|error| error.to_string())?,

                transactions: block
                    .transactions()
                    .iter()
                    .map(|transaction| transaction.id().map_err(|error| error.to_string()))
                    .collect::<Result<Vec<_>, String>>()?,

                activities: super::index::stored_address_activities(block)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    crate::storage::replace_blocks_and_mempool(path, &blocks, &encode_mempool(mempool)?)
}
