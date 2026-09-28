use super::*;

use std::collections::BTreeSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct TxLocation {
    pub(super) height: Height,
    pub(super) transaction_index: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ActivityLocation {
    Emission {
        height: Height,
    },
    Transaction {
        height: Height,
        transaction_index: usize,
    },
}

pub(super) struct AddressActivityPage {
    pub(super) locations: Vec<ActivityLocation>,
    pub(super) next_cursor: Option<[u8; crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE]>,
}

pub(super) fn canonical_block_height(
    path: &Path,
    ledger: &Ledger,
    hash: [u8; 32],
) -> Result<Option<Height>, String> {
    ensure_persistent_indexes(path, ledger)?;

    Ok(crate::storage::read_block_height_by_hash(path, hash)?.map(Height))
}

fn transaction_addresses(
    transaction: &Transaction,
    miner: Address,
) -> Result<BTreeSet<Address>, String> {
    let mut addresses = BTreeSet::new();

    let (sender, outputs) = match transaction {
        AuthorizedTransaction::Spend(transaction) => {
            let coin = &transaction.spend;

            coin.intent
                .coin_parts()
                .ok_or("spend payment is not coin")?;
            (
                coin.intent.signer,
                explorer::coin_outputs_with_charges(&coin.intent, miner),
            )
        }

        AuthorizedTransaction::Asset(transaction) => (
            transaction.payment.intent.signer,
            explorer::coin_outputs_with_charges(&transaction.payment.intent, miner),
        ),
    };

    addresses.insert(sender);

    for output in outputs {
        addresses.insert(explorer::output_recipient(&output));
    }

    Ok(addresses)
}

pub(super) fn stored_address_activities(
    block: &Block,
) -> Result<Vec<crate::storage::StoredAddressActivity>, String> {
    let mut activities = Vec::new();

    if let Some(emission) = block.emission() {
        activities.push(crate::storage::StoredAddressActivity {
            address: emission.to.0,
            transaction_index: None,
        });
    }

    let miner = block.miner_address();

    for (transaction_index, transaction) in block.transactions().iter().enumerate() {
        let transaction_index =
            u64::try_from(transaction_index).map_err(|_| "transaction index exceeds u64")?;

        for address in transaction_addresses(transaction, miner)? {
            activities.push(crate::storage::StoredAddressActivity {
                address: address.0,
                transaction_index: Some(transaction_index),
            });
        }
    }

    Ok(activities)
}

fn ensure_persistent_indexes(path: &Path, ledger: &Ledger) -> Result<(), String> {
    let tip_height = ledger
        .tip_height()
        .ok_or("canonical genesis is missing while checking persistent indexes")?;

    let tip_hash = ledger
        .tip_hash()
        .ok_or("canonical genesis is missing while checking persistent indexes")?
        .0;

    if crate::storage::canonical_index_tip(path)?
        .is_some_and(|(height, hash)| height == tip_height.0 && hash == tip_hash)
    {
        return Ok(());
    }

    let blocks = ledger
        .chain
        .blocks()
        .map(|block| {
            Ok(crate::storage::CanonicalIndexBlock {
                height: block.height().0,

                hash: block.hash().map_err(|error| error.to_string())?.0,

                transactions: block
                    .transactions()
                    .iter()
                    .map(|transaction| transaction.id().map_err(|error| error.to_string()))
                    .collect::<Result<Vec<_>, String>>()?,

                activities: stored_address_activities(block)?,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;

    crate::storage::rebuild_canonical_indexes(path, &blocks)
}

pub(super) fn transaction_location(
    path: &Path,
    ledger: &Ledger,
    hash: [u8; 32],
) -> Result<Option<TxLocation>, String> {
    ensure_persistent_indexes(path, ledger)?;

    let Some((height, transaction_index)) = crate::storage::read_transaction_location(path, hash)?
    else {
        return Ok(None);
    };

    let transaction_index =
        usize::try_from(transaction_index).map_err(|_| "stored transaction index exceeds usize")?;

    Ok(Some(TxLocation {
        height: Height(height),
        transaction_index,
    }))
}

#[cfg(test)]
pub(super) fn address_activity_locations(
    path: &Path,
    ledger: &Ledger,
    address: Address,
) -> Result<Vec<ActivityLocation>, String> {
    ensure_persistent_indexes(path, ledger)?;

    crate::storage::read_address_activities(path, address.0)?
        .into_iter()
        .map(
            |(height, transaction_index)| -> Result<ActivityLocation, String> {
                let height = Height(height);

                match transaction_index {
                    None => Ok(ActivityLocation::Emission { height }),

                    Some(transaction_index) => {
                        let transaction_index =
                            usize::try_from(transaction_index).map_err(|_| {
                                "stored activity transaction index exceeds usize".to_string()
                            })?;

                        Ok(ActivityLocation::Transaction {
                            height,
                            transaction_index,
                        })
                    }
                }
            },
        )
        .collect()
}

pub(super) fn address_activity_page(
    path: &Path,
    ledger: &Ledger,
    address: Address,
    before: Option<[u8; crate::storage::ADDRESS_ACTIVITY_CURSOR_SIZE]>,
    limit: usize,
) -> Result<AddressActivityPage, String> {
    ensure_persistent_indexes(path, ledger)?;

    let page = crate::storage::read_address_activities_page(path, address.0, before, limit)?;

    let locations = page
        .entries
        .into_iter()
        .map(
            |(height, transaction_index)| -> Result<ActivityLocation, String> {
                let height = Height(height);

                match transaction_index {
                    None => Ok(ActivityLocation::Emission { height }),

                    Some(transaction_index) => {
                        let transaction_index =
                            usize::try_from(transaction_index).map_err(|_| {
                                "stored activity transaction index exceeds usize".to_string()
                            })?;

                        Ok(ActivityLocation::Transaction {
                            height,
                            transaction_index,
                        })
                    }
                }
            },
        )
        .collect::<Result<Vec<_>, _>>()?;

    Ok(AddressActivityPage {
        locations,
        next_cursor: page.next_cursor,
    })
}
