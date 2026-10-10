//! A load generator that speaks the ZEO protocol. It measures contention
//! and server saturation.
//!
//! The `conflicts` module ranks the ConflictErrors of Zope logs.

pub mod conflicts;
