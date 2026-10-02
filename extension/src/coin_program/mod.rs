//! Native XPQ transfer execution through a kernel-owned coin host.

pub const TRANSFER: u8 = 1;

/// Restricted capability implemented by the kernel. The program never receives
/// the ledger, coin types, or mutable access to monetary counters.
pub trait CoinHost {
    type CoinId: Copy;
    type Error;

    fn input_amount(&self, id: &Self::CoinId) -> Result<u64, Self::Error>;
    fn consume(&mut self, id: Self::CoinId) -> Result<(), Self::Error>;
    fn create(
        &mut self,
        index: u32,
        owner: crypto::Address,
        amount: u64,
    ) -> Result<(), Self::Error>;
    fn burn(&mut self, amount: u64) -> Result<(), Self::Error>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum TransferError<E> {
    Host(E),
    InvalidBalance,
    AmountOverflow,
    OutputIndexOverflow,
}

/// Execute a validated transfer, including funding for other Program calls.
/// The host must validate authorization and provide atomic commit/rollback.
pub fn execute_transfer<H: CoinHost>(
    host: &mut H,
    inputs: &[H::CoinId],
    outputs: &[(crypto::Address, u64)],
    miner: crypto::Address,
    miner_fee: u64,
) -> Result<(), TransferError<H::Error>> {
    let mut input_total = 0u64;
    for id in inputs {
        input_total = input_total
            .checked_add(host.input_amount(id).map_err(TransferError::Host)?)
            .ok_or(TransferError::AmountOverflow)?;
    }
    let output_total = outputs.iter().try_fold(0u64, |sum, (_, amount)| {
        sum.checked_add(*amount)
            .ok_or(TransferError::AmountOverflow)
    })?;
    let burn = input_total
        .checked_sub(output_total)
        .and_then(|remaining| remaining.checked_sub(miner_fee))
        .ok_or(TransferError::InvalidBalance)?;
    for id in inputs {
        host.consume(*id).map_err(TransferError::Host)?;
    }
    for (index, (owner, amount)) in outputs.iter().enumerate() {
        host.create(
            u32::try_from(index).map_err(|_| TransferError::OutputIndexOverflow)?,
            *owner,
            *amount,
        )
        .map_err(TransferError::Host)?;
    }
    if miner_fee != 0 {
        host.create(
            u32::try_from(outputs.len()).map_err(|_| TransferError::OutputIndexOverflow)?,
            miner,
            miner_fee,
        )
        .map_err(TransferError::Host)?;
    }
    host.burn(burn).map_err(TransferError::Host)
}

/// XPQ.Transfer uses the envelope funding field as its transfer data.
/// An empty payload prevents an ambiguous second set of inputs or outputs.
pub fn decode(opcode: u8, payload: &[u8]) -> Result<(), crate::script::opcode::ProgramError> {
    use crate::script::opcode::ProgramError;
    if opcode != TRANSFER {
        return Err(ProgramError::UnknownOpcode);
    }
    if !payload.is_empty() {
        return Err(ProgramError::InvalidPayload);
    }
    Ok(())
}

pub fn transfer_call() -> crate::script::call::ProgramCall {
    crate::script::call::ProgramCall {
        program: crate::script::call::ProgramId::XPQ,
        opcode: TRANSFER,
        payload: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn native_transfer_rejects_unknown_methods_and_duplicate_payload_data() {
        assert!(decode(TRANSFER, &[]).is_ok());
        assert!(decode(TRANSFER, &[0]).is_err());
        assert!(decode(2, &[]).is_err());
    }
}
