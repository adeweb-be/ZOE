//! The ZOE storage core.
//!
//! This crate is synchronous. It has no tokio and no Python inside.
//! Both skins call it: the in-process storage (`zoe-py`) and the server
//! (`zoe-server`). The core owns correctness: the data model, two-phase
//! commit, per-OID locks, conflict handling, pack and blobs. A backend
//! owns bytes, behind the [`Backend`] trait.

mod backend;
mod commit;
mod error;
mod format;
mod lock;
mod types;
pub mod vfs;

pub use backend::{
    Backend, Bytes, PackStats, Record, RecordMeta, TxnIter, TxnMeta, TxnStatus, WriteTxn,
};
pub use error::{Result, ZoeError};
pub use types::{Oid, Tid};
