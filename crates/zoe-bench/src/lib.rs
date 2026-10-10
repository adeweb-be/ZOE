//! A load generator that speaks the ZEO protocol. It measures contention
//! and server saturation.
//!
//! The `conflicts` module ranks the ConflictErrors of Zope logs. The
//! `record_store` module measures the record-store backends.

pub mod conflicts;
pub mod record_store;
