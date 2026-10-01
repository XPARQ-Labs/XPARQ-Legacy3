//! Staging bridge for authenticated extension calls.
//!
//! These calls are not an on-chain transaction variant yet. Consensus activation
//! requires transaction fees, state-root commitment, snapshots, and rollback.

use borsh::{BorshDeserialize, BorshSerialize};
use crypto::{Address, HASH_SIZE, HashDomain, canonical_bytes, domain};
use extension::{
    asset_program::{
        asset::AssetError,
        state::{AssetJournal, ExecutionContext},
    },
    script::{
        call::{MAX_PROGRAM_PAYLOAD_SIZE, ProgramCall},
        execute::{DecodedProgramCall, decode_program},
        opcode::ProgramError,
        state::ExtensionState,
    },
};

use crate::{
    common::ChainContext,
    transaction::{AccountAuthorization, AuthorizationCommitment, AuthorizationRole},
};

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct AuthorizedProgramCall {
    pub signer: Address,
    pub call: ProgramCall,
    pub authorization: AccountAuthorization,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramCallError {
    TooLarge,
    Encoding,
    Program(ProgramError),
    InvalidAuthorization,
    Asset(AssetError),
}

/// Signature commitment binds the chain, signer, program ID, opcode, and payload.
pub fn program_call_commitment(
    signer: Address,
    call: &ProgramCall,
    chain: ChainContext,
) -> Result<AuthorizationCommitment, ProgramCallError> {
    if call.payload.len() > MAX_PROGRAM_PAYLOAD_SIZE {
        return Err(ProgramCallError::TooLarge);
    }
    decode_program(call).map_err(ProgramCallError::Program)?;
    let bytes = canonical_bytes(&(
        chain.genesis_hash,
        AuthorizationRole::Principal,
        b"xparq:extension-program-call:v1",
        signer,
        call,
    ))
    .map_err(|_| ProgramCallError::Encoding)?;
    Ok(AuthorizationCommitment::from_bytes(
        domain(HashDomain::AssetIntent, &bytes).into_bytes(),
    ))
}

pub struct ValidatedProgramCall {
    signer: Address,
    call: DecodedProgramCall,
    commitment: [u8; HASH_SIZE],
}

pub fn validate_program_call(
    signed: &AuthorizedProgramCall,
    chain: ChainContext,
    height: u64,
) -> Result<ValidatedProgramCall, ProgramCallError> {
    let commitment = program_call_commitment(signed.signer, &signed.call, chain)?;
    if !signed
        .authorization
        .verify_commitment(signed.signer, &commitment, height)
    {
        return Err(ProgramCallError::InvalidAuthorization);
    }
    let call = decode_program(&signed.call).map_err(ProgramCallError::Program)?;
    let bytes = canonical_bytes(&(chain.genesis_hash, signed.signer, &signed.call))
        .map_err(|_| ProgramCallError::Encoding)?;
    let object_commitment = domain(HashDomain::AssetIntent, &bytes).into_bytes();
    Ok(ValidatedProgramCall {
        signer: signed.signer,
        call,
        commitment: object_commitment,
    })
}

impl ValidatedProgramCall {
    pub fn apply(&self, state: &mut ExtensionState) -> Result<AssetJournal, ProgramCallError> {
        match &self.call {
            DecodedProgramCall::Asset(call) => state
                .assets
                .apply(
                    call,
                    ExecutionContext {
                        signer: self.signer,
                        commitment: self.commitment,
                    },
                )
                .map_err(ProgramCallError::Asset),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key};
    use extension::{
        asset_program::{asset::Unit, opcode::AssetOpcode, type_::Register},
        script::call::ProgramId,
    };

    #[test]
    fn signed_call_binds_chain_payload_and_signer_before_execution() {
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([9; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let chain = ChainContext::new([3; HASH_SIZE]);
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: AssetOpcode::Register as u8,
            payload: borsh::to_vec(&Register {
                name: "BRIDGE".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: signer,
                nonce: 1,
            })
            .unwrap(),
        };
        let commitment = program_call_commitment(signer, &call, chain).unwrap();
        let signed = AuthorizedProgramCall {
            signer,
            call,
            authorization: AccountAuthorization {
                public_key: seed.public_key(),
                signature: seed.sign(commitment.as_bytes()),
            },
        };
        assert!(matches!(
            validate_program_call(&signed, ChainContext::new([4; HASH_SIZE]), 0),
            Err(ProgramCallError::InvalidAuthorization)
        ));
        let mut changed = signed.clone();
        changed.call.payload[4] ^= 1;
        assert!(matches!(
            validate_program_call(&changed, chain, 0),
            Err(ProgramCallError::InvalidAuthorization)
        ));
        let mut changed_signer = signed.clone();
        changed_signer.signer = Address::ZERO;
        assert!(matches!(
            validate_program_call(&changed_signer, chain, 0),
            Err(ProgramCallError::InvalidAuthorization)
        ));
        let validated = validate_program_call(&signed, chain, 0).unwrap();
        let mut state = ExtensionState::default();
        let journal = validated.apply(&mut state).unwrap();
        assert_eq!(state.assets.records.len(), 1);
        state.assets.rollback(journal);
        assert_eq!(state, ExtensionState::default());
    }
}
