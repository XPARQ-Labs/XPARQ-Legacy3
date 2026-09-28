use crypto::{
    AccountSignatureScheme, Address, HASH16_SIZE, SigningSeed, address_from_public_key,
    canonical_bytes,
};
use kernel::{
    common::ChainContext,
    consensus::{
        CoinInputState, ProtocolBurn, StateTransitionWeight, TransactionStateView,
        validate_transaction,
    },
    ledger::{CoinUtxo, LedgerState},
    monetary::{
        asset::{AssetContract, AssetError, AssetOutput, AssetShare, Share, Unit},
        coin::{CoinOutput, CoinShare, Zeno},
    },
    transaction::{
        AccountAuthorization, AccountIntent, AssetInstruction, AssetIntent,
        AuthorizedAccountIntent, AuthorizedTransaction, SpendCharges, SpendIntent,
    },
};

struct State {
    owner: Address,
    coin_owner: Address,
    asset: AssetContract,
}

#[test]
fn coin_spend_can_pay_only_miner_with_explicit_charges() {
    let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([44; 32]));
    let owner = address_from_public_key(&seed.public_key());
    let miner = Address([4; 21]);
    let chain = ChainContext::new([12; 32]);
    let coin = CoinShare::from_bytes([6; HASH16_SIZE]);
    let mut state = LedgerState::default();
    state
        .utxos
        .insert_coin(
            coin,
            CoinUtxo {
                amount: Zeno::from_zeno(100_000),
                owner,
            },
        )
        .unwrap();

    let provisional =
        SpendIntent::coin_with_charges(owner, vec![coin], vec![], SpendCharges::new(Zeno::ONE))
            .unwrap();
    let size = canonical_bytes(&signed(provisional, &seed, chain))
        .unwrap()
        .len() as u64;
    let burn = size * 8;
    let intent = SpendIntent::coin_with_charges(
        owner,
        vec![coin],
        vec![],
        SpendCharges::new(Zeno::from_zeno(100_000 - burn)),
    )
    .unwrap();
    let transaction = signed(intent.clone(), &seed, chain);
    let validated = validate_transaction(transaction, chain, 1, &state).unwrap();
    let journal = state
        .apply_validated_transaction(&validated, miner, chain)
        .unwrap();
    assert_eq!(journal.protocol_burn(), Zeno::from_zeno(burn));
    let new_coin = CoinShare::from_output(intent.semantic_commitment(chain).unwrap().as_bytes(), 0);
    assert_eq!(state.utxos.coin(&new_coin).unwrap().owner, miner);
    assert_eq!(
        state.utxos.coin(&new_coin).unwrap().amount,
        intent.charges.miner_fee
    );

    assert_eq!(
        borsh::to_vec(&CoinOutput::new(owner, Zeno::ONE))
            .unwrap()
            .len(),
        crypto::ADDRESS_SIZE + 8,
    );
}

#[test]
fn combined_spend_applies_both_state_transitions() {
    let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([43; 32]));
    let owner = address_from_public_key(&seed.public_key());
    let recipient = Address([7; 21]);
    let coin_recipient = Address([8; 21]);
    let chain = ChainContext::new([11; 32]);
    let registration = AssetIntent::new(
        AssetInstruction::Register {
            name: "Combined test".into(),
            max_supply: Unit::from_units(100),
            initial_mint: Unit::from_units(7),
            mint_authority: owner,
            nonce: 1,
        },
        owner,
    );
    let asset = registration.asset().unwrap();
    let mut state = LedgerState::default();
    state
        .assets
        .apply(&mut state.utxos, &registration, chain.genesis_hash)
        .unwrap();
    let share = Share::derive(
        asset,
        registration
            .semantic_commitment(chain.genesis_hash)
            .unwrap(),
        0,
    );
    let coin = CoinShare::from_bytes([5; HASH16_SIZE]);
    state
        .utxos
        .insert_coin(
            coin,
            CoinUtxo {
                amount: Zeno::from_zeno(100_000),
                owner,
            },
        )
        .unwrap();
    let fee = Zeno::from_zeno(1_000);
    let miner = Address([6; 21]);
    let mut intent = SpendIntent::combined_with_charges(
        owner,
        vec![coin],
        vec![CoinOutput::new(coin_recipient, Zeno::from_zeno(1))],
        asset,
        vec![share],
        vec![AssetOutput {
            recipient,
            amount: Unit::from_units(7),
        }],
        SpendCharges::new(fee),
    )
    .unwrap();
    let provisional = signed(intent.clone(), &seed, chain);
    let weight = state.asset_spend_created_state_weight(&intent).unwrap();
    let burn = ProtocolBurn::for_transaction(
        StateTransitionWeight {
            created_coin_utxos: 2,
            consumed_coin_utxos: 1,
            created_state_weight: weight,
        },
        canonical_bytes(&provisional).unwrap().len() as u64,
    )
    .unwrap()
    .total()
    .unwrap();
    if let kernel::transaction::Spend::Combined { coin_outputs, .. } = &mut intent.spend {
        coin_outputs[0].amount = Zeno::from_zeno(100_000 - fee.as_zeno() - burn.as_zeno());
    }
    let transaction = signed(intent.clone(), &seed, chain);
    let validated = validate_transaction(transaction, chain, 1, &state).unwrap();
    let journal = state
        .apply_validated_transaction(&validated, miner, chain)
        .unwrap();
    assert_eq!(journal.protocol_burn(), burn);
    assert!(state.utxos.coin(&coin).is_none());
    assert!(state.utxos.asset(&share).is_none());
    let commitment = intent.semantic_commitment(chain).unwrap();
    let new_coin = CoinShare::from_output(commitment.as_bytes(), 0);
    let miner_coin = CoinShare::from_output(commitment.as_bytes(), 1);
    let new_share = Share::derive(asset, commitment.into_bytes(), 0);
    assert_eq!(state.utxos.coin(&new_coin).unwrap().owner, coin_recipient);
    assert_eq!(state.utxos.coin(&miner_coin).unwrap().owner, miner);
    assert_eq!(state.utxos.coin(&miner_coin).unwrap().amount, fee);
    assert_eq!(state.utxos.asset(&new_share).unwrap().owner, recipient);
}

impl TransactionStateView for State {
    fn coin(&self, _id: CoinShare) -> Option<CoinInputState> {
        Some(CoinInputState {
            amount: Zeno::from_zeno(100_000),
            owner: self.coin_owner,
        })
    }

    fn asset_share(&self, _id: Share) -> Option<AssetShare> {
        Some(AssetShare {
            asset: self.asset,
            amount: Unit::from_units(7),
            owner: self.owner,
        })
    }

    fn asset_spend_created_state_weight(&self, _intent: &SpendIntent) -> Result<u64, AssetError> {
        Ok(0)
    }
}

fn signed(intent: SpendIntent, seed: &SigningSeed, chain: ChainContext) -> AuthorizedTransaction {
    let commitment = intent.principal_commitment(chain).unwrap();
    AuthorizedTransaction::Spend(Box::new(AuthorizedAccountIntent {
        spend: AuthorizedAccountIntent {
            intent,
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        },
    }))
}

#[test]
fn one_signature_authorizes_coin_and_asset_together() {
    let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([42; 32]));
    let owner = address_from_public_key(&seed.public_key());
    let asset = AssetContract::from_bytes([3; 32]);
    let chain = ChainContext::new([9; 32]);
    let mut intent = SpendIntent::combined(
        owner,
        vec![CoinShare::from_bytes([1; HASH16_SIZE])],
        vec![CoinOutput::new(owner, Zeno::from_zeno(1))],
        asset,
        vec![Share::from_bytes([2; HASH16_SIZE])],
        vec![AssetOutput {
            recipient: Address([8; 21]),
            amount: Unit::from_units(7),
        }],
    )
    .unwrap();
    let initial = signed(intent.clone(), &seed, chain);
    let burn = canonical_bytes(&initial).unwrap().len() as u64 * 8;
    if let kernel::transaction::Spend::Combined { coin_outputs, .. } = &mut intent.spend {
        coin_outputs[0].amount = Zeno::from_zeno(100_000 - burn);
    }
    let transaction = signed(intent.clone(), &seed, chain);
    assert!(canonical_bytes(&transaction).unwrap().len() < 5_000);
    let state = State {
        owner,
        coin_owner: owner,
        asset,
    };
    assert!(validate_transaction(transaction.clone(), chain, 1, &state).is_ok());

    let mut tampered = transaction.clone();
    if let AuthorizedTransaction::Spend(tx) = &mut tampered {
        if let kernel::transaction::Spend::Combined { coin_outputs, .. } = &mut tx.intent.spend {
            coin_outputs[0].output = Address([7; 21]);
        }
    }
    assert!(validate_transaction(tampered, chain, 1, &state).is_err());

    let mut tampered_fee = transaction.clone();
    if let AuthorizedTransaction::Spend(tx) = &mut tampered_fee {
        tx.intent.charges.miner_fee = Zeno::from_zeno(1);
    }
    assert!(validate_transaction(tampered_fee, chain, 1, &state).is_err());

    let other_owner = State {
        coin_owner: Address([6; 21]),
        ..state
    };
    assert!(validate_transaction(transaction, chain, 1, &other_owner).is_err());
}
