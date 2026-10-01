use borsh::{BorshDeserialize, BorshSerialize};

use crate::asset_program::state::AssetState;

#[derive(Debug, Clone, Default, PartialEq, Eq, BorshSerialize, BorshDeserialize)]
pub struct ExtensionState {
    pub assets: AssetState,
}
