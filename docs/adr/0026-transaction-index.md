# 0026. The transaction index: a `(tid, oid)` key space

Status: proposed. The choice of a key space over an oid list in the `txns` row was made while the human was away. The human confirms it at merge, with the signature change of `Backend::txn_records` below.

## Context

F1 asks for `IStorageIteration`, and F10 for the iterator that backup, verification and `zodbconvert` use. The iterator gives each transaction with its records. Undo, `lastInvalidations` and replication tailing read one transaction at a time.

A record key is `(oid, tid)`, so the records of one transaction are spread over the whole `records` key space. No key space mapped a tid to its oids, so `Backend::txn_oids` scanned every key of `records`. Each start of an iterator read every record key of the store, also for a range of one transaction.

Phase 1 writes the first real stores with `zodbconvert`. A key space that comes after them needs a migration of every store. No store exists outside the tests yet.

## Decision

**The key space.** The transaction index is a new redb table, `txn_oids`. It has one key for each record, a deletion included. The key is 16 bytes:

| Bytes | Field |
| --- | --- |
| 0 to 7 | the tid, as a big-endian `u64` |
| 8 to 15 | the oid, as a big-endian `u64` |

So the key is the `records` key with its two halves swapped. For example, the record of oid `0x1234` at tid `0x03f6db4f24444444` has the index key `03 f6 db 4f 24 44 44 44 00 00 00 00 00 00 12 34`. The byte order of the keys is the numeric order of `(tid, oid)`. A range scan over the keys of one tid gives the oids of that transaction in ascending order. A range scan over a tid range gives its transactions in tid order.

The value is empty. The value type of the table is the unit type `()` of redb, with a fixed width of 0.

**Writes.** `WriteTxn::put_record` stages the record and its index key in the same backend write transaction. A normal commit and a restore both write their records through it in `finish`. So both index each record, and a crash leaves a record and its key together, or neither. The signature of `put_record` does not change: its contract now includes the key. A blob record (ADR 0023) has no index key of its own. The core stages each blob record with the record of the same `(oid, tid)`, and that record has the key.

**Reads.** `Backend::txn_oids(from, to)` scans the index keys from `(from, 0)` to `(to, u64::MAX)`, both included, and reads nothing else. The iterator reads the rows and the index keys of its range first. Then `Backend::txn_records(tid)` reads the row of each transaction, the index keys of its tid and their records, in one snapshot. No read path scans `records` to find the oids of a transaction. `load`, `loadBefore` and `history` do not read the index.

**A trait signature changes.** `Backend::txn_records(tid, oids)` becomes `Backend::txn_records(tid)`. It reads the oids of the transaction from the index, in the snapshot of the row and the records, and no longer takes the oids of an earlier read. Pack added the method, and nothing outside `zoe-core` implements `Backend`. The rule of the crate makes every change of an existing signature a decision of the human, so the human confirms this one at merge.

**Corruption.** The index has no frame and no checksum of its own. The checksum of each record frame covers the `(oid, tid)` key of the record. The iterator checks the index against the rows and the records:

- The index keys of a transaction must match the oid count of its row. A record without its index key breaks this rule, and the iterator gives `CorruptTxn` for its transaction.
- Each index key must have its record. An index key without its record is `CorruptTxn` for its transaction, never `NotFound`. The row, the keys and the records of a transaction come from one snapshot. So a key that a removal left behind is `CorruptTxn` also when the oid count of its row went down with the record.
- Index keys of a tid that has no row give `CorruptTxn` for that tid, when the range of the iterator has a row.
- Each scan of the index checks that each key is inside its range and after the key before it. A damaged branch page of redb can move the seek of a scan, and the scan then gives a key of another transaction, or a key twice. Such a key is `CorruptTxn` with its tid.

A record that no row counts and that no index key names is outside what the iterator reads. Only a scan of `records` finds it. A scrub that reads every record can check the index key of each record at the same time.

**Open.** A store without the `txn_oids` table fails to open with `Format`. No migration exists, because no real store exists yet: the converters of phase 1 write the first real stores, with the index. So the format name stays `ZOE format 1`.

**Pack.** Pack removes records with `WriteTxn::remove_record`, which removes the index key with the record. Each batch of a pack is one backend write transaction that removes records, their index keys and their blob records, lowers the oid count of their rows, and removes a row that keeps no record (ADR 0025). So a row that pack removes has no index key left, and in every snapshot the index keys of a transaction match its row and its records. `remove_record` refuses a record without its index key with `CorruptTxn`. The batch write then fails and removes nothing, so pack never erases the evidence of a damaged store; as after any failed batch write, the store halts until it is reopened. The crash simulation checks this on every crash image of a pack.

## Why the value is empty

- The key holds everything that the index knows. A value would repeat what the record holds.
- A frame (ADR 0020) on each key would add a 32-byte header to a 16-byte key, and its checksum would cover only that key. The record frame already covers the oid and the tid, and the iterator checks each key against its record.
- With the unit type, a leaf page of redb stores no value bytes and no value offset, so an entry costs its 16 key bytes. With a byte-slice value, each entry also costs a 4-byte value offset.

## Why not an oid list in the `txns` row

The other choice was to write the oids of a transaction into the payload of its row, after the extension.

- A transaction that writes many objects, for example a large copied transaction or a catalog reindex, would get a row of 8 bytes for each oid. Readers that list transactions, such as `undoLog` and `undoInfo`, read the rows, so they would read every oid list too. The key space keeps the row small.
- `put_record` writes each key next to its record, so the index cannot miss a record of the commit. An oid list must be built from the staged records and written once, by `put_txn`.
- Pack removes single records. With the key space, it removes one key for each record. With a list, it rewrites the whole list of each transaction that it changes.
- RelStorage does the same: it indexes its `object_state` table by tid, so the objects of one transaction come from an index scan.

## Consequences

- A read of one transaction, or of a tid range, reads the index keys, the rows and the records of the range. It reads nothing that grows with the rest of the store, except a deeper tree. In the acceptance test, a read of one commit of 8 records reads 124 KiB in a store, and 160 KiB in a store with 8 times more records.
- Each record costs 16 bytes more in a leaf page, and its part of the branch pages. In a measured store of 500,000 records of about 270 bytes of pickle each, the index takes 2,003 pages of 4 KiB: 13% more pages than the store without it.
- A commit writes one more key for each record, in the same write transaction.
- The iterator holds the tids of its range in memory, and the records of one transaction at a time. The memory grows with the range, not with the store.
- The iterator does not see a record that no row counts and that no index key names.
- A pack removes one index key with each record.
- Every backend writes the key in `put_record`, removes it in `remove_record`, and reads the index in `txn_oids` and `txn_records`.
- A store written without the index does not open. No such store exists outside the tests.
