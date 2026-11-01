//! The prefetch of the references of a record that a load fetched from the
//! server.
//!
//! ZODB loads an object when the application touches it, so a cold walk
//! of a container waits for one round trip per child. When the flag of
//! the pool is on, a miss of `Pool::load_before` also reads the persistent
//! ids of its record, and prefetches the objects of the same database that
//! the cache does not hold for the tid of the load, as `Pool::prefetch`
//! does. The children then load while the application unpickles the
//! parent.
//!
//! The record goes to the caller first: the scan and the prefetch run in a
//! task of the runtime. A record that a prefetch brings is not scanned, so
//! the prefetch goes one level deep. Weak references and references into
//! another database are not prefetched. A record that does not decode
//! prefetches nothing.
//!
//! In release, `persistent_ids` costs about 0.8 µs per reference, and 17
//! to 26 µs per KB of a record of small items. Two caps keep a scan and
//! its prefetch small. `PREFETCH_SCAN_LIMIT` holds one scan below about
//! 0.4 ms of a thread of the runtime. `PREFETCH_REFERENCES` bounds the
//! loads of one record, and still covers a bucket of BTrees, a node of an
//! `OOBTree` and a container of a few hundred children.

use std::sync::Arc;

use zoe_core::{Oid, Tid};
use zoe_pickle::persistent_ids;

use super::Shared;

/// The most loads that the references of one record start.
pub const PREFETCH_REFERENCES: usize = 256;

/// The largest record, in bytes, whose references the pool reads.
pub const PREFETCH_SCAN_LIMIT: usize = 16 * 1024;

/// Start a task that prefetches the references of `data`, the record of
/// `oid` that a load before `tid` fetched from the server. Start nothing
/// when the pool has no cache, or when the record is larger than
/// `PREFETCH_SCAN_LIMIT`.
pub(super) fn prefetch_references(shared: &Arc<Shared>, oid: Oid, tid: Tid, data: &[u8]) {
    if shared.cache.is_none() || data.len() > PREFETCH_SCAN_LIMIT {
        return;
    }
    let data = data.to_vec();
    let task = shared.clone();
    shared.runtime.spawn(Box::pin(async move {
        let oids = references(oid, &data);
        // A prefetch that fails changes nothing.
        let _ = task.prefetch(&oids, tid, PREFETCH_REFERENCES);
    }));
}

/// Return the oids of the objects of the same database that `data`, the
/// record of `oid`, refers to, in pickle order. Leave out `oid` itself.
/// Return none for a record that does not decode.
fn references(oid: Oid, data: &[u8]) -> Vec<Oid> {
    let Ok(ids) = persistent_ids(data) else {
        return Vec::new();
    };
    ids.iter()
        .filter_map(|id| id.oid_u64()?.ok())
        .map(Oid)
        .filter(|&found| found != oid)
        .collect()
}
