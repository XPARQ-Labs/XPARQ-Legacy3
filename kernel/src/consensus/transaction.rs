use std::{collections::BTreeSet, error::Error as StdError, fmt};

use crypto::{Address, canonical_bytes};

use crate::{
    common::ChainContext,
    consensus::{
        BurnError, ProtocolBurn, StateTransitionWeight, created_coin_output_count,
        validate_exact_burn,
    },
    monetary::coin::{CoinOutput, CoinShare, Zeno},
    transaction::{
        AuthorizedAccountIntent, AuthorizedTransaction, IntentError, MAX_TRANSACTION_SIZE, Spend,
        SpendIntent, SpendIntentCommitment, Transaction as OnChainTransaction,
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
    Program(crate::program::PreparedProgramTransaction),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValidatedCoinSpend {
    pub spend: AuthorizationValidated<SpendIntent>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CoinInputState {
    pub amount: Zeno,
    pub owner: Address,
}

pub trait TransactionStateView {
    fn extension_state(&self) -> Option<&extension::script::state::ExtensionState> {
        None
    }

    fn coin(&self, id: CoinShare) -> Option<CoinInputState>;
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
    // Verify each transaction's complete authorization before state checks.
    //
    transaction
        .validate_structure()
        .map_err(TransactionConsensusError::Intent)?;
    let transaction_size = canonical_bytes(&transaction)
        .map_err(|_| TransactionConsensusError::Encoding)?
        .len();
    if transaction_size > MAX_TRANSACTION_SIZE {
        return Err(TransactionConsensusError::TransactionTooLarge);
    }
    let canonical_transaction_weight = u64::try_from(transaction_size)
        .map_err(|_| TransactionConsensusError::Burn(BurnError::WeightOverflow))?;

    validate_authorization_gate(&transaction, chain, current_height)?;

    if let AuthorizedTransaction::Program(tx) = transaction {
        return crate::program::prepare_program_transaction(*tx, chain, current_height, state)
            .map(ValidatedTransaction::Program);
    }
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
            }
        }
        AuthorizedTransaction::Program(_) => {
            unreachable!("Program envelopes are validated before dispatch")
        }
    }
}

fn count_inputs(len: usize) -> Result<u64, TransactionConsensusError> {
    u64::try_from(len).map_err(|_| TransactionConsensusError::Burn(BurnError::WeightOverflow))
}

pub(crate) fn count_coin_outputs(
    outputs: &[CoinOutput],
    miner_fee: Zeno,
) -> Result<u64, TransactionConsensusError> {
    created_coin_output_count(outputs)?
        .checked_add(u64::from(!miner_fee.is_zero()))
        .ok_or(TransactionConsensusError::Burn(BurnError::WeightOverflow))
}

pub(crate) fn validate_required_burn(
    actual: Zeno,
    transition: StateTransitionWeight,
    canonical_transaction_weight: u64,
) -> Result<(), TransactionConsensusError> {
    let required =
        ProtocolBurn::for_transaction(transition, canonical_transaction_weight)?.total()?;

    validate_exact_burn(actual, required)?;
    Ok(())
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

pub(crate) fn validate_coin_inputs(
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
    TransactionTooLarge,
    Intent(IntentError),
    InvalidAuthorization,
    SignatureSchemeInactive,
    UtxoNotFound,
    RecipientMismatch,
    ZenoOverflow,
    ValueMismatch,
    Burn(BurnError),
}

impl fmt::Display for TransactionConsensusError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Encoding => formatter.write_str("transaction encoding failed"),
            Self::TransactionTooLarge => {
                formatter.write_str("transaction exceeds consensus size limit")
            }
            Self::Intent(error) => write!(formatter, "invalid transaction intent: {error}"),
            Self::InvalidAuthorization => {
                formatter.write_str("transaction authorization is invalid")
            }
            Self::SignatureSchemeInactive => {
                formatter.write_str("transaction signature scheme is not active at this height")
            }
            Self::UtxoNotFound => formatter.write_str("transaction input UTXO was not found"),

            Self::RecipientMismatch => {
                formatter.write_str("transaction input is not committed to this signer")
            }
            Self::ZenoOverflow => formatter.write_str("transaction amount overflow"),
            Self::ValueMismatch => {
                formatter.write_str("transaction outputs exceed canonical input value")
            }
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
        monetary::coin::{CoinOutput, Zeno},
        transaction::{AccountAuthorization, AccountIntent},
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
}
