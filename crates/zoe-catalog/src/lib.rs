//! The catalog mirror. It keeps the ZCatalog indexes in memory and answers
//! queries as of the snapshot tid of the client.
//!
//! This is the prototype of a spike. A base holds the indexes, the
//! metadata table and the paths of a catalog at one tid, built from the
//! committed trees. Each commit after it is a delta. A query reads a
//! version: the base with the patch of the deltas up to its tid, and the
//! overlay of its transaction when it has one.

pub mod base;
pub mod catalog;
pub mod query;
pub mod read;
pub mod value;
pub mod version;
