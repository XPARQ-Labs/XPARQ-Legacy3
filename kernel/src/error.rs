//! Central error definitions for the kernel crate.

pub use blockchain_errors::*;

mod blockchain_errors {
    pub use crypto::CodecError;

    use std::{error::Error as StdError, fmt};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum BlockError {
        MissingEmission,
        UnexpectedEmission,
        BlockTooHeavy,
        InvalidTransaction,
        DuplicateTransaction,
        InvalidEmission,
        InvalidMerkleRoot,
        InvalidStateRoot,
        InvalidBlockWeight,
        Serialization(CodecError),
    }

    impl fmt::Display for BlockError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::MissingEmission => f.write_str("non-genesis block must contain emission"),
                Self::UnexpectedEmission => f.write_str("genesis block must not contain emission"),
                Self::BlockTooHeavy => f.write_str("block serialized weight exceeds limit"),
                Self::InvalidTransaction => f.write_str("block contains an invalid transaction"),
                Self::DuplicateTransaction => f.write_str("block contains a duplicate transaction"),
                Self::InvalidEmission => f.write_str("block emission is invalid"),
                Self::InvalidMerkleRoot => {
                    f.write_str("block merkle root does not match transactions")
                }
                Self::InvalidStateRoot => f.write_str("block state root does not match ledger"),
                Self::InvalidBlockWeight => {
                    f.write_str("block header weight does not cover canonical block size")
                }
                Self::Serialization(error) => write!(f, "block encoding failed: {error}"),
            }
        }
    }

    impl StdError for BlockError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            match self {
                Self::Serialization(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<CodecError> for BlockError {
        fn from(error: CodecError) -> Self {
            Self::Serialization(error)
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ChainError {
        DuplicateBlock,
        InvalidHeight,
        InvalidParent,
        MissingBody,
        Serialization(CodecError),
    }

    impl fmt::Display for ChainError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::DuplicateBlock => f.write_str("block height already exists"),
                Self::InvalidHeight => f.write_str("block height does not extend chain tip"),
                Self::InvalidParent => f.write_str("block parent does not match chain tip"),
                Self::MissingBody => f.write_str("full block body is not retained"),
                Self::Serialization(error) => write!(f, "block encoding failed: {error}"),
            }
        }
    }

    impl StdError for ChainError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            match self {
                Self::Serialization(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<CodecError> for ChainError {
        fn from(error: CodecError) -> Self {
            Self::Serialization(error)
        }
    }
}

pub use consensus_errors::*;

mod consensus_errors {
    use std::{error::Error as StdError, fmt};

    use crate::{blockchain::BlockError, consensus::EmissionError};

    #[derive(Debug)]
    pub enum ConsensusError {
        InvalidBlock(BlockError),
        InvalidEmission(EmissionError),
        InvalidDifficulty,
        UnexpectedDifficulty,
        InvalidPoWParameters,
        PoWHashFailed,
        InvalidHeight,
        InvalidPreviousHash,
        GenesisRequired,
        WrongGenesis,
        InsufficientPoW,
        Serialization,
    }

    impl fmt::Display for ConsensusError {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::InvalidBlock(error) => write!(f, "invalid block: {error}"),
                Self::InvalidEmission(error) => write!(f, "invalid emission: {error}"),
                Self::InvalidDifficulty => f.write_str("difficulty is outside allowed range"),
                Self::UnexpectedDifficulty => {
                    f.write_str("block difficulty does not match expected difficulty")
                }
                Self::InvalidPoWParameters => f.write_str("proof-of-work parameters are invalid"),
                Self::PoWHashFailed => f.write_str("proof-of-work hash failed"),
                Self::InvalidHeight => f.write_str("block height does not extend tip"),
                Self::InvalidPreviousHash => f.write_str("block previous hash does not match tip"),
                Self::GenesisRequired => {
                    f.write_str("canonical chain must be initialized through validated genesis")
                }
                Self::WrongGenesis => {
                    f.write_str("genesis block does not match configured chain identity")
                }
                Self::InsufficientPoW => {
                    f.write_str("block hash does not satisfy proof-of-work difficulty")
                }
                Self::Serialization => f.write_str("consensus encoding failed"),
            }
        }
    }

    impl StdError for ConsensusError {
        fn source(&self) -> Option<&(dyn StdError + 'static)> {
            match self {
                Self::InvalidBlock(error) => Some(error),
                Self::InvalidEmission(error) => Some(error),
                _ => None,
            }
        }
    }

    impl From<EmissionError> for ConsensusError {
        fn from(error: EmissionError) -> Self {
            Self::InvalidEmission(error)
        }
    }

    impl From<BlockError> for ConsensusError {
        fn from(error: BlockError) -> Self {
            Self::InvalidBlock(error)
        }
    }
}

pub use ledger_errors::*;

mod ledger_errors {
    //! Ledger state-transition errors.

    use crate::ledger::utxo;
    use crate::monetary::asset::AssetError;
    use std::{error::Error as StdError, fmt};

    #[derive(Debug, Clone, PartialEq, Eq)]
    pub enum StateError {
        Utxo(utxo::Error),
        Asset(AssetError),
        InvalidTransaction,
        OutputIndexOverflow,
        BurnOverflow,
        BurnUnderflow,
        AmountOverflow,
    }

    impl fmt::Display for StateError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Utxo(error) => write!(formatter, "UTXO transition failed: {error}"),
                Self::Asset(error) => write!(formatter, "asset transition failed: {error}"),
                Self::InvalidTransaction => {
                    formatter.write_str("invalid transaction state transition")
                }
                Self::OutputIndexOverflow => {
                    formatter.write_str("transaction output index overflow")
                }
                Self::BurnOverflow => formatter.write_str("total burned amount overflow"),
                Self::BurnUnderflow => formatter.write_str("total burned amount underflow"),
                Self::AmountOverflow => formatter.write_str("coin amount overflow"),
            }
        }
    }
    impl StdError for StateError {}
    impl From<utxo::Error> for StateError {
        fn from(error: utxo::Error) -> Self {
            Self::Utxo(error)
        }
    }
    impl From<AssetError> for StateError {
        fn from(error: AssetError) -> Self {
            Self::Asset(error)
        }
    }
}

pub use transaction_errors::*;

mod transaction_errors {
    use std::{error::Error as StdError, fmt};

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum IntentError {
        EmptyInputs,
        EmptyOutputs,
        ZeroAmount,
        DuplicateInput,
        TooManyItems,
        InvalidAssetCall,
        Encoding,
    }

    impl fmt::Display for IntentError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::EmptyInputs => formatter.write_str("intent has no inputs"),
                Self::EmptyOutputs => formatter.write_str("intent has no outputs"),
                Self::ZeroAmount => formatter.write_str("intent contains a zero amount"),
                Self::DuplicateInput => formatter.write_str("intent contains a duplicate input"),
                Self::TooManyItems => formatter.write_str("intent list exceeds consensus limit"),
                Self::InvalidAssetCall => formatter.write_str("asset call is structurally invalid"),
                Self::Encoding => formatter.write_str("intent encoding failed"),
            }
        }
    }

    impl StdError for IntentError {}

    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum TransactionEncodingError {
        Encoding,
    }

    impl fmt::Display for TransactionEncodingError {
        fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self {
                Self::Encoding => formatter.write_str("transaction encoding failed"),
            }
        }
    }

    impl StdError for TransactionEncodingError {}
}
