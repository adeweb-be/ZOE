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
