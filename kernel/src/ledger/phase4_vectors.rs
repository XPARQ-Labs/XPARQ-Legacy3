//! Frozen canonical bytes for a mainnet execution and asset lifecycle.

use super::*;

use crypto::{
    AccountSignatureScheme, Address, SigningSeed, address_from_public_key, canonical_bytes,
};

use crate::{
    blockchain::{Block, Emission, decode_block},
    common::Nonce,
    consensus::{
        ProtocolBurn, StateTransitionWeight, expected_emission_for_height,
        expected_next_difficulty, validate_candidate_for_apply,
    },
    genesis,
    monetary::coin::CoinOutput,
    transaction::{
        AccountAuthorization, AccountIntent, AuthorizedAccountIntent, AuthorizedProgramTransaction,
        AuthorizedTransaction, SpendCharges, SpendIntent, program_transaction_commitment,
    },
};

use extension::{
    asset_program::{
        asset::{AssetOutput, Unit},
        opcode::AssetOpcode,
        state::ExecutionContext,
        type_::{AssetCall, Burn, Mint, Register, Transfer},
    },
    script::call::{ProgramCall, ProgramId},
};

const FIXTURE: &str = include_str!("../../tests/vectors/phase4_mainnet.txt");

fn record<T: BorshSerialize>(
    vectors: &mut Vec<(&'static str, Vec<u8>)>,
    name: &'static str,
    value: &T,
) {
    vectors.push((name, canonical_bytes(value).unwrap()));
}

fn commit(ledger: &mut Ledger, mut block: Block) -> Block {
    let (root, weight) = ledger.preview_block_commitments(&block).unwrap();
    block.set_state_root(root);
    block.set_block_weight(weight);
    let validated = validate_candidate_for_apply(&block, &ledger.chain).unwrap();
    ledger.apply_validated_block(validated).unwrap();
    block
}

fn next_block(ledger: &Ledger, miner: Address, transactions: Vec<AuthorizedTransaction>) -> Block {
    let height = Height(ledger.tip_height().unwrap().0 + 1);
    Block::from_protocol_transactions(
        height,
        ledger.tip_hash().unwrap(),
        expected_next_difficulty(&ledger.chain).unwrap(),
        Nonce(0),
        Some(Emission::new(miner, expected_emission_for_height(height))),
        transactions,
    )
    .unwrap()
}

fn signed_spend(
    intent: SpendIntent,
    seed: &SigningSeed,
    chain: crate::common::ChainContext,
) -> AuthorizedTransaction {
    let commitment = intent.principal_commitment(chain).unwrap();
    AuthorizedTransaction::Spend(Box::new(AuthorizedAccountIntent {
        intent,
        authorization: AccountAuthorization {
            public_key: seed.public_key(),
            signature: seed.sign(commitment.as_bytes()),
        },
    }))
}

fn commit_program(ledger: &mut Ledger, seed: &SigningSeed, call: AssetCall) -> Block {
    let signer = address_from_public_key(&seed.public_key());
    let chain = ledger.chain_context.unwrap();
    let (opcode, payload) = match &call {
        AssetCall::Register(v) => (AssetOpcode::Register, borsh::to_vec(v).unwrap()),
        AssetCall::Mint(v) => (AssetOpcode::Mint, borsh::to_vec(v).unwrap()),
        AssetCall::Transfer(v) => (AssetOpcode::Transfer, borsh::to_vec(v).unwrap()),
        AssetCall::Burn(v) => (AssetOpcode::Burn, borsh::to_vec(v).unwrap()),
    };
    let mut preview = ledger.state.extensions.clone();
    preview
        .assets
        .apply(
            &call,
            ExecutionContext {
                signer,
                commitment: [11; 32],
            },
        )
        .unwrap();
    let growth = canonical_bytes(&preview)
        .unwrap()
        .len()
        .saturating_sub(canonical_bytes(&ledger.state.extensions).unwrap().len())
        as u64;
    let call = ProgramCall {
        program: ProgramId::ASSET,
        opcode: opcode as u8,
        payload,
    };
    let (input, coin) = ledger
        .state
        .utxos
        .coins()
        .filter(|(_, v)| v.owner == signer)
        .max_by_key(|(_, v)| v.amount)
        .unwrap();
    let amount = coin.amount;
    let sign = |output| {
        let payment = SpendIntent::coin_with_charges(
            signer,
            vec![input],
            vec![CoinOutput::new(signer, output)],
            SpendCharges::new(Zeno::ONE),
        )
        .unwrap();
        let commitment = program_transaction_commitment(signer, &call, &payment, chain).unwrap();
        AuthorizedTransaction::Program(Box::new(AuthorizedProgramTransaction {
            signer,
            call: call.clone(),
            payment,
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        }))
    };
    let size = canonical_bytes(&sign(Zeno::ONE)).unwrap().len() as u64;
    let burn = ProtocolBurn::for_transaction(
        StateTransitionWeight {
            created_coin_utxos: 2,
            consumed_coin_utxos: 1,
            created_state_weight: growth,
        },
        size,
    )
    .unwrap()
    .total()
    .unwrap();
    let tx = sign(
        amount
            .checked_sub(burn)
            .unwrap()
            .checked_sub(Zeno::ONE)
            .unwrap(),
    );
    let block = next_block(ledger, signer, vec![tx]);
    commit(ledger, block)
}

fn vector_data() -> Vec<(&'static str, Vec<u8>)> {
    let mut vectors = Vec::new();
    let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([0x24; 32]));
    let owner = address_from_public_key(&seed.public_key());
    let recipient = Address([0x35; crypto::ADDRESS_SIZE]);
    let other_miner = Address([0x46; crypto::ADDRESS_SIZE]);
    let chain = genesis::chain_context().unwrap();
    let mut ledger = genesis::genesis_ledger().unwrap();
    record(
        &mut vectors,
        "chain_spec_hash",
        &genesis::chain_spec_hash().unwrap(),
    );

    let first_candidate = next_block(&ledger, owner, vec![]);
    let first = commit(&mut ledger, first_candidate);
    record(&mut vectors, "block_1", &first);
    record(&mut vectors, "state_after_emission", &ledger.state);
    record(
        &mut vectors,
        "root_after_emission",
        &ledger.state_root().unwrap(),
    );

    let (input, coin) = ledger.state.utxos.coins().next().unwrap();
    let fee = Zeno::from_zeno(1_000);
    let draft = SpendIntent::coin_with_charges(
        owner,
        vec![input],
        vec![CoinOutput::new(recipient, Zeno::ONE)],
        SpendCharges::new(fee),
    )
    .unwrap();
    let draft_bytes = canonical_bytes(&signed_spend(draft, &seed, chain)).unwrap();
    let burn = ProtocolBurn::for_transaction(
        StateTransitionWeight {
            created_coin_utxos: 2,
            consumed_coin_utxos: 1,
            created_state_weight: 0,
        },
        draft_bytes.len() as u64,
    )
    .unwrap()
    .total()
    .unwrap();
    let amount = coin
        .amount
        .checked_sub(fee)
        .unwrap()
        .checked_sub(burn)
        .unwrap();
    let intent = SpendIntent::coin_with_charges(
        owner,
        vec![input],
        vec![CoinOutput::new(recipient, amount)],
        SpendCharges::new(fee),
    )
    .unwrap();
    let authorization_commitment = intent.principal_commitment(chain).unwrap();
    let transaction = signed_spend(intent.clone(), &seed, chain);
    let transaction_bytes = canonical_bytes(&transaction).unwrap();
    assert_eq!(transaction_bytes.len(), draft_bytes.len());
    assert_eq!(
        transaction_bytes,
        canonical_bytes(&signed_spend(intent.clone(), &seed, chain)).unwrap()
    );
    vectors.push((
        "authorization_commitment",
        authorization_commitment.as_bytes().to_vec(),
    ));
    vectors.push(("transaction", transaction_bytes));

    let second_candidate = next_block(&ledger, other_miner, vec![transaction]);
    let second = commit(&mut ledger, second_candidate);
    record(&mut vectors, "block_2_spend", &second);
    record(&mut vectors, "state_after_spend", &ledger.state);
    record(&mut vectors, "utxos_after_spend", &ledger.state.utxos);
    record(
        &mut vectors,
        "coin_counters_after_spend",
        &ledger.state.coin,
    );
    record(
        &mut vectors,
        "root_after_spend",
        &ledger.state_root().unwrap(),
    );
    let spend_state = canonical_bytes(&ledger.state).unwrap();
    let spend_root = ledger.state_root().unwrap();

    assert_eq!(ledger.rollback_tip().unwrap(), second);
    assert_eq!(
        canonical_bytes(&ledger.state).unwrap(),
        vectors
            .iter()
            .find(|(name, _)| *name == "state_after_emission")
            .unwrap()
            .1
    );
    let alternative_candidate = next_block(&ledger, recipient, vec![]);
    let alternative = commit(&mut ledger, alternative_candidate);
    assert_ne!(alternative.state_root(), second.state_root());
    record(&mut vectors, "block_2_alternative", &alternative);
    record(&mut vectors, "state_after_alternative", &ledger.state);
    record(
        &mut vectors,
        "root_after_alternative",
        &ledger.state_root().unwrap(),
    );
    assert_eq!(ledger.rollback_tip().unwrap(), alternative);
    let replayed = commit(&mut ledger, second.clone());
    assert_eq!(replayed, second);
    assert_eq!(canonical_bytes(&ledger.state).unwrap(), spend_state);
    assert_eq!(ledger.state_root().unwrap(), spend_root);

    let mut program_ledger = genesis::genesis_ledger().unwrap();
    let funding = next_block(&program_ledger, owner, vec![]);
    commit(&mut program_ledger, funding);
    let funded = canonical_bytes(&program_ledger.state).unwrap();
    let register = commit_program(
        &mut program_ledger,
        &seed,
        AssetCall::Register(Register {
            name: "Phase4 Vector".into(),
            max_supply: Unit::from_units(100),
            initial_mint: Unit::from_units(10),
            mint_authority: owner,
            nonce: 7,
        }),
    );
    let asset = *program_ledger
        .state
        .extensions
        .assets
        .records
        .keys()
        .next()
        .unwrap();
    record(&mut vectors, "program_register_block", &register);
    record(
        &mut vectors,
        "program_after_register",
        &program_ledger.state,
    );
    let registered = canonical_bytes(&program_ledger.state).unwrap();
    let mint = commit_program(
        &mut program_ledger,
        &seed,
        AssetCall::Mint(Mint {
            asset,
            nonce: 1,
            recipient: owner,
            amount: Unit::from_units(4),
        }),
    );
    record(&mut vectors, "program_mint_block", &mint);
    record(&mut vectors, "program_after_mint", &program_ledger.state);
    let minted = canonical_bytes(&program_ledger.state).unwrap();
    let inputs = program_ledger
        .state
        .extensions
        .assets
        .shares
        .keys()
        .copied()
        .collect();
    let transfer = commit_program(
        &mut program_ledger,
        &seed,
        AssetCall::Transfer(Transfer {
            asset,
            inputs,
            outputs: vec![AssetOutput::new(owner, Unit::from_units(14))],
        }),
    );
    record(&mut vectors, "program_transfer_block", &transfer);
    record(
        &mut vectors,
        "program_after_transfer",
        &program_ledger.state,
    );
    let transferred = canonical_bytes(&program_ledger.state).unwrap();
    let inputs = program_ledger
        .state
        .extensions
        .assets
        .shares
        .keys()
        .copied()
        .collect();
    let burn = commit_program(
        &mut program_ledger,
        &seed,
        AssetCall::Burn(Burn {
            asset,
            inputs,
            amount: Unit::from_units(3),
            output: Unit::from_units(11),
        }),
    );
    record(&mut vectors, "program_burn_block", &burn);
    record(&mut vectors, "program_after_burn", &program_ledger.state);
    record(
        &mut vectors,
        "program_root_after_burn",
        &program_ledger.state_root().unwrap(),
    );
    record(
        &mut vectors,
        "program_counters_after_burn",
        &program_ledger.state.extensions.assets.records[&asset],
    );
    program_ledger.state.validate_supply_invariants().unwrap();
    let mut replay = genesis::genesis_ledger().unwrap();
    for block in program_ledger.chain.blocks().skip(1) {
        let validated = validate_candidate_for_apply(block, &replay.chain).unwrap();
        replay.apply_validated_block(validated).unwrap();
    }
    assert_eq!(replay.state, program_ledger.state);
    for (block, previous) in [
        (burn, transferred),
        (transfer, minted),
        (mint, registered),
        (register, funded),
    ] {
        assert_eq!(program_ledger.rollback_tip().unwrap(), block);
        assert_eq!(canonical_bytes(&program_ledger.state).unwrap(), previous);
    }
    vectors
}

fn encoded_vectors() -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut text = String::new();
    for (name, bytes) in vector_data() {
        text.push_str(name);
        text.push(' ');
        for byte in bytes {
            text.push(HEX[(byte >> 4) as usize] as char);
            text.push(HEX[(byte & 15) as usize] as char);
        }
        text.push('\n');
    }
    text
}

#[test]
fn frozen_phase4_vectors_match_execution() {
    let actual = encoded_vectors();
    let expected = FIXTURE;
    if actual != expected {
        let actual_lines = actual.lines().collect::<Vec<_>>();
        let expected_lines = expected.lines().collect::<Vec<_>>();
        let first = actual_lines
            .iter()
            .zip(&expected_lines)
            .position(|(actual, expected)| actual != expected)
            .unwrap_or(actual_lines.len().min(expected_lines.len()));
        panic!(
            "Phase 4 vector drift at line {}: {}",
            first + 1,
            actual_lines
                .get(first)
                .and_then(|line| line.split_once(' '))
                .map_or("missing", |(name, _)| name)
        );
    }

    let line = |name: &str| {
        expected
            .lines()
            .find_map(|line| {
                line.strip_prefix(name)
                    .and_then(|tail| tail.strip_prefix(' '))
            })
            .unwrap()
    };
    let decode_vector = |name: &str| {
        let hex = line(name);
        (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect::<Vec<_>>()
    };
    for name in [
        "block_1",
        "block_2_spend",
        "block_2_alternative",
        "program_register_block",
        "program_mint_block",
        "program_transfer_block",
        "program_burn_block",
    ] {
        let bytes = decode_vector(name);
        let decoded = decode_block(&bytes).unwrap();
        assert_eq!(canonical_bytes(&decoded).unwrap(), bytes);
    }

    let mut replay = genesis::genesis_ledger().unwrap();
    for name in ["block_1", "block_2_spend"] {
        let block = decode_block(&decode_vector(name)).unwrap();
        let validated = validate_candidate_for_apply(&block, &replay.chain).unwrap();
        replay.apply_validated_block(validated).unwrap();
    }
    assert_eq!(
        canonical_bytes(&replay.state).unwrap(),
        decode_vector("state_after_spend")
    );
    assert_eq!(
        canonical_bytes(&replay.state_root().unwrap()).unwrap(),
        decode_vector("root_after_spend")
    );
}

#[test]
#[ignore = "run explicitly only when the consensus vectors are intentionally updated"]
fn regenerate_phase4_vectors() {
    std::fs::write(
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/vectors/phase4_mainnet.txt"
        ),
        encoded_vectors(),
    )
    .unwrap();
}
