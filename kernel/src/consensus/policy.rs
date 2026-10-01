use std::{error::Error as StdError, fmt};

use static_assertions::const_assert;

use crate::{
    blockchain::Block,
    common::Height,
    consensus::PoWTarget,
    monetary::coin::{CoinOutput, Zeno},
};

use crypto::{ADDRESS_SIZE, Address, HASH_SIZE, Hash, HashDomain, canonical_bytes, domain};

pub const WBDA_WINDOW: usize = 10_000;
pub const WBDA_TARGET_BLOCK_WEIGHT: usize = 1 * 1024 * 1024;
pub const WBDA_LOW_UTILIZATION_PPM: u64 = 800_000;
pub const WBDA_HIGH_UTILIZATION_PPM: u64 = 1_200_000;
pub const WBDA_HARDER_PERCENT: u32 = 80;
pub const WBDA_EASIER_PERCENT: u32 = 120;
pub const DIFFICULTY_ALGORITHM: &str = "argon2id-wbda-algorithm";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WbdaAdjustment {
    Decrease,
    Keep,
    Increase,
}

pub const fn is_wbda_epoch_boundary(height: u64) -> bool {
    height > 1 && (height - 1).is_multiple_of(WBDA_WINDOW as u64)
}

pub fn average_block_weight(block_weights: &[usize]) -> Option<u64> {
    if block_weights.len() != WBDA_WINDOW {
        return None;
    }

    let total = block_weights
        .iter()
        .try_fold(0_u64, |total, weight| total.checked_add(*weight as u64))?;

    Some(total / WBDA_WINDOW as u64)
}

pub fn utilization_ppm(block_weights: &[usize]) -> Option<u64> {
    let average = average_block_weight(block_weights)?;
    let target = WBDA_TARGET_BLOCK_WEIGHT as u64;

    if target == 0 {
        return None;
    }

    Some(average.saturating_mul(1_000_000) / target)
}

pub fn adjustment_for_utilization_ppm(utilization: u64) -> WbdaAdjustment {
    if utilization < WBDA_LOW_UTILIZATION_PPM {
        WbdaAdjustment::Increase
    } else if utilization > WBDA_HIGH_UTILIZATION_PPM {
        WbdaAdjustment::Decrease
    } else {
        WbdaAdjustment::Keep
    }
}

pub fn adjustment_for_window(block_weights: &[usize]) -> Option<WbdaAdjustment> {
    utilization_ppm(block_weights).map(adjustment_for_utilization_ppm)
}

pub fn next_difficulty_from_window(
    previous_target_bits: u32,
    block_weights: &[usize],
) -> Option<u32> {
    let adjustment = adjustment_for_window(block_weights)?;

    let previous = PoWTarget::from_compact(previous_target_bits)?;

    let pow_limit = PoWTarget::from_compact(crate::consensus::TARGET_BITS_START)?;

    let next = match adjustment {

        WbdaAdjustment::Decrease => previous.scale_ratio(WBDA_EASIER_PERCENT, 100)?,

        WbdaAdjustment::Keep => previous,

        WbdaAdjustment::Increase => previous.scale_ratio(WBDA_HARDER_PERCENT, 100)?,
    };

    let next = if next > pow_limit { pow_limit } else { next };

    Some(next.to_compact())
}

pub fn expected_difficulty_from_window(
    next_height: u64,
    parent_difficulty: u32,
    current_window: &[usize],
) -> Option<u32> {
    if next_height == 1 {
        return Some(crate::consensus::TARGET_BITS_START);
    }

    if !is_wbda_epoch_boundary(next_height) {
        return Some(parent_difficulty);
    }

    next_difficulty_from_window(parent_difficulty, current_window)
}

pub fn expected_difficulty_for_height<E>(
    next_height: u64,
    parent_difficulty: u32,
    mut weight_at: impl FnMut(u64) -> Result<usize, E>,
) -> Result<Option<u32>, E> {
    if next_height == 1 {
        return Ok(Some(crate::consensus::TARGET_BITS_START));
    }

    if !is_wbda_epoch_boundary(next_height) {
        return Ok(Some(parent_difficulty));
    }

    let start = next_height - WBDA_WINDOW as u64;

    let weights = (start..next_height)
        .map(&mut weight_at)
        .collect::<Result<Vec<_>, _>>()?;

    Ok(expected_difficulty_from_window(
        next_height,
        parent_difficulty,
        &weights,
    ))
}

pub const BLOCK_EMISSION_START: u64 = 156_250_000; // 1.562500 XPQ
pub const MAX_BLOCK_EMISSION: u64 = 5_000_000_000; // 50 XPQ
pub const TAIL_BLOCK_EMISSION: u64 = 78_125_000; // 0.781250 XPQ
pub const EMISSION_RISING_STEPS: u64 = 5;
pub const EMISSION_HALVINGS_TO_TAIL: u64 = 6;

pub const EMISSION_INTERVAL: u64 = 100_000;

const_assert!(BLOCK_EMISSION_START * (1_u64 << EMISSION_RISING_STEPS) == MAX_BLOCK_EMISSION);

const_assert!(MAX_BLOCK_EMISSION / (1_u64 << EMISSION_HALVINGS_TO_TAIL) == TAIL_BLOCK_EMISSION);

pub const fn initial_block_emission() -> Zeno {
    Zeno::from_zeno(BLOCK_EMISSION_START)
}

pub const fn is_emission_epoch_boundary(height: u64) -> bool {
    height > 1 && (height - 1).is_multiple_of(EMISSION_INTERVAL)
}

pub fn block_emission_for_height(height: Height) -> Zeno {
    let completed_intervals = height.0.saturating_sub(1) / EMISSION_INTERVAL;

    let emission = if completed_intervals <= EMISSION_RISING_STEPS {
        // Reverse halving / doubling phase:
        //
        // 1.5625
        // 3.125
        // 6.25
        // 12.5
        // 25
        // 50
        let multiplier = 1_u64 << completed_intervals;

        BLOCK_EMISSION_START
            .saturating_mul(multiplier)
            .min(MAX_BLOCK_EMISSION)
    } else {
        // Normal halving phase after peak.
        let halvings = completed_intervals - EMISSION_RISING_STEPS;

        if halvings >= EMISSION_HALVINGS_TO_TAIL {
            TAIL_BLOCK_EMISSION
        } else {
            (MAX_BLOCK_EMISSION >> halvings).max(TAIL_BLOCK_EMISSION)
        }
    };

    Zeno::from_zeno(emission)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValidatedEmission {
    recipient: Address,
    subsidy: Zeno,
    miner_emission: Zeno,
    protocol_burn: Zeno,
    origin: Hash,
}

impl ValidatedEmission {
    pub const fn recipient(self) -> Address {
        self.recipient
    }

    pub const fn subsidy(self) -> Zeno {
        self.subsidy
    }

    pub const fn miner_emission(self) -> Zeno {
        self.miner_emission
    }

    pub const fn protocol_burn(self) -> Zeno {
        self.protocol_burn
    }

    pub const fn origin(self) -> Hash {
        self.origin
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EmissionError {
    MissingEmission,
    InvalidSubsidy,
    Serialization,
}

impl fmt::Display for EmissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingEmission => f.write_str("block emission is missing"),
            Self::InvalidSubsidy => f.write_str("block emission subsidy is invalid"),
            Self::Serialization => f.write_str("emission encoding failed"),
        }
    }
}

impl StdError for EmissionError {}

pub fn validate_emission(block: &Block) -> Result<ValidatedEmission, EmissionError> {
    authorize_emission(block)
}

/// Derive the emission UTXO origin from block body fields, independent of
/// subsidy validation. Canonical block admission validates the subsidy.
pub fn emission_origin(block: &Block) -> Result<Hash, EmissionError> {
    let emission = block.emission().ok_or(EmissionError::MissingEmission)?;
    let bytes = canonical_bytes(&(
        b"emission",
        block.previous_hash(),
        block.height(),
        emission.to,
        emission.subsidy,
    ))
    .map_err(|_| EmissionError::Serialization)?;
    Ok(domain(HashDomain::Emission, &bytes))
}

pub(crate) fn authorize_emission(block: &Block) -> Result<ValidatedEmission, EmissionError> {
    let emission = block.emission().ok_or(EmissionError::MissingEmission)?;

    let expected = block_emission_for_height(block.height());

    if emission.subsidy != expected {
        return Err(EmissionError::InvalidSubsidy);
    }

    let protocol_burn = MINER_PROTOCOL_BURN;

    let miner_emission = emission
        .subsidy
        .checked_sub(protocol_burn)
        .ok_or(EmissionError::InvalidSubsidy)?;

    let origin = emission_origin(block)?;

    Ok(ValidatedEmission {
        recipient: emission.to,
        subsidy: emission.subsidy,
        miner_emission,
        protocol_burn,
        origin,
    })
}

pub fn expected_emission_for_height(height: Height) -> Zeno {
    block_emission_for_height(height)
}

pub const STATE_BURN_ALGORITHM: &str = "xparq-canonical-archival-and-net-coin-state-growth-burn";
pub const STATE_BURN_RATE_ZENO_PER_WEIGHT: u64 = 8;

const BORSH_OPTION_TAG_BYTES: usize = 1;
const BORSH_VEC_LENGTH_BYTES: usize = core::mem::size_of::<u32>();

pub const EMPTY_BLOCK_ARCHIVAL_BYTES: u64 = (3 * HASH_SIZE
    + 2 * core::mem::size_of::<u32>()
    + core::mem::size_of::<u64>()
    + core::mem::size_of::<u64>()
    + BORSH_OPTION_TAG_BYTES
    + ADDRESS_SIZE
    + core::mem::size_of::<u64>()
    + BORSH_VEC_LENGTH_BYTES) as u64;

/// Canonical coin UTXO: XPQ key + amount + owner.
pub const COIN_UTXO_STATE_WEIGHT: u64 =
    (crate::monetary::coin::CoinShare::SIZE + core::mem::size_of::<u64>() + ADDRESS_SIZE) as u64;

pub const EMISSION_UTXO_STATE_GROWTH_BURN: Zeno =
    Zeno::from_zeno(COIN_UTXO_STATE_WEIGHT * STATE_BURN_RATE_ZENO_PER_WEIGHT);

pub const EMPTY_BLOCK_ARCHIVAL_BURN: Zeno =
    Zeno::from_zeno(EMPTY_BLOCK_ARCHIVAL_BYTES * STATE_BURN_RATE_ZENO_PER_WEIGHT);

pub const MINER_PROTOCOL_BURN: Zeno = Zeno::from_zeno(
    (EMPTY_BLOCK_ARCHIVAL_BYTES + COIN_UTXO_STATE_WEIGHT) * STATE_BURN_RATE_ZENO_PER_WEIGHT,
);

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateTransitionWeight {
    pub created_coin_utxos: u64,
    pub consumed_coin_utxos: u64,
    pub created_state_weight: u64,
}

impl StateTransitionWeight {
    pub fn state_growth_burn(self) -> Result<Zeno, BurnError> {
        let net_coin_utxos = self
            .created_coin_utxos
            .saturating_sub(self.consumed_coin_utxos);

        let created = net_coin_utxos
            .checked_mul(COIN_UTXO_STATE_WEIGHT)
            .and_then(|weight| weight.checked_add(self.created_state_weight))
            .ok_or(BurnError::WeightOverflow)?;

        let burn = created
            .checked_mul(STATE_BURN_RATE_ZENO_PER_WEIGHT)
            .ok_or(BurnError::ZenoOverflow)?;

        Ok(Zeno::from_zeno(burn))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProtocolBurn {
    pub archival: Zeno,
    pub state_growth: Zeno,
}

impl ProtocolBurn {
    pub fn for_transaction(
        transition: StateTransitionWeight,
        canonical_transaction_bytes: u64,
    ) -> Result<Self, BurnError> {
        let archival = canonical_transaction_bytes
            .checked_mul(STATE_BURN_RATE_ZENO_PER_WEIGHT)
            .ok_or(BurnError::ZenoOverflow)?;

        Ok(Self {
            archival: Zeno::from_zeno(archival),
            state_growth: transition.state_growth_burn()?,
        })
    }

    pub fn total(self) -> Result<Zeno, BurnError> {
        self.archival
            .checked_add(self.state_growth)
            .ok_or(BurnError::ZenoOverflow)
    }
}

pub fn created_coin_output_count(outputs: &[CoinOutput]) -> Result<u64, BurnError> {
    u64::try_from(outputs.len()).map_err(|_| BurnError::WeightOverflow)
}

pub fn validate_exact_burn(actual: Zeno, required: Zeno) -> Result<(), BurnError> {
    if actual != required {
        return Err(BurnError::IncorrectBurn {
            required: required.as_zeno(),
            actual: actual.as_zeno(),
        });
    }

    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BurnError {
    WeightOverflow,
    ZenoOverflow,
    IncorrectBurn { required: u64, actual: u64 },
}

impl fmt::Display for BurnError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WeightOverflow => formatter.write_str("state transition weight overflow"),

            Self::ZenoOverflow => formatter.write_str("protocol burn Zeno overflow"),

            Self::IncorrectBurn { required, actual } => {
                write!(
                    formatter,
                    "incorrect protocol burn: \
                     required {required} zeno, \
                     actual {actual} zeno"
                )
            }
        }
    }
}

impl StdError for BurnError {}
