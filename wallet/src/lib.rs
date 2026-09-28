use bip39::{Language, Mnemonic};

use kernel::{
    crypto::{
        Address, PublicKey, Signature, SigningSeed, address_from_public_key, address_from_string,
        address_to_string, hash_bytes,
    },
    transaction::{
        AccountAuthorization, AccountIntent, AssetIntent, AuthorizedAccountIntent, SpendIntent,
        payment_commitment,
    },
};

use serde::{Deserialize, Serialize};

use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

pub const BIP39_MNEMONIC_DEFAULT_WORDS: usize = 12;

pub const BIP39_MNEMONIC_12_ENTROPY_BYTES: usize = 16;

pub const BIP39_MNEMONIC_24_ENTROPY_BYTES: usize = 32;

#[derive(Debug)]

pub struct AccountWallet {
    pub mnemonic: Option<String>,

    pub address: Address,

    pub public_key: PublicKey,

    signing_seed: SigningSeed,
}

impl Drop for AccountWallet {
    fn drop(&mut self) {
        self.mnemonic.zeroize();
    }
}

#[derive(Deserialize, Serialize, Zeroize, ZeroizeOnDrop)]
#[serde(deny_unknown_fields)]

struct WalletFile {
    address: String,

    mnemonic: String,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    signature_account: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    public_key: Option<String>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    private_key: Option<String>,
}

#[derive(Deserialize)]

struct WalletHeader {
    address: String,
}

pub fn wallet_address_from_file_bytes(bytes: &[u8]) -> Result<Address, String> {
    let header: WalletHeader = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse wallet: {error}"))?;

    address_from_string(&header.address).map_err(|error| format!("invalid wallet address: {error}"))
}

pub fn account_wallet_file_bytes(wallet: &AccountWallet) -> Result<Zeroizing<Vec<u8>>, String> {
    let mnemonic = wallet
        .mnemonic
        .as_deref()
        .ok_or_else(|| "wallet has no mnemonic recovery material".to_string())?;

    decode_bip39_mnemonic(mnemonic)?;

    let wallet_file = WalletFile {
        address: address_to_string(&wallet.address),

        mnemonic: mnemonic.to_string(),

        signature_account: Some(wallet.account().as_str().to_string()),

        public_key: Some(hex::encode(&wallet.public_key.bytes)),

        private_key: Some({
            let seed = wallet.signing_seed.dangerous_export_seed();

            hex::encode(&seed[..])
        }),
    };

    serde_json::to_vec_pretty(&wallet_file)
        .map(Zeroizing::new)
        .map_err(|error| format!("failed to encode wallet file: {error}"))
}

pub fn account_wallet_from_file_bytes(bytes: &[u8]) -> Result<AccountWallet, String> {
    let wallet_file: WalletFile = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse wallet: {error}"))?;

    let account = wallet_file
        .signature_account
        .as_deref()
        .ok_or("wallet file does not contain a signature account")?
        .parse::<Signature>()
        .map_err(str::to_string)?;

    let mut wallet = account_wallet_from_bip39_mnemonic(&wallet_file.mnemonic, account)?;

    let stored_address = address_from_string(&wallet_file.address)
        .map_err(|error| format!("invalid wallet address: {error}"))?;

    if wallet.address != stored_address {
        return Err("wallet address does not match its mnemonic and signature account".to_string());
    }

    if let Some(public_key) = wallet_file.public_key.as_deref()
        && public_key != hex::encode(&wallet.public_key.bytes)
    {
        return Err("wallet public key does not match its mnemonic and signature account".into());
    }

    if let Some(private_key) = wallet_file.private_key.as_deref() {
        let expected_private_key = {
            let seed = wallet.signing_seed.dangerous_export_seed();

            Zeroizing::new(hex::encode(&seed[..]))
        };

        if private_key != expected_private_key.as_str() {
            return Err(
                "wallet private key does not match its mnemonic and signature account".into(),
            );
        }
    }

    wallet.mnemonic = Some(wallet_file.mnemonic.clone());

    Ok(wallet)
}

pub fn wallet_file_signature_account(bytes: &[u8]) -> Result<Option<Signature>, String> {
    let wallet_file: WalletFile = serde_json::from_slice(bytes)
        .map_err(|error| format!("failed to parse wallet: {error}"))?;

    wallet_file
        .signature_account
        .as_deref()
        .map(|account| account.parse::<Signature>().map_err(str::to_string))
        .transpose()
}

pub fn generate_bip39_mnemonic(words: usize) -> Result<Zeroizing<String>, String> {
    let entropy_len = match words {
        12 => BIP39_MNEMONIC_12_ENTROPY_BYTES,

        24 => BIP39_MNEMONIC_24_ENTROPY_BYTES,

        _ => return Err("mnemonic words must be 12 or 24".to_string()),
    };

    let mut entropy = Zeroizing::new(vec![0_u8; entropy_len]);

    getrandom::fill(&mut entropy)
        .map_err(|error| format!("secure random generation failed: {error}"))?;

    encode_bip39_mnemonic(&entropy).map(Zeroizing::new)
}

pub fn account_wallet_from_bip39_mnemonic(
    phrase: &str,

    account: Signature,
) -> Result<AccountWallet, String> {
    let entropy = decode_bip39_mnemonic(phrase)?;

    let mut tag = Vec::from(b"XPARQ_WALLET_SIGNATURE_ACCOUNT".as_slice());

    tag.push(account as u8);

    let seed = tagged_wallet_hash(&tag, &entropy);

    let mut boxed_seed = Box::new([0_u8; 32]);

    boxed_seed.copy_from_slice(seed.as_ref());

    let signing_seed = SigningSeed::new(account, boxed_seed);

    let public_key = signing_seed.public_key();

    Ok(AccountWallet {
        mnemonic: None,

        address: address_from_public_key(&public_key),

        public_key,

        signing_seed,
    })
}

pub fn encode_bip39_mnemonic(entropy: &[u8]) -> Result<String, String> {
    Mnemonic::from_entropy_in(Language::English, entropy)
        .map(|mnemonic| mnemonic.to_string())
        .map_err(|error| format!("failed to encode mnemonic: {error}"))
}

pub fn decode_bip39_mnemonic(phrase: &str) -> Result<Zeroizing<Vec<u8>>, String> {
    let normalized = Zeroizing::new(
        phrase
            .split_whitespace()
            .map(str::to_ascii_lowercase)
            .collect::<Vec<_>>()
            .join(" "),
    );

    let word_count = normalized.split_whitespace().count();

    if !matches!(word_count, 12 | 24) {
        return Err("invalid bip39 mnemonic: expected 12 or 24 words".to_string());
    }

    Mnemonic::parse_in_normalized(Language::English, &normalized)
        .map(|mnemonic| Zeroizing::new(mnemonic.to_entropy()))
        .map_err(|error| format!("invalid bip39 mnemonic: {error}"))
}

fn tagged_wallet_hash(tag: &[u8], bytes: &[u8]) -> Zeroizing<[u8; 32]> {
    let mut payload = Zeroizing::new(Vec::with_capacity(tag.len() + bytes.len()));

    payload.extend_from_slice(tag);

    payload.extend_from_slice(bytes);

    Zeroizing::new(hash_bytes(&payload).0)
}

impl AccountWallet {
    pub const fn account(&self) -> Signature {
        self.signing_seed.account()
    }

    pub fn sign_account_intent<T: AccountIntent>(
        &self,

        intent: T,
    ) -> Result<AuthorizedAccountIntent<T>, String> {
        let chain = kernel::genesis::chain_context().map_err(|error| error.to_string())?;

        let commitment = intent
            .principal_commitment(chain)
            .map_err(|error| error.to_string())?;

        let signature = self.signing_seed.sign(commitment.as_bytes());

        let authorization = AccountAuthorization {
            public_key: self.public_key.clone(),

            signature,
        };

        Ok(AuthorizedAccountIntent {
            intent,

            authorization,
        })
    }

    pub fn sign_asset_intent(
        &self,

        action: kernel::transaction::AssetInstruction,
    ) -> Result<AuthorizedAccountIntent<kernel::transaction::AssetIntent>, String> {
        self.sign_account_intent(kernel::transaction::AssetIntent::new(action, self.address))
    }

    pub fn sign_asset_call_payment(
        &self,

        parent: &AssetIntent,

        payment: SpendIntent,
    ) -> Result<AuthorizedAccountIntent<SpendIntent>, String> {
        let chain = kernel::genesis::chain_context().map_err(|error| error.to_string())?;

        let commitment =
            payment_commitment(parent, &payment, chain).map_err(|error| error.to_string())?;

        let signature = self.signing_seed.sign(commitment.as_bytes());

        Ok(AuthorizedAccountIntent {
            intent: payment,

            authorization: AccountAuthorization {
                public_key: self.public_key.clone(),

                signature,
            },
        })
    }
}

#[cfg(test)]

mod tests {

    use super::*;

    /* Legacy wallet tests removed with the account-only chain reset.

    #[test]

    fn wallet_file_roundtrip_preserves_signing_identity() {

        let mnemonic = encode_bip39_mnemonic(&[7; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        let mut wallet = wallet_from_bip39_mnemonic(&mnemonic).unwrap();

        wallet.mnemonic = Some(mnemonic.clone());

        let encoded = wallet_file_bytes(&wallet).unwrap();

        let decoded = wallet_from_file_bytes(&encoded).unwrap();

        assert_eq!(decoded.address, wallet.address);

        assert_eq!(decoded.public_key, wallet.public_key);

        let json: serde_json::Value = serde_json::from_slice(&encoded).unwrap();

        assert_eq!(json.as_object().unwrap().len(), 2);

        assert_eq!(json.get("mnemonic").unwrap(), &mnemonic);

        assert_eq!(

            json.get("address").unwrap().as_str(),

            Some(wallet_address_string(&wallet).as_str())

        );

        assert!(json.get("secret_key").is_none());

        assert!(encoded.len() < 512);

    }

    #[test]

    fn wallet_address_reader_accepts_legacy_version_field() {

        let mnemonic = encode_bip39_mnemonic(&[8; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        let wallet = wallet_from_bip39_mnemonic(&mnemonic).unwrap();

        let encoded = serde_json::to_vec(&serde_json::json!({

            "version": 1,

            "address": wallet_address_string(&wallet),

            "mnemonic": mnemonic,

        }))

        .unwrap();

        assert_eq!(wallet_address_from_file_bytes(&encoded), Ok(wallet.address));

    }

    #[test]

    fn mnemonic_restore_preserves_signing_identity() {

        let mnemonic = encode_bip39_mnemonic(&[9; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        let mut first = wallet_from_bip39_mnemonic(&mnemonic).unwrap();

        first.mnemonic = Some(mnemonic.clone());

        let first_file = wallet_file_bytes(&first).unwrap();

        let mut restored = wallet_from_bip39_mnemonic(&mnemonic).unwrap();

        restored.mnemonic = Some(mnemonic);

        let restored_file = wallet_file_bytes(&restored).unwrap();

        assert_eq!(first.address, restored.address);

        assert_eq!(first.public_key, restored.public_key);

        assert_eq!(

            wallet_from_file_bytes(&first_file).unwrap().address,

            wallet_from_file_bytes(&restored_file).unwrap().address

        );

    }

    */

    #[test]

    fn mnemonic_derives_distinct_recoverable_account_addresses() {
        let mnemonic = encode_bip39_mnemonic(&[12; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        let accounts = [Signature::MlDsa44, Signature::MlDsa65, Signature::MlDsa87];

        let first =
            accounts.map(|account| account_wallet_from_bip39_mnemonic(&mnemonic, account).unwrap());

        let second =
            accounts.map(|account| account_wallet_from_bip39_mnemonic(&mnemonic, account).unwrap());

        for (left, right) in first.iter().zip(&second) {
            assert_eq!(left.address, right.address);
        }

        let unique = first
            .iter()
            .map(|wallet| wallet.address)
            .collect::<std::collections::BTreeSet<_>>();

        assert_eq!(unique.len(), accounts.len());
    }

    #[test]

    fn account_wallet_file_roundtrip_preserves_account_and_identity() {
        let mnemonic = encode_bip39_mnemonic(&[13; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        for account in [Signature::MlDsa44, Signature::MlDsa65, Signature::MlDsa87] {
            let mut wallet = account_wallet_from_bip39_mnemonic(&mnemonic, account).unwrap();

            wallet.mnemonic = Some(mnemonic.clone());

            let bytes = account_wallet_file_bytes(&wallet).unwrap();

            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

            assert_eq!(json["public_key"], hex::encode(&wallet.public_key.bytes));

            let expected_private_key = {
                let seed = wallet.signing_seed.dangerous_export_seed();

                Zeroizing::new(hex::encode(&seed[..]))
            };

            assert_eq!(
                json["private_key"].as_str(),
                Some(expected_private_key.as_str())
            );

            assert_eq!(
                wallet_file_signature_account(&bytes).unwrap(),
                Some(account)
            );

            let restored = account_wallet_from_file_bytes(&bytes).unwrap();

            assert_eq!(restored.account(), account);

            assert_eq!(restored.address, wallet.address);

            assert_eq!(restored.public_key, wallet.public_key);
        }
    }

    #[test]

    fn account_wallet_file_rejects_keys_that_do_not_match_recovery_material() {
        let mnemonic = encode_bip39_mnemonic(&[14; BIP39_MNEMONIC_12_ENTROPY_BYTES]).unwrap();

        let mut wallet = account_wallet_from_bip39_mnemonic(&mnemonic, Signature::MlDsa44).unwrap();

        wallet.mnemonic = Some(mnemonic);

        let bytes = account_wallet_file_bytes(&wallet).unwrap();

        let mut json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();

        json["public_key"] = serde_json::Value::String("00".repeat(wallet.public_key.bytes.len()));

        let tampered_public = serde_json::to_vec(&json).unwrap();

        assert!(
            account_wallet_from_file_bytes(&tampered_public)
                .unwrap_err()
                .contains("public key does not match")
        );

        json["public_key"] = serde_json::Value::String(hex::encode(&wallet.public_key.bytes));

        json["private_key"] = serde_json::Value::String("00".repeat(32));

        let tampered_private = serde_json::to_vec(&json).unwrap();

        assert!(
            account_wallet_from_file_bytes(&tampered_private)
                .unwrap_err()
                .contains("private key does not match")
        );
    }
}
