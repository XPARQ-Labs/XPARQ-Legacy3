use std::{collections::BTreeSet, error::Error as StdError, fmt};

use crypto::{Address, canonical_bytes};

use crate::{
    common::ChainContext,
    consensus::{
        BurnError, ProtocolBurn, StateTransitionWeight, created_coin_output_count,
        validate_exact_burn,
    },
    monetary::{
        asset::{AssetError, AssetShare, Share},
        coin::{CoinOutput, CoinShare, Zeno},
    },
    transaction::{
        AssetInstruction, AssetIntent, AuthorizedAccountIntent, AuthorizedTransaction, IntentError,
        Spend, SpendIntent, SpendIntentCommitment, Transaction as OnChainTransaction,
    },
};

pub trait ConsensusIntent: Clone {
    fn validate_structure(&self) -> Result<(), IntentError>;
    fn commitment_for(&self, chain: ChainContext) -> Result<SpendIntentCommitment, IntentError>;
}

impl ConsensusIntent for SpendIntent {
    fn validate_structure(&self) -> Result<(), IntentError> {
        self.validate()
    }

    fn commitment_for(&self, chain: ChainContext) -> Result<SpendIntentCommitment, IntentError> {
        self.semantic_commitment(chain)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructurallyValidated<T> {
    intent: T,
    commitment: SpendIntentCommitment,
}

impl<T> StructurallyValidated<T> {
    pub fn intent(&self) -> &T {
        &self.intent
    }

    pub const fn commitment(&self) -> SpendIntentCommitment {
        self.commitment
    }

    pub fn into_intent(self) -> T {
        self.intent
    }
}

pub fn validate_intent<T: ConsensusIntent>(
    intent: T,
    chain: ChainContext,
) -> Result<StructurallyValidated<T>, TransactionConsensusError> {
    intent
        .validate_structure()
        .map_err(TransactionConsensusError::Intent)?;

    let commitment = intent
        .commitment_for(chain)
        .map_err(TransactionConsensusError::Intent)?;

    Ok(StructurallyValidated { intent, commitment })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthorizationValidated<T> {
    intent: T,
    commitment: SpendIntentCommitment,
}

impl<T> AuthorizationValidated<T> {
    pub fn intent(&self) -> &T {
        &self.intent
    }

    pub const fn commitment(&self) -> SpendIntentCommitment {
        self.commitment
    }
}

/// Validated direct on-chain transaction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidatedTransaction {
    CoinSpend(ValidatedCoinSpend),
    CombinedSpend(ValidatedCombinedSpend),
    AssetCall(ValidatedAssetCall),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCoinSpend {
    pub spend: AuthorizationValidated<SpendIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCombinedSpend {
    pub spend: AuthorizationValidated<SpendIntent>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedAssetCall {
    pub call: AuthorizationValidated<AssetIntent>,
    pub payment: AuthorizationValidated<SpendIntent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinInputState {
    pub amount: Zeno,
    pub owner: Address,
}

pub trait TransactionStateView {
    fn coin(&self, id: CoinShare) -> Option<CoinInputState>;

    fn asset_share(&self, _id: Share) -> Option<AssetShare> {
        None
    }

    fn asset_spend_created_state_weight(&self, _intent: &SpendIntent) -> Result<u64, AssetError> {
        Err(AssetError::UnknownAsset)
    }

    fn asset_transition_created_state_weight(
        &self,
        _call: &AssetIntent,
        _genesis_hash: [u8; 32],
    ) -> Result<u64, AssetError> {
        Err(AssetError::UnknownAsset)
    }
}

pub fn validate_transaction(
    transaction: OnChainTransaction,
    chain: ChainContext,
    current_height: u64,
    state: &impl TransactionStateView,
) -> Result<ValidatedTransaction, TransactionConsensusError> {
    //
    // Authorization is a transaction-level invariant.
    //
    // Principal and payment signatures MUST be verified together here so
    // role separation and parent-binding cannot be bypassed by consensus.
    //
    validate_authorization_gate(&transaction, chain, current_height)?;

    let canonical_transaction_weight = u64::try_from(
        canonical_bytes(&transaction)
            .map_err(|_| TransactionConsensusError::Encoding)?
            .len(),
    )
    .map_err(|_| TransactionConsensusError::Burn(BurnError::WeightOverflow))?;

    validate_authorized_transaction(transaction, chain, canonical_transaction_weight, state)
}

fn validate_authorization_gate(
    transaction: &AuthorizedTransaction,
    chain: ChainContext,
    current_height: u64,
) -> Result<(), TransactionConsensusError> {
    transaction
        .validate_structure()
        .map_err(TransactionConsensusError::Intent)?;

    let valid = transaction
        .verify_authorizations(chain, current_height)
        .map_err(TransactionConsensusError::Intent)?;

    if !valid {
        return Err(TransactionConsensusError::InvalidAuthorization);
    }

    Ok(())
}

fn validate_authorized_transaction(
    transaction: AuthorizedTransaction,
    chain: ChainContext,
    canonical_transaction_weight: u64,
    state: &impl TransactionStateView,
) -> Result<ValidatedTransaction, TransactionConsensusError> {
    match transaction {
        AuthorizedTransaction::Spend(transaction) => {
            let transaction = *transaction;

            let spend = prepare_spend_intent(transaction, chain)?;

            match &spend.intent().spend {
                Spend::Coin { inputs, outputs } => {
                    let actual_burn = validate_coin_inputs(
                        inputs,
                        outputs,
                        spend.intent().charges.miner_fee,
                        spend.intent().signer,
                        state,
                    )?;

                    validate_required_burn(
                        actual_burn,
                        StateTransitionWeight {
                            created_coin_utxos: count_coin_outputs(
                                outputs,
                                spend.intent().charges.miner_fee,
                            )?,
                            consumed_coin_utxos: count_inputs(inputs.len())?,
                            ..StateTransitionWeight::default()
                        },
                        canonical_transaction_weight,
                    )?;

                    Ok(ValidatedTransaction::CoinSpend(ValidatedCoinSpend {
                        spend,
                    }))
                }

                Spend::Combined {
                    coin_inputs,
                    coin_outputs,
                    asset_inputs,
                    ..
                } => {
                    validate_share_ownership(asset_inputs, spend.intent().signer, state)?;
                    let actual_burn = validate_coin_inputs(
                        coin_inputs,
                        coin_outputs,
                        spend.intent().charges.miner_fee,
                        spend.intent().signer,
                        state,
                    )?;
                    let asset_weight = state
                        .asset_spend_created_state_weight(spend.intent())
                        .map_err(TransactionConsensusError::Asset)?;
                    validate_required_burn(
                        actual_burn,
                        StateTransitionWeight {
                            created_coin_utxos: count_coin_outputs(
                                coin_outputs,
                                spend.intent().charges.miner_fee,
                            )?,
                            consumed_coin_utxos: count_inputs(coin_inputs.len())?,
                            created_state_weight: asset_weight,
                        },
                        canonical_transaction_weight,
                    )?;
                    Ok(ValidatedTransaction::CombinedSpend(
                        ValidatedCombinedSpend { spend },
                    ))
                }
            }
        }

        AuthorizedTransaction::Asset(transaction) => {
            let transaction = *transaction;

            let call = prepare_asset_intent(transaction.call, chain)?;

            if let AssetInstruction::Burn { inputs, .. } = &call.intent().instruction {
                validate_share_ownership(inputs, call.intent().signer, state)?;
            }

            let asset_created_state_weight = state
                .asset_transition_created_state_weight(call.intent(), chain.genesis_hash)
                .map_err(TransactionConsensusError::Asset)?;

            let payment = prepare_spend_intent(transaction.payment, chain)?;

            let (payment_inputs, payment_outputs) = coin_parts(payment.intent())?;

            let actual_burn = validate_coin_inputs(
                payment_inputs,
                payment_outputs,
                payment.intent().charges.miner_fee,
                payment.intent().signer,
                state,
            )?;

            validate_required_burn(
                actual_burn,
                StateTransitionWeight {
                    created_coin_utxos: count_coin_outputs(
                        payment_outputs,
                        payment.intent().charges.miner_fee,
                    )?,
                    consumed_coin_utxos: count_inputs(payment_inputs.len())?,
                    created_state_weight: asset_created_state_weight,
                },
                canonical_transaction_weight,
            )?;

            Ok(ValidatedTransaction::AssetCall(ValidatedAssetCall {
                call,
                payment,
            }))
        }
    }
}

fn count_inputs(len: usize) -> Result<u64, TransactionConsensusError> {
    u64::try_from(len).map_err(|_| TransactionConsensusError::Burn(BurnError::WeightOverflow))
}

fn count_coin_outputs(
    outputs: &[CoinOutput],
    miner_fee: Zeno,
) -> Result<u64, TransactionConsensusError> {
    created_coin_output_count(outputs)?
        .checked_add(u64::from(!miner_fee.is_zero()))
        .ok_or(TransactionConsensusError::Burn(BurnError::WeightOverflow))
}

fn coin_parts(
    intent: &SpendIntent,
) -> Result<(&[CoinShare], &[CoinOutput]), TransactionConsensusError> {
    intent.coin_parts().ok_or(TransactionConsensusError::Intent(
        IntentError::InvalidAssetCall,
    ))
}

fn validate_required_burn(
    actual: Zeno,
    transition: StateTransitionWeight,
    canonical_transaction_weight: u64,
) -> Result<(), TransactionConsensusError> {
    let required =
        ProtocolBurn::for_transaction(transition, canonical_transaction_weight)?.total()?;

    validate_exact_burn(actual, required)?;
    Ok(())
}

fn prepare_asset_intent(
    authorized: AuthorizedAccountIntent<AssetIntent>,
    chain: ChainContext,
) -> Result<AuthorizationValidated<AssetIntent>, TransactionConsensusError> {
    //
    // Signature verification already happened at the transaction-level gate.
    // This commitment is the semantic commitment used by ledger object IDs,
    // NOT an authorization commitment.
    //
    authorized
        .intent
        .validate_structure()
        .map_err(TransactionConsensusError::Asset)?;

    let commitment = SpendIntentCommitment::from_bytes(
        authorized
            .intent
            .semantic_commitment(chain.genesis_hash)
            .map_err(TransactionConsensusError::Asset)?,
    );

    Ok(AuthorizationValidated {
        intent: authorized.intent,
        commitment,
    })
}

fn prepare_spend_intent(
    authorized: AuthorizedAccountIntent<SpendIntent>,
    chain: ChainContext,
) -> Result<AuthorizationValidated<SpendIntent>, TransactionConsensusError> {
    //
    // Signature verification already happened at the transaction-level gate.
    // Keep the semantic spend commitment for canonical output derivation.
    //
    let structurally_validated = validate_intent(authorized.intent, chain)?;
    let commitment = structurally_validated.commitment();
    let intent = structurally_validated.into_intent();

    Ok(AuthorizationValidated { intent, commitment })
}

fn validate_coin_inputs(
    inputs: &[CoinShare],
    outputs: &[CoinOutput],
    miner_fee: Zeno,
    signer: Address,
    state: &impl TransactionStateView,
) -> Result<Zeno, TransactionConsensusError> {
    ensure_unique_coin_ids(inputs.iter().copied())?;

    let mut input_total = Zeno::ZERO;

    for id in inputs {
        let input = state
            .coin(*id)
            .ok_or(TransactionConsensusError::UtxoNotFound)?;

        if input.owner != signer {
            return Err(TransactionConsensusError::RecipientMismatch);
        }

        input_total = input_total
            .checked_add(input.amount)
            .ok_or(TransactionConsensusError::ZenoOverflow)?;
    }

    let output_total = outputs.iter().try_fold(Zeno::ZERO, |sum, output| {
        sum.checked_add(output.amount)
            .ok_or(TransactionConsensusError::ZenoOverflow)
    })?;

    input_total
        .checked_sub(output_total)
        .and_then(|value| value.checked_sub(miner_fee))
        .ok_or(TransactionConsensusError::ValueMismatch)
}

fn validate_share_ownership(
    inputs: &[Share],
    signer: Address,
    state: &impl TransactionStateView,
) -> Result<(), TransactionConsensusError> {
    let mut unique = BTreeSet::new();

    for id in inputs {
        if !unique.insert(*id) {
            return Err(TransactionConsensusError::Intent(
                IntentError::DuplicateInput,
            ));
        }

        let share = state
            .asset_share(*id)
            .ok_or(TransactionConsensusError::OwnershipProofMissing)?;

        if share.owner != signer {
            return Err(TransactionConsensusError::RecipientMismatch);
        }
    }

    Ok(())
}

fn ensure_unique_coin_ids(
    ids: impl IntoIterator<Item = CoinShare>,
) -> Result<(), TransactionConsensusError> {
    let mut unique = BTreeSet::new();

    if ids.into_iter().any(|id| !unique.insert(id)) {
        return Err(TransactionConsensusError::Intent(
            IntentError::DuplicateInput,
        ));
    }

    Ok(())
}

#[derive(Debug)]
pub enum TransactionConsensusError {
    Encoding,
    Intent(IntentError),
    InvalidAuthorization,
    SignatureSchemeInactive,
    UtxoNotFound,
    OwnershipProofMissing,
    RecipientMismatch,
    ZenoOverflow,
    ValueMismatch,
    Asset(AssetError),
    Burn(BurnError),
}

impl fmt::Display for TransactionConsensusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encoding => formatter.write_str("transaction encoding failed"),
            Self::Intent(error) => write!(formatter, "invalid transaction intent: {error}"),
            Self::InvalidAuthorization => {
                formatter.write_str("transaction authorization is invalid")
            }
            Self::SignatureSchemeInactive => {
                formatter.write_str("transaction signature scheme is not active at this height")
            }
            Self::UtxoNotFound => formatter.write_str("transaction input UTXO was not found"),
            Self::OwnershipProofMissing => {
                formatter.write_str("transaction input ownership proof is unavailable")
            }
            Self::RecipientMismatch => {
                formatter.write_str("transaction input is not committed to this signer")
            }
            Self::ZenoOverflow => formatter.write_str("transaction amount overflow"),
            Self::ValueMismatch => {
                formatter.write_str("transaction outputs exceed canonical input value")
            }
            Self::Asset(error) => write!(formatter, "invalid monetary asset transaction: {error}"),
            Self::Burn(error) => write!(formatter, "invalid protocol burn: {error}"),
        }
    }
}

impl StdError for TransactionConsensusError {}

impl From<BurnError> for TransactionConsensusError {
    fn from(error: BurnError) -> Self {
        Self::Burn(error)
    }
}

#[cfg(test)]
mod p3e_authorization_gate_tests {
    use super::*;

    use crypto::{
        AccountSignatureScheme, HASH_SIZE, HASH16_SIZE, SigningSeed, address_from_public_key,
    };

    use crate::{
        monetary::{
            asset::Unit,
            coin::{CoinOutput, Zeno},
        },
        transaction::{
            AccountAuthorization, AccountIntent, AuthorizedAssetTransaction, payment_commitment,
        },
    };

    const TEST_HEIGHT: u64 = 0;

    fn seed(tag: u8) -> SigningSeed {
        SigningSeed::new(AccountSignatureScheme::MlDsa44, Box::new([tag; 32]))
    }

    fn signer(seed: &SigningSeed) -> Address {
        address_from_public_key(&seed.public_key())
    }

    fn chain(tag: u8) -> ChainContext {
        ChainContext::new([tag; HASH_SIZE])
    }

    fn coin_intent(seed: &SigningSeed, input_tag: u8, amount: u64) -> SpendIntent {
        let owner = signer(seed);

        SpendIntent::coin(
            owner,
            vec![CoinShare::from_bytes([input_tag; HASH16_SIZE])],
            vec![CoinOutput::new(owner, Zeno::from_zeno(amount))],
        )
        .expect("valid coin fixture")
    }

    fn asset_call_intent(seed: &SigningSeed, nonce: u64) -> AssetIntent {
        let owner = signer(seed);

        AssetIntent::new(
            AssetInstruction::Register {
                name: "P3E".into(),
                max_supply: Unit::from_units(1_000_000),
                initial_mint: Unit::from_units(100),
                mint_authority: owner,
                nonce,
            },
            owner,
        )
    }

    fn authorize_principal<T: AccountIntent>(
        intent: T,
        signer_seed: &SigningSeed,
        chain: ChainContext,
    ) -> AuthorizedAccountIntent<T> {
        let commitment = intent
            .principal_commitment(chain)
            .expect("principal authorization commitment");

        AuthorizedAccountIntent {
            intent,
            authorization: AccountAuthorization {
                public_key: signer_seed.public_key(),
                signature: signer_seed.sign(commitment.as_bytes()),
            },
        }
    }

    fn authorize_asset_call_payment(
        parent: &AssetIntent,
        payment: SpendIntent,
        payer: &SigningSeed,
        chain: ChainContext,
    ) -> AuthorizedAccountIntent<SpendIntent> {
        let commitment = payment_commitment(parent, &payment, chain).unwrap();

        AuthorizedAccountIntent {
            intent: payment,
            authorization: AccountAuthorization {
                public_key: payer.public_key(),
                signature: payer.sign(commitment.as_bytes()),
            },
        }
    }

    #[test]
    fn valid_direct_spend_passes_consensus_authorization_gate() {
        let owner = seed(1);
        let chain = chain(0x11);
        let intent = coin_intent(&owner, 1, 10);

        let transaction =
            AuthorizedTransaction::Spend(Box::new(authorize_principal(intent, &owner, chain)));

        assert!(validate_authorization_gate(&transaction, chain, TEST_HEIGHT).is_ok());
    }

    #[test]
    fn cross_chain_transaction_is_rejected_before_state_validation() {
        let owner = seed(2);
        let chain_a = chain(0x21);
        let chain_b = chain(0x22);
        let intent = coin_intent(&owner, 2, 10);

        let transaction =
            AuthorizedTransaction::Spend(Box::new(authorize_principal(intent, &owner, chain_a)));

        assert!(matches!(
            validate_authorization_gate(&transaction, chain_b, TEST_HEIGHT),
            Err(TransactionConsensusError::InvalidAuthorization)
        ));
    }

    #[test]
    fn direct_spend_signature_cannot_be_used_as_asset_call_payment_in_consensus() {
        let caller = seed(3);
        let payer = seed(4);
        let chain = chain(0x31);

        let call = asset_call_intent(&caller, 7);
        let payment = coin_intent(&payer, 3, 20);

        // Deliberately authorize payment as a principal DirectSpend.
        let wrong_payment = authorize_principal(payment, &payer, chain);

        let transaction = AuthorizedTransaction::Asset(Box::new(AuthorizedAssetTransaction {
            call: authorize_principal(call, &caller, chain),
            payment: wrong_payment,
        }));

        assert!(matches!(
            validate_authorization_gate(&transaction, chain, TEST_HEIGHT),
            Err(TransactionConsensusError::InvalidAuthorization)
        ));
    }

    #[test]
    fn asset_call_payment_cannot_be_detached_to_another_parent_in_consensus() {
        let caller = seed(5);
        let payer = seed(6);
        let chain = chain(0x41);

        let call_a = asset_call_intent(&caller, 1);
        let call_b = asset_call_intent(&caller, 2);
        let payment = coin_intent(&payer, 4, 30);

        let payment_for_a = authorize_asset_call_payment(&call_a, payment, &payer, chain);

        let forged = AuthorizedTransaction::Asset(Box::new(AuthorizedAssetTransaction {
            call: authorize_principal(call_b, &caller, chain),
            payment: payment_for_a,
        }));

        assert!(matches!(
            validate_authorization_gate(&forged, chain, TEST_HEIGHT),
            Err(TransactionConsensusError::InvalidAuthorization)
        ));
    }
}
