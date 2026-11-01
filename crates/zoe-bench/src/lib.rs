//! A load generator that speaks the ZEO protocol. It measures contention
//! and server saturation.
//!
//! - `conflicts` ranks the ConflictErrors of Zope logs.
//! - `hot` runs writers of one hot object, with vote-time or early locks.
//! - `load` runs clients against a ZEO server: `zoe-bench load` and
//!   `zoe-bench saturate`.
//! - `schedule` gives each client its transactions from a seed.
//! - `machine` describes the machine and the run, with no host name, user
//!   name or path.

pub mod conflicts;
pub mod hot;
pub mod load;
pub mod machine;
pub mod schedule;

use zoe_client::Error as ClientError;
use zoe_core::Oid;

use crate::load::MAX_ATTEMPTS;

/// An error of a run of `hot`, `load` or `saturate`.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A call of a client failed with an error that is not a conflict.
    #[error(transparent)]
    Client(#[from] ClientError),
    /// A transaction met a conflict at each of its attempts.
    #[error("a transaction met a conflict {MAX_ATTEMPTS} times")]
    TooManyRetries,
    /// An object of the store has no record at the snapshot of a read.
    #[error("the object {0:?} has no record")]
    Missing(Oid),
}
