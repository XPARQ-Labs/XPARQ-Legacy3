use crate::asset_program::state::AssetState;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExtensionState {
    pub assets: AssetState,
}
