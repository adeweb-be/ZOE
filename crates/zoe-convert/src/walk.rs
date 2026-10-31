//! The walks that read the keys of a store a chunk at a time: the record
//! keys, one oid at a time, and the durable rows of the KV namespaces.

use zoe_core::{DurableRow, Oid, Store, Tid};

use crate::Error;

/// The number of record keys that one read of a walk takes.
const KEY_CHUNK: usize = 1024;

/// The number of durable rows that one read of a walk takes.
const ROW_CHUNK: usize = 1024;

/// The record keys of a store, one oid at a time, in ascending oid order.
/// The walk reads `KEY_CHUNK` keys at a time, and no frame. A read of the
/// store gives all the keys of each oid in it, so the keys of an oid are
/// never in two reads.
pub(crate) struct OidVersions<'a> {
    store: &'a Store,
    /// The keys of the last read.
    keys: Vec<(Oid, Tid)>,
    /// The position in `keys` of the first key of the next oid.
    at: usize,
    /// The first oid of the next read, or `None` after the last read.
    from: Option<Oid>,
}

impl<'a> OidVersions<'a> {
    /// Start a walk at the first oid of `store`.
    pub(crate) fn new(store: &'a Store) -> Self {
        Self {
            store,
            keys: Vec::new(),
            at: 0,
            from: Some(Oid(0)),
        }
    }

    /// Return the keys of the next oid, oldest first, deletions too.
    /// Return `None` after the last oid. After an error, the walk ends.
    pub(crate) fn next(&mut self) -> zoe_core::Result<Option<&[(Oid, Tid)]>> {
        if self.at == self.keys.len() {
            let Some(from) = self.from.take() else {
                return Ok(None);
            };
            self.keys = self.store.record_keys(from, KEY_CHUNK)?;
            self.at = 0;
            let Some(&(last, _)) = self.keys.last() else {
                return Ok(None);
            };
            self.from = last.0.checked_add(1).map(Oid);
        }
        let start = self.at;
        let oid = self.keys[start].0;
        let count = self.keys[start..]
            .iter()
            .take_while(|&&(other, _)| other == oid)
            .count();
        self.at += count;
        Ok(Some(&self.keys[start..self.at]))
    }

    /// Return the next oid. Return `None` after the last oid.
    pub(crate) fn next_oid(&mut self) -> zoe_core::Result<Option<Oid>> {
        Ok(self.next()?.and_then(<[_]>::first).map(|&(oid, _)| oid))
    }
}

/// Give each durable row of the KV namespaces of `store` to `each`, in key
/// order, the generation rows included. The walk reads `ROW_CHUNK` rows at
/// a time. The first error stops it.
pub(crate) fn each_durable_row(
    store: &Store,
    mut each: impl FnMut(DurableRow) -> Result<(), Error>,
) -> Result<(), Error> {
    let mut after: Option<Vec<u8>> = None;
    loop {
        let (rows, next) = store.kv_durable_rows(after.as_deref(), ROW_CHUNK)?;
        let last = rows.len() < ROW_CHUNK;
        for row in rows {
            each(row)?;
        }
        if last {
            return Ok(());
        }
        after = next;
    }
}
