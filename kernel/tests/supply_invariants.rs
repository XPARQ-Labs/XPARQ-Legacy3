use crypto::{Address, HASH16_SIZE};
use kernel::{
    ledger::{CoinUtxo, LedgerError, LedgerState},
    monetary::coin::{CoinShare, Zeno},
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
fn program_shares_must_equal_recorded_supply_and_reference_known_assets() {
    use extension::asset_program::{
        asset::{AssetContract, Unit},
        state::ExecutionContext,
        type_::{AssetCall, Register},
    };
    let owner = Address([3; crypto::ADDRESS_SIZE]);
    let mut state = LedgerState::default();
    state
        .extensions
        .assets
        .apply(
            &AssetCall::Register(Register {
                name: "Guard Test".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: owner,
                nonce: 1,
            }),
            ExecutionContext {
                signer: owner,
                commitment: [4; 32],
            },
        )
        .unwrap();
    assert!(state.utxos.is_empty());
    assert!(state.validate_supply_invariants().is_ok());
    let id = *state.extensions.assets.shares.keys().next().unwrap();
    state.extensions.assets.shares.get_mut(&id).unwrap().amount = Unit::from_units(11);
    assert!(matches!(
        state.validate_supply_invariants(),
        Err(LedgerError::AssetSupplyMismatch)
    ));
    let share = state.extensions.assets.shares.get_mut(&id).unwrap();
    share.amount = Unit::from_units(10);
    share.asset = AssetContract::from_bytes([8; 32]);
    assert!(matches!(
        state.validate_supply_invariants(),
        Err(LedgerError::UnknownAssetShare)
    ));
}
