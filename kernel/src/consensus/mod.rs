//! XPARQ consensus rules.
//!
//! Consensus is intentionally split by responsibility:
//! - block: canonical block admission/application
//! - transaction: direct authorization/value validation
//! - policy: WBDA, emission, and protocol burn
//! - pow: Argon2id proof of work
//! - fork: fork choice and reorganization planning
//! - header: header-only synchronization validation

mod block;
mod fork;
mod header;
mod policy;
mod pow;
mod target;
pub(crate) mod transaction;

pub use crate::error::ConsensusError;
pub use block::*;
pub use fork::*;
pub use header::*;
pub use policy::*;
pub use pow::*;
pub use transaction::*;

pub use crate::monetary::coin::{DECIMALS, Zeno};

pub use target::{PoWTarget, hash_meets_target};
