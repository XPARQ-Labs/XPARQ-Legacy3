pub mod asset;
pub mod opcode;
pub mod state;
#[path = "type.rs"]
pub mod type_;

use borsh::BorshDeserialize;

use crate::script::opcode::ProgramError;
use opcode::{AssetOpcode, MAX_ASSET_INPUTS, MAX_ASSET_OUTPUTS};
use type_::AssetCall;

pub fn decode(opcode: u8, payload: &[u8]) -> Result<AssetCall, ProgramError> {
    let opcode = AssetOpcode::try_from(opcode)?;
    if payload.len() > opcode.max_payload_size() {
        return Err(ProgramError::PayloadTooLarge);
    }
    let call = match opcode {
        AssetOpcode::Register => Register::try_from_slice(payload).map(AssetCall::Register),
        AssetOpcode::Mint => Mint::try_from_slice(payload).map(AssetCall::Mint),
        AssetOpcode::Transfer => return decode_transfer(payload).and_then(validate),
        AssetOpcode::Burn => return decode_burn(payload).and_then(validate),
    }
    .map_err(|_| ProgramError::InvalidPayload)?;
    validate(call)
}

use type_::{Burn, Mint, Register, Transfer};

fn bounded_vec<T: BorshDeserialize>(
    reader: &mut &[u8],
    max: usize,
    excess: ProgramError,
) -> Result<Vec<T>, ProgramError> {
    let count = u32::deserialize_reader(reader).map_err(|_| ProgramError::InvalidPayload)? as usize;
    if count > max {
        return Err(excess);
    }
    (0..count)
        .map(|_| T::deserialize_reader(reader).map_err(|_| ProgramError::InvalidPayload))
        .collect()
}

fn decode_transfer(payload: &[u8]) -> Result<AssetCall, ProgramError> {
    let mut reader = payload;
    let asset = asset::AssetContract::deserialize_reader(&mut reader)
        .map_err(|_| ProgramError::InvalidPayload)?;
    let inputs = bounded_vec(&mut reader, MAX_ASSET_INPUTS, ProgramError::TooManyInputs)?;
    let outputs = bounded_vec(&mut reader, MAX_ASSET_OUTPUTS, ProgramError::TooManyOutputs)?;
    if !reader.is_empty() {
        return Err(ProgramError::InvalidPayload);
    }
    Ok(AssetCall::Transfer(Transfer {
        asset,
        inputs,
        outputs,
    }))
}

fn decode_burn(payload: &[u8]) -> Result<AssetCall, ProgramError> {
    let mut reader = payload;
    let asset = asset::AssetContract::deserialize_reader(&mut reader)
        .map_err(|_| ProgramError::InvalidPayload)?;
    let inputs = bounded_vec(&mut reader, MAX_ASSET_INPUTS, ProgramError::TooManyInputs)?;
    let amount =
        asset::Unit::deserialize_reader(&mut reader).map_err(|_| ProgramError::InvalidPayload)?;
    let output =
        asset::Unit::deserialize_reader(&mut reader).map_err(|_| ProgramError::InvalidPayload)?;
    if !reader.is_empty() {
        return Err(ProgramError::InvalidPayload);
    }
    Ok(AssetCall::Burn(Burn {
        asset,
        inputs,
        amount,
        output,
    }))
}

fn validate_inputs(inputs: &[asset::Share]) -> Result<(), ProgramError> {
    if inputs.is_empty() {
        return Err(ProgramError::NoInputs);
    }
    if inputs.len() > MAX_ASSET_INPUTS {
        return Err(ProgramError::TooManyInputs);
    }
    asset::ensure_unique_asset_inputs(inputs).map_err(|_| ProgramError::DuplicateInput)
}

fn validate(call: AssetCall) -> Result<AssetCall, ProgramError> {
    match &call {
        AssetCall::Register(register) => {
            asset::validate_asset_name(&register.name)
                .map_err(|_| ProgramError::InvalidMetadata)?;
            if register.max_supply.is_zero() || register.initial_mint.is_zero() {
                return Err(ProgramError::ZeroAmount);
            }
            if register.initial_mint > register.max_supply {
                return Err(ProgramError::AmountOverflow);
            }
        }
        AssetCall::Mint(mint) => {
            if mint.amount.is_zero() {
                return Err(ProgramError::ZeroAmount);
            }
        }
        AssetCall::Transfer(transfer) => {
            validate_inputs(&transfer.inputs)?;
            if transfer.outputs.is_empty() {
                return Err(ProgramError::NoOutputs);
            }
            if transfer.outputs.len() > MAX_ASSET_OUTPUTS {
                return Err(ProgramError::TooManyOutputs);
            }
            let mut total = asset::Unit::ZERO;
            for output in &transfer.outputs {
                if output.amount.is_zero() {
                    return Err(ProgramError::ZeroAmount);
                }
                total = total
                    .checked_add(output.amount)
                    .ok_or(ProgramError::AmountOverflow)?;
            }
        }
        AssetCall::Burn(burn) => {
            validate_inputs(&burn.inputs)?;
            if burn.amount.is_zero() {
                return Err(ProgramError::ZeroAmount);
            }
        }
    }
    Ok(call)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::script::{
        call::{ProgramCall, ProgramId},
        execute::{DecodedProgramCall, decode_program},
    };

    #[test]
    fn decodes_typed_mint_and_rejects_invalid_calls() {
        let mint = Mint {
            asset: asset::AssetContract::from_bytes([1; crypto::HASH_SIZE]),
            nonce: 7,
            recipient: crypto::Address::from_bytes([2; crypto::ADDRESS_SIZE]),
            amount: asset::Unit::from_units(9),
        };
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: AssetOpcode::Mint as u8,
            payload: borsh::to_vec(&mint).unwrap(),
        };
        assert_eq!(
            decode_program(&call),
            Ok(DecodedProgramCall::Asset(AssetCall::Mint(mint)))
        );
        assert_eq!(
            decode_program(&ProgramCall {
                opcode: 0,
                ..call.clone()
            }),
            Err(ProgramError::UnknownOpcode)
        );
        assert_eq!(
            decode_program(&ProgramCall {
                payload: vec![1],
                ..call.clone()
            }),
            Err(ProgramError::InvalidPayload)
        );
        assert_eq!(
            decode_program(&ProgramCall {
                program: ProgramId(99),
                ..call
            }),
            Err(ProgramError::UnknownProgram)
        );
    }

    #[test]
    fn rejects_oversize_and_unbounded_vectors_before_decode() {
        assert_eq!(
            decode(AssetOpcode::Mint as u8, &vec![0; 1025]),
            Err(ProgramError::PayloadTooLarge)
        );
        let mut transfer = vec![0; crypto::HASH_SIZE];
        transfer.extend_from_slice(&((MAX_ASSET_INPUTS + 1) as u32).to_le_bytes());
        assert_eq!(
            decode(AssetOpcode::Transfer as u8, &transfer),
            Err(ProgramError::TooManyInputs)
        );
        transfer.truncate(crypto::HASH_SIZE);
        transfer.extend_from_slice(&1_u32.to_le_bytes());
        transfer.extend_from_slice(&[0; crypto::HASH16_SIZE]);
        transfer.extend_from_slice(&((MAX_ASSET_OUTPUTS + 1) as u32).to_le_bytes());
        assert_eq!(
            decode(AssetOpcode::Transfer as u8, &transfer),
            Err(ProgramError::TooManyOutputs)
        );
    }

    #[test]
    fn rejects_zero_output_and_duplicate_inputs() {
        let asset = asset::AssetContract::from_bytes([1; crypto::HASH_SIZE]);
        let share = asset::Share::from_bytes([2; crypto::HASH16_SIZE]);
        let recipient = crypto::Address::from_bytes([3; crypto::ADDRESS_SIZE]);
        let transfer = Transfer {
            asset,
            inputs: vec![share],
            outputs: vec![asset::AssetOutput::new(recipient, asset::Unit::ZERO)],
        };
        assert_eq!(
            decode(
                AssetOpcode::Transfer as u8,
                &borsh::to_vec(&transfer).unwrap()
            ),
            Err(ProgramError::ZeroAmount)
        );
        let transfer = Transfer {
            inputs: vec![share, share],
            outputs: vec![asset::AssetOutput::new(
                recipient,
                asset::Unit::from_units(1),
            )],
            ..transfer
        };
        assert_eq!(
            decode(
                AssetOpcode::Transfer as u8,
                &borsh::to_vec(&transfer).unwrap()
            ),
            Err(ProgramError::DuplicateInput)
        );
    }
}
