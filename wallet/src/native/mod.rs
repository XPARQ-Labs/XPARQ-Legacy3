use std::{
    fs,
    io::{self, Write},
    path::Path,
    str::FromStr,
};

use kernel::monetary::asset::{AssetContract, Unit};
use kernel::monetary::coin::{CoinOutput, CoinShare, Zeno};
use kernel::{
    codec::canonical_bytes,
    consensus::{DECIMALS, StateTransitionWeight},
    crypto::{Address, Signature, address_from_string},
    transaction::{AssetInstruction, AuthorizedTransaction, SpendCharges, SpendIntent},
};
use serde::Deserialize;
use wallet::{
    AccountWallet, account_wallet_file_bytes, account_wallet_from_bip39_mnemonic,
    account_wallet_from_file_bytes, generate_bip39_mnemonic, wallet_address_from_file_bytes,
};
use zeroize::{Zeroize, Zeroizing};

const DEFAULT_WALLET_PATH: &str = "wallet.json";
const AUTOMATIC_FEE_ZENO_PER_BYTE: u64 = 8;
const MAX_FEE_CONVERGENCE_ROUNDS: usize = 8;
const DEFAULT_HISTORY_LIMIT: usize = 50;
const MAX_HISTORY_LIMIT: usize = 250;
const HISTORY_CURSOR_HEX_LEN: usize = 34;

struct LoadedWallet(AccountWallet);

impl LoadedWallet {
    fn address(&self) -> Address {
        self.0.address
    }

    fn sign_onchain_spend(
        &self,
        intent: SpendIntent,
    ) -> Result<kernel::transaction::AuthorizedAccountIntent<SpendIntent>, String> {
        self.0.sign_account_intent(intent)
    }
}
#[cfg(feature = "mainnet")]
const DEFAULT_RPC_ADDR: &str = "127.0.0.1:6666";
#[cfg(feature = "testnet")]
const DEFAULT_RPC_ADDR: &str = "127.0.0.1:16666";
#[cfg(feature = "devnet")]
const DEFAULT_RPC_ADDR: &str = "127.0.0.1:26666";

#[derive(Deserialize)]
struct AccountResponse {
    next_height: u64,
    #[serde(rename = "utxo_snapshot")]
    _utxo_snapshot: String,
    utxos: Vec<AccountUtxo>,
    next_utxo_offset: Option<usize>,
    next_utxo_cursor: Option<String>,
}

#[derive(Deserialize)]
struct BalanceResponse {
    total: u64,
    reserved: u64,
    utxo_count: usize,
    #[serde(default)]
    assets: Vec<AccountAssetBalance>,
}

#[derive(Deserialize)]
struct NodeBurnResponse {
    total_mined: u64,
    total_burned: u64,
    supply: u64,
}

#[derive(Deserialize)]
struct AccountAssetBalance {
    asset: String,
    name: String,
    max_supply: String,
    mint: String,
    #[serde(default)]
    shares: Vec<AccountAssetShare>,
}

#[derive(Deserialize)]
struct AccountAssetShare {
    share_id: String,
    amount: String,
    owner: serde_json::Value,
}

#[derive(Deserialize)]
struct AssetMetadataResponse {
    mint_nonce: u64,
}

#[derive(Deserialize)]
struct AccountUtxo {
    id: String,
    amount: u64,
    reserved: bool,
}

#[derive(Deserialize)]
struct AddressHistoryResponse {
    address: String,
    tip_height: u64,
    emission_count: usize,
    activities: Vec<AddressActivity>,
    #[serde(default)]
    next_cursor: Option<String>,
}

#[derive(Deserialize)]
struct AddressActivity {
    height: u64,
    block_hash: String,
    hash: Option<String>,
    #[serde(rename = "type")]
    activity_type: String,
    direction: String,
    amount: u64,
    size_bytes: Option<usize>,
}

#[derive(Deserialize)]
struct SubmitTransactionResponse {
    hash: String,
}

const MAX_CONSOLIDATION_INPUTS: usize = 1_000;

pub fn run(mut args: Vec<String>) -> Result<(), String> {
    let result = match args.first().map(String::as_str) {
        None | Some("menu") | Some("interactive") => interactive_menu(),
        Some("new") => create_wallet(&args[1..]),
        Some("restore") => restore_wallet(&args[1..]),
        Some("address") => print_address(&args[1..]),
        Some("balance") => print_balance(&args[1..]),
        Some("history") => print_history(&args[1..]),
        Some("utxos") | Some("utxo-tracker") => print_utxo_tracker(&args[1..]),
        Some("sign-spend") => sign_spend(&args[1..]),
        Some("consolidate") => consolidate_coin_utxos(&args[1..]),
        Some("asset-consolidate") => consolidate_asset_shares(&args[1..]),
        Some("asset-register") => asset_register(&args[1..]),
        Some("asset-mint") => asset_mint(&args[1..]),
        Some("asset-burn") => asset_burn(&args[1..]),
        Some("asset-transfer") => asset_transfer(&args[1..]),
        Some("asset-info") => asset_info(&args[1..]),
        Some("asset-balance") => asset_balance(&args[1..]),
        Some("version") | Some("--version") | Some("-V") => {
            println!("wallet {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some(command) => Err(format!("unknown command `{command}`")),
    };
    args.zeroize();
    result
}

mod asset;
mod balance;
mod cli;
mod history;
mod rpc;
mod transaction;
mod util;
mod utxo;
mod wallet_file;

use asset::{
    asset_balance, asset_burn, asset_info, asset_mint, asset_register, asset_transfer,
    consolidate_asset_shares,
};

use wallet_file::{load_wallet, write_account_wallet};

use cli::{create_wallet, interactive_menu, print_address, restore_wallet};
use util::{format_amount, has_flag, option, parse_amount, print_help, repeated_options};

use balance::print_balance;
use history::print_history;
use rpc::{http_get_json, http_post_bytes};
use transaction::{consolidate_coin_utxos, sign_spend};
use utxo::print_utxo_tracker;

#[cfg(test)]
use history::{parse_history_limit, validate_history_cursor};

#[cfg(test)]
mod tests;
