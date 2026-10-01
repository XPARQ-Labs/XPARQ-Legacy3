#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgramError {
    UnknownProgram,
    UnknownOpcode,
    InvalidPayload,
    PayloadTooLarge,
    NoInputs,
    TooManyInputs,
    NoOutputs,
    TooManyOutputs,
    DuplicateInput,
    ZeroAmount,
    InvalidMetadata,
    AmountOverflow,
    ExecutionNotImplemented,
}
