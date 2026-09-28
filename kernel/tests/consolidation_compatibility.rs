//! Frozen values captured from the working tree before crate consolidation.

#[test]
#[cfg(feature = "mainnet")]
fn mainnet_genesis_and_chain_spec_match_the_current_structure() {
    use kernel::{codec, genesis};
    assert_eq!(
        genesis::genesis_hash().unwrap().into_bytes(),
        [
            212, 9, 99, 195, 104, 129, 74, 178, 35, 16, 87, 234, 192, 76, 226, 169, 187, 120, 36,
            172, 185, 246, 186, 145, 237, 119, 232, 131, 179, 234, 188, 197
        ]
    );
    // The target-bits header and current consensus parameters define a new chain
    // identity while the native asset IDs below remain frozen independently.
    assert_eq!(genesis::CHAIN_SPEC_VERSION, 3);
    assert_eq!(
        genesis::chain_spec_hash().unwrap().into_bytes(),
        [
            144, 110, 3, 100, 135, 55, 114, 199, 137, 213, 171, 190, 192, 239, 253, 238, 83, 94,
            128, 32, 6, 176, 42, 116, 230, 235, 250, 179, 164, 76, 126, 0
        ]
    );
    assert_ne!(
        genesis::chain_spec_hash().unwrap().into_bytes(),
        [
            166, 191, 246, 35, 4, 125, 100, 134, 53, 253, 52, 62, 165, 243, 91, 247, 90, 255, 55,
            241, 236, 179, 25, 8, 168, 155, 66, 92, 154, 68, 94, 218
        ]
    );
    let expected = [
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 255, 255, 127, 32, 125, 0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
        0, 0, 0, 0, 0, 0, 0, 0,
    ];
    let block = genesis::genesis_block().unwrap();
    assert_eq!(codec::block_bytes(&block).unwrap(), expected);
    assert_eq!(codec::decode_block(&expected).unwrap(), block);
    genesis::genesis_ledger().unwrap();
}

#[test]
fn native_asset_and_share_ids_are_frozen() {
    use kernel::{
        crypto::{ADDRESS_SIZE, Address},
        monetary::asset::{AssetContract, Metadata, Share, Unit},
    };
    let parent = AssetContract::derive(
        &Metadata::new(
            "Test Asset".into(),
            Unit::from_units(1_000_000),
            Address([7; ADDRESS_SIZE]),
            Address([7; ADDRESS_SIZE]),
        )
        .unwrap(),
        0,
    )
    .unwrap();
    assert_eq!(
        parent.to_string(),
        "71495bdff294767e0164dd776bad398ad5ebb638263176ff16092dcb369aa4aa"
    );
    assert_eq!(
        Share::derive(parent, [9; 32], 3).to_string(),
        "881a6a48837e0bc76a91790cd124507c"
    );
}
