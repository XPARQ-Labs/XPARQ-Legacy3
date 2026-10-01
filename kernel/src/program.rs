//! Authenticated extension calls and on-chain payment validation.
//!
//! On-chain envelopes bind the asset operation and XPQ payment together.

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

/// Validated Program payment and execution context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedProgramTransaction {
    pub transaction: crate::transaction::AuthorizedProgramTransaction,
    pub required_burn: crate::monetary::coin::Zeno,
    pub created_state_weight: u64,
    pub(crate) height: u64,
}

/// Authenticate the complete envelope, preview asset execution, and check exact XPQ
/// payment. State growth uses the positive difference in canonical extension bytes.
/// Shrinking state never rebates the archival burn or miner fee.
pub fn prepare_program_transaction(
    transaction: crate::transaction::AuthorizedProgramTransaction,
    chain: ChainContext,
    height: u64,
    state: &impl crate::consensus::TransactionStateView,
) -> Result<PreparedProgramTransaction, crate::consensus::TransactionConsensusError> {
    use crate::consensus::transaction::{
        count_coin_outputs, validate_coin_inputs, validate_required_burn,
    };
    use crate::consensus::{
        ProtocolBurn, StateTransitionWeight, TransactionConsensusError as Error,
    };
    use crate::transaction::{AuthorizedTransaction, MAX_TRANSACTION_SIZE};
    transaction.validate_structure().map_err(Error::Intent)?;
    let size = canonical_bytes(&AuthorizedTransaction::Program(Box::new(
        transaction.clone(),
    )))
    .map_err(|_| Error::Encoding)?
    .len();
    if size > MAX_TRANSACTION_SIZE {
        return Err(Error::TransactionTooLarge);
    }
    if !transaction
        .verify_authorizations(chain, height)
        .map_err(Error::Intent)?
    {
        return Err(Error::InvalidAuthorization);
    }
    let extensions = state.extension_state().ok_or(Error::Intent(
        crate::transaction::IntentError::InvalidAssetCall,
    ))?;
    let created_state_weight = program_created_state_weight(&transaction, chain, extensions)?;
    let (inputs, outputs) = transaction.payment.coin_parts().ok_or(Error::Intent(
        crate::transaction::IntentError::InvalidAssetCall,
    ))?;
    let fee = transaction.payment.charges.miner_fee;
    let actual = validate_coin_inputs(inputs, outputs, fee, transaction.signer, state)?;
    let transition = StateTransitionWeight {
        created_coin_utxos: count_coin_outputs(outputs, fee)?,
        consumed_coin_utxos: inputs.len() as u64,
        created_state_weight,
    };
    validate_required_burn(actual, transition, size as u64)?;
    Ok(PreparedProgramTransaction {
        transaction,
        required_burn: ProtocolBurn::for_transaction(transition, size as u64)?.total()?,
        created_state_weight,
        height,
    })
}

/// Read-only state growth quote using the same execution preview as consensus.
pub fn program_created_state_weight(
    transaction: &crate::transaction::AuthorizedProgramTransaction,
    chain: ChainContext,
    extensions: &ExtensionState,
) -> Result<u64, crate::consensus::TransactionConsensusError> {
    use crate::consensus::{BurnError, TransactionConsensusError as Error};
    transaction.validate_structure().map_err(Error::Intent)?;
    let mut preview = extensions.clone();
    let DecodedProgramCall::Asset(call) = decode_program(&transaction.call)
        .map_err(|_| Error::Intent(crate::transaction::IntentError::InvalidAssetCall))?;
    let commitment = canonical_bytes(&(
        chain.genesis_hash,
        transaction.signer,
        &transaction.call,
        &transaction.payment,
    ))
    .map_err(|_| Error::Encoding)?;
    preview
        .assets
        .apply(
            &call,
            ExecutionContext {
                signer: transaction.signer,
                commitment: domain(HashDomain::AssetIntent, &commitment).into_bytes(),
            },
        )
        .map_err(|_| Error::Intent(crate::transaction::IntentError::InvalidAssetCall))?;
    let before = canonical_bytes(extensions)
        .map_err(|_| Error::Encoding)?
        .len();
    let after = canonical_bytes(&preview)
        .map_err(|_| Error::Encoding)?
        .len();
    let created_state_weight = u64::try_from(after.saturating_sub(before))
        .map_err(|_| Error::Burn(BurnError::WeightOverflow))?;
    Ok(created_state_weight)
}

#[cfg(test)]
mod payment_tests {
    use super::*;
    use crate::{
        consensus::{ProtocolBurn, StateTransitionWeight},
        ledger::{CoinUtxo, LedgerState},
        monetary::coin::{CoinOutput, CoinShare, Zeno},
        transaction::{AuthorizedProgramTransaction, AuthorizedTransaction, SpendIntent},
    };
    use crypto::{AccountSignatureScheme, SigningSeed, address_from_public_key};
    use extension::{
        asset_program::{asset::Unit, opcode::AssetOpcode, type_::Register},
        script::call::ProgramId,
    };

    #[test]
    fn payment_requires_exact_burn_and_binds_call_without_mutating_state() {
        let seed = SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([19; 32]));
        let signer = address_from_public_key(&seed.public_key());
        let chain = ChainContext::new([7; 32]);
        let input = CoinShare::from_bytes([2; crypto::HASH16_SIZE]);
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: AssetOpcode::Register as u8,
            payload: borsh::to_vec(&Register {
                name: "STAGED".into(),
                max_supply: Unit::from_units(100),
                initial_mint: Unit::from_units(10),
                mint_authority: signer,
                nonce: 1,
            })
            .unwrap(),
        };
        let mut state = LedgerState::default();
        state
            .utxos
            .insert_coin(
                input,
                CoinUtxo {
                    owner: signer,
                    amount: Zeno::from_zeno(1_000_000),
                },
            )
            .unwrap();
        let original = state.clone();
        let payment = SpendIntent::coin(
            signer,
            vec![input],
            vec![CoinOutput::new(signer, Zeno::from_zeno(1))],
        )
        .unwrap();
        let sign = |payment: SpendIntent| {
            let commitment =
                crate::transaction::program_transaction_commitment(signer, &call, &payment, chain)
                    .unwrap();
            AuthorizedProgramTransaction {
                signer,
                call: call.clone(),
                payment,
                authorization: AccountAuthorization {
                    public_key: seed.public_key(),
                    signature: seed.sign(commitment.as_bytes()),
                },
            }
        };
        let tx = sign(payment);
        assert!(prepare_program_transaction(tx.clone(), chain, 0, &state).is_err());
        let mut preview = state.extensions.clone();
        let DecodedProgramCall::Asset(decoded) = decode_program(&call).unwrap();
        preview
            .assets
            .apply(
                &decoded,
                ExecutionContext {
                    signer,
                    commitment: [1; 32],
                },
            )
            .unwrap();
        let growth = (canonical_bytes(&preview).unwrap().len()
            - canonical_bytes(&state.extensions).unwrap().len()) as u64;
        let size = canonical_bytes(&AuthorizedTransaction::Program(Box::new(tx.clone())))
            .unwrap()
            .len() as u64;
        let burn = ProtocolBurn::for_transaction(
            StateTransitionWeight {
                created_coin_utxos: 1,
                consumed_coin_utxos: 1,
                created_state_weight: growth,
            },
            size,
        )
        .unwrap()
        .total()
        .unwrap();
        let output = Zeno::from_zeno(1_000_000).checked_sub(burn).unwrap();
        let tx = sign(
            SpendIntent::coin(signer, vec![input], vec![CoinOutput::new(signer, output)]).unwrap(),
        );
        let prepared = prepare_program_transaction(tx.clone(), chain, 0, &state).unwrap();
        assert_eq!(prepared.required_burn, burn);
        assert_eq!(prepared.created_state_weight, growth);
        assert_eq!(state, original);
        let fee = Zeno::from_zeno(7);
        let fee_burn = ProtocolBurn::for_transaction(
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
        let change = Zeno::from_zeno(1_000_000)
            .checked_sub(fee_burn)
            .unwrap()
            .checked_sub(fee)
            .unwrap();
        let payment = SpendIntent::coin_with_charges(
            signer,
            vec![input],
            vec![CoinOutput::new(signer, change)],
            crate::transaction::SpendCharges::new(fee),
        )
        .unwrap();
        assert_eq!(
            prepare_program_transaction(sign(payment), chain, 0, &state)
                .unwrap()
                .required_burn,
            fee_burn
        );
        let underpaid = SpendIntent::coin(
            signer,
            vec![input],
            vec![CoinOutput::new(
                signer,
                output.checked_add(Zeno::from_zeno(1)).unwrap(),
            )],
        )
        .unwrap();
        assert!(prepare_program_transaction(sign(underpaid), chain, 0, &state).is_err());
        assert_eq!(state, original);
        assert!(matches!(
            prepare_program_transaction(tx, ChainContext::new([8; 32]), 0, &state),
            Err(crate::consensus::TransactionConsensusError::InvalidAuthorization)
        ));
    }
}
