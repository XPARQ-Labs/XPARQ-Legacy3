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
    // The zero-valued native CoinContract is committed to this chain identity.
    assert_eq!(genesis::CHAIN_SPEC_VERSION, 1);
    assert_eq!(
        genesis::chain_spec_hash().unwrap().into_bytes(),
        [
            75, 41, 124, 251, 231, 121, 69, 241, 13, 211, 192, 109, 134, 210, 84, 221, 53, 152,
            129, 181, 200, 78, 95, 222, 110, 114, 158, 92, 149, 58, 47, 91
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
