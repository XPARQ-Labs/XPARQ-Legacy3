use crypto::{AccountSignatureScheme, HASH16_SIZE, SigningSeed, address_from_public_key};
use kernel::{
    common::ChainContext,
    monetary::{
        asset::Unit,
        coin::{CoinShare, Zeno},
    },
    transaction::{
        AccountAuthorization, AssetInstruction, AssetIntent, AuthorizedAssetTransaction,
        SpendCharges, SpendIntent, asset_call_commitment,
    },
};

#[test]
fn one_signature_binds_asset_call_payment_and_chain() {
    let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([41; 32]));
    let owner = address_from_public_key(&seed.public_key());
    let chain = ChainContext::new([7; 32]);
    let call = AssetIntent::new(
        AssetInstruction::Register {
            name: "Single Auth".into(),
            max_supply: Unit::from_units(1_000),
            initial_mint: Unit::from_units(10),
            mint_authority: owner,
            nonce: 1,
        },
        owner,
    );
    let payment = SpendIntent::coin_with_charges(
        owner,
        vec![CoinShare::from_bytes([9; HASH16_SIZE])],
        vec![],
        SpendCharges::new(Zeno::from_zeno(100)),
    )
    .unwrap();
    let commitment = asset_call_commitment(&call, &payment, chain).unwrap();
    let transaction = AuthorizedAssetTransaction {
        call,
        payment,
        authorization: AccountAuthorization {
            public_key: seed.public_key(),
            signature: seed.sign(commitment.as_bytes()),
        },
    };

    assert!(transaction.verify_authorizations(chain, 0).unwrap());
    assert!(
        !transaction
            .verify_authorizations(ChainContext::new([8; 32]), 0)
            .unwrap()
    );

    let mut changed_call = transaction.clone();
    changed_call.call = AssetIntent::new(
        AssetInstruction::Register {
            name: "Changed".into(),
            max_supply: Unit::from_units(1_000),
            initial_mint: Unit::from_units(10),
            mint_authority: owner,
            nonce: 1,
        },
        owner,
    );
    assert!(!changed_call.verify_authorizations(chain, 0).unwrap());

    let mut changed_payment = transaction.clone();
    changed_payment.payment.charges = SpendCharges::new(Zeno::from_zeno(101));
    assert!(!changed_payment.verify_authorizations(chain, 0).unwrap());

    let mut other_signer = transaction;
    other_signer.payment.signer = crypto::Address([3; 21]);
    assert!(other_signer.verify_authorizations(chain, 0).is_err());
}
