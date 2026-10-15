//! A test hook that reads and rewrites the stored bytes of a row, so that
//! a test can damage a frame and check that every read detects it. It
//! also writes and removes a row of `meta`, so that a test can make a
//! store of an older format.
//!
//! The feature `testing` builds this module. Only the dev-dependency of
//! this crate on itself turns the feature on, so only test builds have
//! the hook. A release build never has it.

use crate::error::{Result, ZoeError};
use crate::store::Store;
use crate::types::{Oid, Tid};

/// A row of a store: its key space and its key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// The record of the oid at the tid, in `records`.
    Record(Oid, Tid),
    /// The blob record of the oid at the tid, in `blobs`.
    Blob(Oid, Tid),
    /// The transaction row of the tid, in `txns`.
    Txn(Tid),
}

/// Return the bytes that `store` holds under `key`, as stored. A row that
/// does not exist is a `Backend` error.
pub fn read_frame(store: &Store, key: Key) -> Result<Vec<u8>> {
    store
        .backend
        .read_stored(key)?
        .ok_or_else(|| ZoeError::Backend(format!("no row under {key:?}").into()))
}

/// Store `bytes` under `key` in `store`, in place of the row. The hook
/// checks nothing and changes no other row.
pub fn write_frame(store: &Store, key: Key, bytes: &[u8]) -> Result<()> {
    store.backend.write_stored(key, bytes)
}

/// Store `bytes` in the row `key` of `meta`. The hook checks nothing and
/// changes no other row. The open store keeps the state that it read at
/// its open, so the change counts from the next open.
pub fn write_meta(store: &Store, key: &str, bytes: &[u8]) -> Result<()> {
    store.backend.write_meta_stored(key, Some(bytes))
}

/// Remove the row `key` of `meta`, as `write_meta` changes it. A row that
/// does not exist is a `Backend` error.
pub fn remove_meta(store: &Store, key: &str) -> Result<()> {
    store.backend.write_meta_stored(key, None)
}

/// Return the number of zstd compressions of records that this thread
/// made. The finish of a transaction runs on the thread that calls it
/// when no other finish leads a batch, so a test reads what its finish
/// compressed.
pub fn compressions() -> u64 {
    crate::format::compressions()
}
