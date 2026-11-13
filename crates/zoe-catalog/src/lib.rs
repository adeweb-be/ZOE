//! The catalog mirror (ADR 0059). It keeps the indexes of each registered
//! ZCatalog of a store in memory and answers queries as of the snapshot
//! tid of the client, with the rids, the order and the counts of ZCatalog.
//!
//! - `read`: the reads of the store, the decode of a record, the walk of a
//!   tree.
//! - `base` and `build`: the rows and the structures of a base, and their
//!   build from the committed trees.
//! - `table` and `cell`: the metadata table of a base, and the msgpack
//!   cells of its columns.
//! - `change`: the change sets of the commits, and the read of a commit.
//! - `mirror`: the versions of a mirror, the window and the rebase.
//! - `query`: the clauses and the four sort paths of ZCatalog.
//! - `security`: the `ZoeSecurityIndex` of `zoe.plone`, resolved from the
//!   inherited inputs of each entry.
//! - `plan`: the codec of a plan and of a reply.
//! - `order`: the order clause of a plan, a sort on the `GopipIndex` of
//!   plone.folder over one container, and the read of its order.
//! - `overlay`: the transaction overlay of a query, read over the version
//!   of the query for that query only.
//! - `mirrors`: the mirrors of a store, their registration, their thread
//!   and the memory bound.
//! - `shadow`: the shadow rebuild of the structures that a stale commit
//!   changed, beside the base, and the swap.
//! - `reason` and `counts`: the reasons of a fallback and the counts.
//! - `text`, with the feature `text`: the native text index of each
//!   registered catalog, on tantivy (ADR 0060).
//! - `testing`, with the feature `testing`: the records of a catalog as
//!   Products.ZCatalog writes them, and the deltas and the settings of its
//!   text indexes, for tests.
//!
//! The crate is synchronous. It starts its threads when a store with a
//! registered catalog starts its mirrors, never at the load of a module.

pub mod base;
pub mod build;
pub mod cell;
pub mod change;
pub mod counts;
pub mod mirror;
pub mod mirrors;
pub mod order;
pub mod overlay;
pub mod plan;
pub mod query;
pub mod read;
pub mod reason;
pub mod security;
pub mod shadow;
pub mod table;
#[cfg(feature = "testing")]
pub mod testing;
#[cfg(feature = "text")]
pub mod text;
pub mod value;

pub use mirrors::{Config, Hooks, Mirrors, Opener, Status};
pub use order::{Order, OrderList};
#[cfg(feature = "text")]
pub use plan::split_text;
pub use plan::{
    ClauseKind, PlanError, Wire, decode_ordered, decode_plan, decode_request, decode_typed,
    encode_reply,
};
pub use query::{Clause, Columns, Plan, Reply};
pub use read::{NAMESPACE, Reads};
pub use reason::{Fallback, Reason};
