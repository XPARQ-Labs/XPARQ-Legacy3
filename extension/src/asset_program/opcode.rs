use crate::script::opcode::ProgramError;

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssetOpcode {
    Register = 0x01,
    Mint = 0x02,
    Transfer = 0x03,
    Burn = 0x04,
}

pub const MAX_ASSET_INPUTS: usize = 256;
pub const MAX_ASSET_OUTPUTS: usize = 256;

impl AssetOpcode {
    pub const fn max_payload_size(self) -> usize {
        match self {
            Self::Register => 4 * 1024,
            Self::Mint => 1024,
            Self::Transfer => 64 * 1024,
            Self::Burn => 32 * 1024,
        }
    }
}

impl TryFrom<u8> for AssetOpcode {
    type Error = ProgramError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0x01 => Ok(Self::Register),
            0x02 => Ok(Self::Mint),
            0x03 => Ok(Self::Transfer),
            0x04 => Ok(Self::Burn),
            _ => Err(ProgramError::UnknownOpcode),
        }
    }
}
