use borsh::{BorshDeserialize, BorshSerialize};
use std::io::{Error, ErrorKind, Read};

use super::opcode::ProgramError;

pub const MAX_PROGRAM_PAYLOAD_SIZE: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ProgramId(pub u32);

impl ProgramId {
    pub const XPQ: Self = Self(0);
    pub const ASSET: Self = Self(1);
}

#[derive(Debug, Clone, PartialEq, Eq, BorshSerialize)]
pub struct ProgramCall {
    pub program: ProgramId,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

impl BorshDeserialize for ProgramCall {
    fn deserialize_reader<R: Read>(reader: &mut R) -> std::io::Result<Self> {
        let program = ProgramId::deserialize_reader(reader)?;
        let opcode = u8::deserialize_reader(reader)?;
        let length = u32::deserialize_reader(reader)? as usize;
        if length > MAX_PROGRAM_PAYLOAD_SIZE {
            return Err(Error::new(
                ErrorKind::InvalidData,
                "program payload too large",
            ));
        }
        let mut payload = vec![0; length];
        reader.read_exact(&mut payload)?;
        Ok(Self {
            program,
            opcode,
            payload,
        })
    }
}

/// Decode exactly one canonical call. The length is checked before allocating the payload.
pub fn decode_program_call(bytes: &[u8]) -> Result<ProgramCall, ProgramError> {
    let mut reader = bytes;
    ProgramId::deserialize_reader(&mut reader).map_err(|_| ProgramError::InvalidPayload)?;
    u8::deserialize_reader(&mut reader).map_err(|_| ProgramError::InvalidPayload)?;
    let length = u32::deserialize_reader(&mut reader).map_err(|_| ProgramError::InvalidPayload)?;
    if length as usize > MAX_PROGRAM_PAYLOAD_SIZE {
        return Err(ProgramError::PayloadTooLarge);
    }
    ProgramCall::try_from_slice(bytes).map_err(|_| ProgramError::InvalidPayload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_envelope_roundtrip_and_invalid_lengths() {
        let call = ProgramCall {
            program: ProgramId::ASSET,
            opcode: 2,
            payload: vec![7; MAX_PROGRAM_PAYLOAD_SIZE],
        };
        let bytes = borsh::to_vec(&call).unwrap();
        assert_eq!(decode_program_call(&bytes), Ok(call));
        assert_eq!(
            ProgramCall::try_from_slice(&bytes).unwrap().payload.len(),
            MAX_PROGRAM_PAYLOAD_SIZE
        );

        let mut oversized = bytes[..9].to_vec();
        oversized[5..9].copy_from_slice(&((MAX_PROGRAM_PAYLOAD_SIZE + 1) as u32).to_le_bytes());
        assert_eq!(
            decode_program_call(&oversized),
            Err(ProgramError::PayloadTooLarge)
        );
        assert!(ProgramCall::try_from_slice(&oversized).is_err());

        assert_eq!(
            decode_program_call(&bytes[..bytes.len() - 1]),
            Err(ProgramError::InvalidPayload)
        );
        let mut trailing = bytes;
        trailing.push(0);
        assert_eq!(
            decode_program_call(&trailing),
            Err(ProgramError::InvalidPayload)
        );
    }
}
