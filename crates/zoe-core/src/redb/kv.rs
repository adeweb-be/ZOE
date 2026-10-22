//! The tables of the KV namespaces in the redb backend: `kv_ephemeral` and
//! `kv_durable`, one row per key, and `kv_expiry`, the index of the
//! expiries. The first write of a flavour creates its tables, so a store
//! without them reads as a store without rows.

use std::ops::Bound::{self, Excluded, Included, Unbounded};

use redb::{ReadableTable, TableDefinition, WriteTransaction};

use super::write::main_frame;
use super::{Db, KV_DURABLE, KV_EPHEMERAL, KV_EXPIRY, backend, open_optional};
use crate::backend::Bytes;
use crate::error::Result;
use crate::format::{self, KeySpace};
use crate::kv::{Flavour, Swept};

/// Return the table of the rows of the namespaces of `flavour`.
pub(super) fn table(flavour: Flavour) -> TableDefinition<'static, &'static [u8], &'static [u8]> {
    match flavour {
        Flavour::Ephemeral => KV_EPHEMERAL,
        Flavour::Durable => KV_DURABLE,
    }
}

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
fn empty_range(from: Bound<&[u8]>, to: Bound<&[u8]>) -> bool {
    match (from, to) {
        (Included(from), Included(to)) => from > to,
        (Included(from) | Excluded(from), Excluded(to)) | (Excluded(from), Included(to)) => {
            from >= to
        }
        _ => false,
    }
}

/// Return the stored bytes of the row `row` of the namespaces of
/// `flavour` in `db`.
pub(super) fn row(db: &Db, flavour: Flavour, row: &[u8]) -> Result<Option<Bytes>> {
    let txn = db.begin_read().map_err(backend)?;
    let Some(table) = open_optional(&txn, table(flavour))? else {
        return Ok(None);
    };
    Ok(table.get(row).map_err(backend)?.map(|v| v.value().to_vec()))
}

/// Return the rows of the namespaces of `flavour` in `db` from `from` to
/// `to`, at most `limit` of them, as `Backend::kv_rows` tells.
pub(super) fn rows(
    db: &Db,
    flavour: Flavour,
    from: Bound<&[u8]>,
    to: Bound<&[u8]>,
    limit: usize,
) -> Result<Vec<(Bytes, Bytes)>> {
    if limit == 0 || empty_range(from, to) {
        return Ok(Vec::new());
    }
    let txn = db.begin_read().map_err(backend)?;
    let Some(table) = open_optional(&txn, table(flavour))? else {
        return Ok(Vec::new());
    };
    let mut rows: Vec<(Bytes, Bytes)> = Vec::new();
    for entry in table.range::<&[u8]>((from, to)).map_err(backend)? {
        let (key, value) = entry.map_err(backend)?;
        let key = key.value();
        // redb checks one bound of a scan only, so a damaged branch
        // page can give a key outside the scan or out of order.
        let ordered = rows.last().is_none_or(|(last, _)| key > last.as_slice());
        if !inside(key, from, to) || !ordered {
            return Err(crate::kv::corrupt(flavour, key));
        }
        rows.push((key.to_vec(), value.value().to_vec()));
        if rows.len() == limit {
            break;
        }
    }
    Ok(rows)
}

/// Tell if the index of the expiries of `db` holds `key`.
pub(super) fn expiry_has(db: &Db, key: &[u8]) -> Result<bool> {
    let txn = db.begin_read().map_err(backend)?;
    let Some(index) = open_optional(&txn, KV_EXPIRY)? else {
        return Ok(false);
    };
    Ok(index.get(key).map_err(backend)?.is_some())
}

/// Tell if `db` has the table of the namespaces of `flavour`.
pub(super) fn has(db: &Db, flavour: Flavour) -> Result<bool> {
    let txn = db.begin_read().map_err(backend)?;
    Ok(open_optional(&txn, table(flavour))?.is_some())
}

/// Stage the row `row` of the namespaces of `flavour` with `payload` in
/// `txn`, as `WriteTxn::put_kv` tells.
pub(super) fn put(
    txn: &WriteTransaction,
    flavour: Flavour,
    row: &[u8],
    payload: &[u8],
) -> Result<()> {
    let frame = main_frame(flavour.space(), row, payload);
    let mut table = txn.open_table(table(flavour)).map_err(backend)?;
    table.insert(row, frame.as_slice()).map_err(backend)?;
    Ok(())
}

/// Stage the removal of the row `row` of the namespaces of `flavour` in
/// `txn`.
pub(super) fn remove(txn: &WriteTransaction, flavour: Flavour, row: &[u8]) -> Result<()> {
    let mut table = txn.open_table(table(flavour)).map_err(backend)?;
    table.remove(row).map_err(backend)?;
    Ok(())
}

/// Stage `key` in the index of the expiries in `txn`.
pub(super) fn put_expiry(txn: &WriteTransaction, key: &[u8]) -> Result<()> {
    let mut index = txn.open_table(KV_EXPIRY).map_err(backend)?;
    index.insert(key, ()).map_err(backend)?;
    Ok(())
}

/// Stage one sweep step at the time `now` in `txn`, as
/// `WriteTxn::sweep_kv` tells.
pub(super) fn sweep(txn: &WriteTransaction, now: u64, limit: usize) -> Result<Swept> {
    let mut index = txn.open_table(KV_EXPIRY).map_err(backend)?;
    // A key of the index starts with its expiry, big-endian. So the
    // keys before the first key of `now + 1` have an expiry at or before
    // `now`.
    let end = now.checked_add(1).map(u64::to_be_bytes);
    let to = match &end {
        Some(end) => Excluded(&end[..]),
        None => Unbounded,
    };
    let mut keys: Vec<Bytes> = Vec::new();
    for entry in index.range::<&[u8]>((Unbounded, to)).map_err(backend)? {
        let (key, _) = entry.map_err(backend)?;
        let key = key.value();
        let ordered = keys.last().is_none_or(|last| key > last.as_slice());
        // A key that a damaged page gives outside the scan, or out of
        // order, ends the step. The keys before it go.
        if !inside(key, Unbounded, to) || !ordered {
            break;
        }
        keys.push(key.to_vec());
        if keys.len() == limit {
            break;
        }
    }
    let mut rows = txn.open_table(KV_EPHEMERAL).map_err(backend)?;
    let mut swept = Swept::default();
    for key in &keys {
        index.remove(key.as_slice()).map_err(backend)?;
        swept.taken += 1;
        let Some((_, row)) = format::split_kv_expiry_key(key) else {
            continue;
        };
        let Some(bytes) = rows.get(row).map_err(backend)?.map(|v| v.value().to_vec()) else {
            continue;
        };
        // A row that fails its checksum stays, as evidence. A row that a
        // later write refreshed has a later expiry, and stays too.
        let expired = format::decode_frame(KeySpace::KvEphemeral, row, &bytes)
            .ok()
            .and_then(|frame| {
                format::decode_kv_ephemeral(&frame.data)
                    .ok()
                    .map(|(e, _)| e)
            })
            .is_some_and(|expiry| expiry <= now);
        if expired {
            rows.remove(row).map_err(backend)?;
            swept.removed += 1;
        }
    }
    Ok(swept)
}
