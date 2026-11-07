//! The tables of the text deltas in the redb backend (ADR 0060):
//! `text_deltas`, one row for each delta, and `text_txn`, the index of the
//! deltas by tid. A write puts the row and its key of the index in the
//! same write transaction. A store without the tables reads as a store
//! without deltas.

use std::ops::Bound::{self, Excluded, Included, Unbounded};

use redb::{TableDefinition, WriteTransaction};

use super::{Db, backend, open_optional};
use crate::backend::Bytes;
use crate::error::{Result, ZoeError};
use crate::format::{self, Compression, KeySpace, text};
use crate::types::Tid;

/// The rows of the deltas.
pub(crate) const TEXT_DELTAS: TableDefinition<&[u8], &[u8]> = TableDefinition::new("text_deltas");

/// The index of the deltas by tid.
pub(crate) const TEXT_TXN: TableDefinition<&[u8], ()> = TableDefinition::new("text_txn");

/// Tell if `key` is inside the bounds `from` and `to`.
fn inside(key: &[u8], from: Bound<&[u8]>, to: Bound<&[u8]>) -> bool {
    let after = match from {
        Included(from) => key >= from,
        Excluded(from) => key > from,
        Unbounded => true,
    };
    let before = match to {
        Included(to) => key <= to,
        Excluded(to) => key < to,
        Unbounded => true,
    };
    after && before
}

/// Tell if no key can be inside the bounds `from` and `to`. redb panics on
/// a range whose start is after its end.
pub(crate) fn empty_range(from: Bound<&[u8]>, to: Bound<&[u8]>) -> bool {
    match (from, to) {
        (Included(from), Included(to)) => from > to,
        (Included(from) | Excluded(from), Excluded(to)) | (Excluded(from), Included(to)) => {
            from >= to
        }
        _ => false,
    }
}

/// Return the newest row of `prefix`, a key of `text_deltas` without its
/// tid, with a tid before `before`: its key and its stored frame. A key
/// that the scan gives outside the prefix is `CorruptText`.
pub(super) fn newest(db: &Db, prefix: &[u8], before: Tid) -> Result<Option<(Bytes, Bytes)>> {
    newest_with(db, prefix, before, |key, frame| (key.to_vec(), frame.to_vec()))
}

/// Return the key of the newest row of `prefix` with a tid before
/// `before`, as `newest` does. The read copies no frame.
pub(super) fn newest_key(db: &Db, prefix: &[u8], before: Tid) -> Result<Option<Bytes>> {
    newest_with(db, prefix, before, |key, _| key.to_vec())
}

/// Find the newest row of `prefix` with a tid before `before`, and give
/// its key and its stored frame to `take`.
fn newest_with<T>(
    db: &Db,
    prefix: &[u8],
    before: Tid,
    take: impl FnOnce(&[u8], &[u8]) -> T,
) -> Result<Option<T>> {
    if before == Tid::ZERO {
        return Ok(None);
    }
    let txn = db.begin_read().map_err(backend)?;
    let Some(table) = open_optional(&txn, TEXT_DELTAS)? else {
        return Ok(None);
    };
    let start = text::delta_key(prefix, Tid::ZERO);
    let end = text::delta_key(prefix, before);
    let mut scan = table
        .range::<&[u8]>(start.as_slice()..end.as_slice())
        .map_err(backend)?;
    let Some(entry) = scan.next_back() else {
        return Ok(None);
    };
    let (key, value) = entry.map_err(backend)?;
    let key = key.value();
    // redb checks one bound of a scan only, so a damaged branch page can
    // give a key outside the scan.
    if key < start.as_slice() || key >= end.as_slice() {
        return Err(crate::text::corrupt(prefix, key));
    }
    Ok(Some(take(key, value.value())))
}

/// Return the rows of `text_deltas` in `db` from `from` to `to`, at most
/// `limit` of them, in key order: the key and the stored frame of each. A
/// key outside the scan or out of order is `CorruptText`.
pub(super) fn rows(
    db: &Db,
    from: Bound<&[u8]>,
    to: Bound<&[u8]>,
    limit: usize,
) -> Result<Vec<(Bytes, Bytes)>> {
    if limit == 0 || empty_range(from, to) {
        return Ok(Vec::new());
    }
    let txn = db.begin_read().map_err(backend)?;
    let Some(table) = open_optional(&txn, TEXT_DELTAS)? else {
        return Ok(Vec::new());
    };
    let mut rows: Vec<(Bytes, Bytes)> = Vec::new();
    for entry in table.range::<&[u8]>((from, to)).map_err(backend)? {
        let (key, value) = entry.map_err(backend)?;
        let key = key.value();
        let ordered = rows.last().is_none_or(|(last, _)| key > last.as_slice());
        if !inside(key, from, to) || !ordered {
            return Err(crate::text::corrupt_key(key));
        }
        rows.push((key.to_vec(), value.value().to_vec()));
        if rows.len() == limit {
            break;
        }
    }
    Ok(rows)
}

/// Return the keys of `text_txn` in `db` from `from` to `to`, at most
/// `limit` of them, in key order. A key outside the scan or out of order
/// is `CorruptTxn` with the tid of the key.
pub(super) fn txn_keys(
    db: &Db,
    from: Bound<&[u8]>,
    to: Bound<&[u8]>,
    limit: usize,
) -> Result<Vec<Bytes>> {
    if limit == 0 || empty_range(from, to) {
        return Ok(Vec::new());
    }
    let txn = db.begin_read().map_err(backend)?;
    let Some(table) = open_optional(&txn, TEXT_TXN)? else {
        return Ok(Vec::new());
    };
    let mut keys: Vec<Bytes> = Vec::new();
    for entry in table.range::<&[u8]>((from, to)).map_err(backend)? {
        let (key, _) = entry.map_err(backend)?;
        let key = key.value();
        let ordered = keys.last().is_none_or(|last| key > last.as_slice());
        if !inside(key, from, to) || !ordered {
            let tid = text::split_txn_key(key).map_or(Tid::ZERO, |(tid, _)| tid);
            return Err(ZoeError::CorruptTxn { tid });
        }
        keys.push(key.to_vec());
        if keys.len() == limit {
            break;
        }
    }
    Ok(keys)
}

/// Stage the delta `key` in `txn` with the frame of `payload`, whose
/// previous serial is `prev`, and its key in `text_txn`. A key without a
/// tid is a `Backend` error.
pub(super) fn put(
    txn: &WriteTransaction,
    key: &[u8],
    prev: Tid,
    payload: &[u8],
    compression: Compression,
) -> Result<()> {
    let frame = format::encode_frame_with(
        KeySpace::TextDeltas,
        key,
        format::MAIN_BRANCH,
        prev,
        payload,
        compression,
    );
    put_frame(txn, key, &frame)
}

/// Stage the delta `key` in `txn` with `frame`, a frame that is ready, as
/// it is, and its key in `text_txn`, which the call derives from `key`.
pub(super) fn put_frame(txn: &WriteTransaction, key: &[u8], frame: &[u8]) -> Result<()> {
    let index = text::txn_key_of(key)
        .ok_or_else(|| ZoeError::Backend("a key of a delta without its tid".into()))?;
    let mut rows = txn.open_table(TEXT_DELTAS).map_err(backend)?;
    rows.insert(key, frame).map_err(backend)?;
    let mut keys = txn.open_table(TEXT_TXN).map_err(backend)?;
    keys.insert(index.as_slice(), ()).map_err(backend)?;
    Ok(())
}

/// Stage the removal of the delta `key` in `txn`, and of its key in
/// `text_txn`, which the call derives from `key`. A key without a tid is a
/// `Backend` error. A missing row or index key is `CorruptText`, so the
/// write does not erase the evidence of a damaged store.
pub(super) fn remove(txn: &WriteTransaction, key: &[u8]) -> Result<()> {
    let index = text::txn_key_of(key)
        .ok_or_else(|| ZoeError::Backend("a key of a delta without its tid".into()))?;
    let mut rows = txn.open_table(TEXT_DELTAS).map_err(backend)?;
    let row = rows.remove(key).map_err(backend)?.is_some();
    let mut keys = txn.open_table(TEXT_TXN).map_err(backend)?;
    let indexed = keys.remove(index.as_slice()).map_err(backend)?.is_some();
    if !row || !indexed {
        return Err(crate::text::corrupt_key(key));
    }
    Ok(())
}

/// Create the tables of the deltas in `txn`, empty when they are missing.
pub(super) fn create(txn: &WriteTransaction) -> Result<()> {
    txn.open_table(TEXT_DELTAS).map_err(backend)?;
    txn.open_table(TEXT_TXN).map_err(backend)?;
    Ok(())
}

/// Store `bytes` under `key` in `text_deltas` of `txn`, or in `text_txn`
/// when `index` is true, or remove the row for `None`. Tests use it to
/// damage a store.
#[cfg(feature = "testing")]
pub(super) fn write_stored(
    txn: &WriteTransaction,
    index: bool,
    key: &[u8],
    bytes: Option<&[u8]>,
) -> Result<()> {
    if index {
        let mut keys = txn.open_table(TEXT_TXN).map_err(backend)?;
        match bytes {
            Some(_) => keys.insert(key, ()).map(drop),
            None => keys.remove(key).map(drop),
        }
        .map_err(backend)
    } else {
        let mut rows = txn.open_table(TEXT_DELTAS).map_err(backend)?;
        match bytes {
            Some(bytes) => rows.insert(key, bytes).map(drop),
            None => rows.remove(key).map(drop),
        }
        .map_err(backend)
    }
}
