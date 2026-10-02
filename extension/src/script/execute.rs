use crate::asset_program::{self, type_::AssetCall};

use super::{
    call::{ProgramCall, ProgramId},
    opcode::ProgramError,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodedProgramCall {
    XpqTransfer,
    Asset(AssetCall),
}

/// Decode a call before any state transition. This does not execute it.
pub fn decode_program(call: &ProgramCall) -> Result<DecodedProgramCall, ProgramError> {
    match call.program {
        ProgramId::XPQ => {
            crate::coin_program::decode(call.opcode, &call.payload)?;
            Ok(DecodedProgramCall::XpqTransfer)
        }
        ProgramId::ASSET => {
            asset_program::decode(call.opcode, &call.payload).map(DecodedProgramCall::Asset)
        }
        _ => Err(ProgramError::UnknownProgram),
    }
}
