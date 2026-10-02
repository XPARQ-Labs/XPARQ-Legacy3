use super::*;
use extension::{
    asset_program::{
        asset::{AssetContract, AssetOutput, Share, Unit},
        opcode::AssetOpcode,
        type_::{AssetCall, Burn, Mint, Register, Transfer},
    },
    script::call::{ProgramCall, ProgramId},
};
use kernel::{
    consensus::{ProtocolBurn, StateTransitionWeight},
    transaction::CoinCharges,
};

fn signed_program(rpc: &str, owner: &AccountWallet, instruction: &AssetCall) -> Transaction {
    let (opcode, payload) = match instruction {
        AssetCall::Register(call) => (AssetOpcode::Register, borsh::to_vec(call).unwrap()),
        AssetCall::Mint(call) => (AssetOpcode::Mint, borsh::to_vec(call).unwrap()),
        AssetCall::Transfer(call) => (AssetOpcode::Transfer, borsh::to_vec(call).unwrap()),
        AssetCall::Burn(call) => (AssetOpcode::Burn, borsh::to_vec(call).unwrap()),
    };
    let call = ProgramCall {
        program: ProgramId::ASSET,
        opcode: opcode as u8,
        payload,
    };
    let account = account(rpc, &address_to_string(&owner.address)).unwrap();
    let input = account["utxos"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|input| input["reserved"] == false)
        .max_by_key(|input| input["amount"].as_u64().unwrap())
        .unwrap();
    let id = input["id"].as_str().unwrap().parse().unwrap();
    let amount = input["amount"].as_u64().unwrap();
    let mut size = 0;
    let mut growth = None;
    for _ in 0..8 {
        let fee = (size * 8).max(1);
        let burn = ProtocolBurn::for_transaction(
            StateTransitionWeight {
                created_coin_utxos: 2,
                consumed_coin_utxos: 1,
                created_state_weight: growth.unwrap_or(0),
            },
            size,
        )
        .unwrap()
        .total()
        .unwrap()
        .as_zeno();
        let payment = CoinTransition::coin_with_charges(
            owner.address,
            vec![id],
            vec![CoinOutput::new(
                owner.address,
                Zeno::from_zeno(amount.checked_sub(burn + fee).unwrap()),
            )],
            CoinCharges::new(Zeno::from_zeno(fee)),
        )
        .unwrap();
        let transaction = AuthorizedTransaction::Program(Box::new(
            owner.sign_program_call(call.clone(), payment).unwrap(),
        ));
        if growth.is_none() {
            growth = Some(
                post_program_rpc(rpc, "/program/quote", &transaction)["created_state_weight"]
                    .as_u64()
                    .unwrap(),
            );
        } else if canonical_bytes(&transaction).unwrap().len() as u64 == size {
            return transaction;
        }
        size = canonical_bytes(&transaction).unwrap().len() as u64;
    }
    panic!("Program fee sizing did not converge");
}

fn wait_program_mempool(rpc: &str, owner: &str, input: &str) {
    let deadline = Instant::now() + WAIT;
    loop {
        if account(rpc, owner).is_ok_and(|account| {
            account["utxos"]
                .as_array()
                .unwrap()
                .iter()
                .any(|utxo| utxo["id"] == input && utxo["reserved"] == true)
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "Program did not reach mempool at {rpc}"
        );
        thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn program_lifecycle_gossips_across_three_nodes_and_rolls_back_on_reorg() {
    let root = temp_root("program-gossip-reorg");
    let a = root.join("a");
    let b = root.join("b");
    let c = root.join("c");
    let common = root.join("registered");
    let alternative = root.join("alternative");
    let ap = free_address();
    let ar = free_address();
    let bp = free_address();
    let br = free_address();
    let cp = free_address();
    let cr = free_address();
    let owner = sender_wallet();
    let owner_address = address_to_string(&owner.address);
    let recipient = address_from_public_key(
        &SigningSeed::new(Signature::MlDsa44, Box::new([72; 32])).public_key(),
    );
    let recipient_address = address_to_string(&recipient);
    mine(&a, 1);
    let mut an = start_node(&a, &ap, &ar, &[], None);
    wait_for_status(&ar, |s| s["tip_height"] == 1);
    let mut bn = start_node(&b, &bp, &br, &[&ap], None);
    wait_for_status(&br, |s| s["tip_height"] == 1);
    let mut cn = start_node(&c, &cp, &cr, &[&bp], None);
    wait_for_status(&cr, |s| s["tip_height"] == 1);
    let mut asset: Option<AssetContract> = None;
    let mut hashes = Vec::new();
    for step in 0..5 {
        eprintln!("Program lifecycle step {step}: gossip and mining");
        let instruction = if step == 0 {
            AssetCall::Register(Register {
                name: "GOSSIPPROGRAM".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(40),
                mint_authority: owner.address,
                nonce: 17,
            })
        } else {
            let asset = asset.unwrap();
            let balance = http_get(
                &ar,
                &format!("/program/asset/{asset}/balance/{owner_address}"),
            )
            .unwrap();
            let shares = balance["shares"].as_array().unwrap();
            let inputs: Vec<Share> = shares
                .iter()
                .map(|share| share["share_id"].as_str().unwrap().parse().unwrap())
                .collect();
            match step {
                1 => AssetCall::Mint(Mint {
                    asset,
                    nonce: 1,
                    recipient: owner.address,
                    amount: Unit::from_units(20),
                }),
                2 => AssetCall::Transfer(Transfer {
                    asset,
                    inputs,
                    outputs: vec![
                        AssetOutput::new(owner.address, Unit::from_units(20)),
                        AssetOutput::new(owner.address, Unit::from_units(25)),
                        AssetOutput::new(recipient, Unit::from_units(15)),
                    ],
                }),
                3 => AssetCall::Burn(Burn {
                    asset,
                    inputs: vec![inputs[0]],
                    amount: Unit::from_units(5),
                    output: Unit::from_units(
                        shares[0]["amount"]
                            .as_str()
                            .unwrap()
                            .parse::<u128>()
                            .unwrap()
                            - 5,
                    ),
                }),
                4 => {
                    assert_eq!(inputs.len(), 2);
                    AssetCall::Transfer(Transfer {
                        asset,
                        inputs,
                        outputs: vec![AssetOutput::new(owner.address, Unit::from_units(40))],
                    })
                }
                _ => unreachable!(),
            }
        };
        // Only one node receives an RPC submission; alternate the ingress direction.
        let ingress = if step % 2 == 0 { &ar } else { &cr };
        let tx = signed_program(ingress, &owner, &instruction);
        let AuthorizedTransaction::Program(program) = &tx;
        let input = program.payment.coin_parts().unwrap().0[0].to_string();
        let hash = hex::encode(tx.id().unwrap());
        assert_eq!(post_transaction(ingress, &tx)["hash"], hash);
        for rpc in [&ar, &br, &cr] {
            wait_program_mempool(rpc, &owner_address, &input);
        }
        hashes.push(hash);
        // The middle node mines a transaction it received exclusively through P2P.
        drop(bn);
        mine(&b, 1);
        if step == 0 {
            copy_tree(&b, &common);
        }
        bn = start_node(&b, &bp, &br, &[&ap], None);
        let expected = wait_for_status(&br, |s| s["tip_height"] == step + 2);
        for rpc in [&ar, &cr] {
            wait_for_status(rpc, |s| s["tip_hash"] == expected["tip_hash"]);
        }
        if step == 0 {
            asset = Some(
                account(&ar, &owner_address).unwrap()["program_assets"][0]["asset"]
                    .as_str()
                    .unwrap()
                    .parse()
                    .unwrap(),
            );
        }
        let asset = asset.unwrap();
        for route in [
            format!("/program/asset/{asset}"),
            format!("/program/asset/{asset}/balance/{owner_address}"),
            format!("/program/asset/{asset}/balance/{recipient_address}"),
        ] {
            let expected = http_get(&br, &route).unwrap();
            for rpc in [&ar, &cr] {
                assert_eq!(http_get(rpc, &route).unwrap(), expected);
            }
        }
        for rpc in [&ar, &br, &cr] {
            assert_eq!(
                http_get(
                    rpc,
                    &format!("/explorer/transaction/{}", hashes.last().unwrap())
                )
                .unwrap()["status"],
                "confirmed"
            );
        }
    }
    let asset = asset.unwrap();
    let metadata_route = format!("/program/asset/{asset}");
    let owner_route = format!("/program/asset/{asset}/balance/{owner_address}");
    let recipient_route = format!("/program/asset/{asset}/balance/{recipient_address}");
    let routes = [&metadata_route, &owner_route, &recipient_route];
    let metadata = http_get(&ar, &metadata_route).unwrap();
    assert_eq!(metadata["supply"], "55");
    assert_eq!(metadata["total_minted"], "60");
    assert_eq!(metadata["total_burned"], "5");
    assert_eq!(http_get(&ar, &owner_route).unwrap()["balance"], "40");
    assert_eq!(
        http_get(&ar, &owner_route).unwrap()["shares"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(http_get(&ar, &recipient_route).unwrap()["balance"], "15");
    let snapshots: Vec<_> = routes
        .iter()
        .map(|route| http_get(&ar, route).unwrap())
        .collect();
    let tip = status(&ar).unwrap()["tip_hash"].clone();
    drop(an);
    drop(bn);
    drop(cn);
    an = start_node(&a, &ap, &ar, &[], None);
    bn = start_node(&b, &bp, &br, &[&ap], None);
    cn = start_node(&c, &cp, &cr, &[&bp], None);
    for rpc in [&ar, &br, &cr] {
        wait_for_status(rpc, |s| s["tip_hash"] == tip);
        for (route, expected) in routes.iter().zip(&snapshots) {
            assert_eq!(&http_get(rpc, route).unwrap(), expected);
        }
    }
    drop(an);
    drop(bn);
    drop(cn);
    // A stronger fork retains registration but discards the four later operations.
    copy_tree(&common, &alternative);
    mine(&alternative, 5);
    let fp = free_address();
    let fr = free_address();
    let fork = start_node(&alternative, &fp, &fr, &[], None);
    let stronger = wait_for_status(&fr, |s| s["tip_height"] == 7);
    an = start_node(&a, &ap, &ar, &[&fp], None);
    bn = start_node(&b, &bp, &br, &[&ap], None);
    cn = start_node(&c, &cp, &cr, &[&bp], None);
    for rpc in [&ar, &br, &cr] {
        wait_for_status(rpc, |s| s["tip_hash"] == stronger["tip_hash"]);
        assert_eq!(http_get(rpc, &metadata_route).unwrap()["supply"], "40");
        assert_eq!(
            http_get(rpc, &metadata_route).unwrap()["total_minted"],
            "40"
        );
        assert_eq!(http_get(rpc, &metadata_route).unwrap()["total_burned"], "0");
        assert_eq!(http_get(rpc, &owner_route).unwrap()["balance"], "40");
        assert_eq!(http_get(rpc, &recipient_route).unwrap()["balance"], "0");
        for route in routes {
            assert_eq!(http_get(rpc, route).unwrap(), http_get(&fr, route).unwrap());
        }
        assert_eq!(
            http_get(rpc, &format!("/explorer/transaction/{}", hashes[0])).unwrap()["status"],
            "confirmed"
        );
        for hash in &hashes[1..] {
            assert!(
                http_get(rpc, &format!("/explorer/transaction/{hash}"))
                    .unwrap()
                    .get("error")
                    .is_some(),
                "orphan Program transaction remains indexed"
            );
        }
    }
    let rolled_back: Vec<_> = routes
        .iter()
        .map(|route| http_get(&fr, route).unwrap())
        .collect();
    drop(an);
    drop(bn);
    drop(cn);
    drop(fork);
    for (db, p2p, rpc) in [(&a, &ap, &ar), (&b, &bp, &br), (&c, &cp, &cr)] {
        let node = start_node(db, p2p, rpc, &[], None);
        wait_for_status(rpc, |s| s["tip_hash"] == stronger["tip_hash"]);
        for (route, expected) in routes.iter().zip(&rolled_back) {
            assert_eq!(&http_get(rpc, route).unwrap(), expected);
        }
        assert_eq!(http_get(rpc, &metadata_route).unwrap()["supply"], "40");
        assert_eq!(http_get(rpc, &recipient_route).unwrap()["balance"], "0");
        drop(node);
        assert!(
            Command::new(node_binary())
                .args(["check", db.to_str().unwrap()])
                .stdout(Stdio::null())
                .status()
                .unwrap()
                .success()
        );
    }
    fs::remove_dir_all(root).unwrap();
}
