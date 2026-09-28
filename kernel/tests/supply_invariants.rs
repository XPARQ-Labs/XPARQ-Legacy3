use crypto::{Address, HASH16_SIZE};
use kernel::{
    ledger::{CoinUtxo, LedgerError, LedgerState},
    monetary::{
        asset::{AssetShare, Share, Unit},
        coin::{CoinShare, Zeno},
    },
    transaction::{AssetInstruction, AssetIntent},
};

#[test]
fn coin_utxos_must_equal_recorded_live_supply() {
    let mut state = LedgerState::default();
    let id = CoinShare::from_bytes([1; HASH16_SIZE]);
    state.coin.total_mined = Zeno::from_zeno(100);
    state.coin.total_burned = Zeno::from_zeno(10);
    state
        .utxos
        .insert_coin(
            id,
            CoinUtxo {
                amount: Zeno::from_zeno(90),
                owner: Address([2; 21]),
            },
        )
        .unwrap();
    assert!(state.validate_supply_invariants().is_ok());

    state.utxos.consume_coin(&id).unwrap();
    state
        .utxos
        .insert_coin(
            id,
            CoinUtxo {
                amount: Zeno::from_zeno(91),
                owner: Address([2; 21]),
            },
        )
        .unwrap();
    assert!(matches!(
        state.validate_supply_invariants(),
        Err(LedgerError::CoinSupplyMismatch)
    ));
}

#[test]
fn asset_shares_must_equal_recorded_supply() {
    let owner = Address([3; 21]);
    let call = AssetIntent::new(
        AssetInstruction::Register {
            name: "Guard Test".into(),
            max_supply: Unit::from_units(100),
            initial_mint: Unit::from_units(10),
            mint_authority: owner,
            nonce: 1,
        },
        owner,
    );
    let asset = call.asset().unwrap();
    let genesis_hash = [4; 32];
    let mut state = LedgerState::default();
    state
        .assets
        .apply(&mut state.utxos, &call, genesis_hash)
        .unwrap();
    assert!(state.validate_supply_invariants().is_ok());

    let share_id = Share::derive(asset, call.semantic_commitment(genesis_hash).unwrap(), 0);
    let share = state.utxos.consume_asset(&share_id).unwrap();
    state
        .utxos
        .insert_asset(
            share_id,
            AssetShare {
                amount: Unit::from_units(11),
                ..share
            },
        )
        .unwrap();
    assert!(matches!(
        state.validate_supply_invariants(),
        Err(LedgerError::AssetSupplyMismatch)
    ));

    state.utxos.consume_asset(&share_id).unwrap();
    state
        .utxos
        .insert_asset(
            share_id,
            AssetShare {
                asset: kernel::monetary::asset::AssetContract::from_bytes([8; 32]),
                amount: Unit::from_units(10),
                owner,
            },
        )
        .unwrap();
    assert!(matches!(
        state.validate_supply_invariants(),
        Err(LedgerError::UnknownAssetShare)
    ));
}
