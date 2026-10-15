# 0037. Records keyed by `(tid, oid)`, an object index, ZOE format 3

Status: accepted. The human approved this change of the on-disk format on #363, and decided there that no migration exists: the project is at an early stage, and no store is in production. It supersedes the transaction index of ADR 0026 and the upgrade open of ADR 0033.

## Context

A record key was `(oid, tid)`. A commit that writes new versions of existing objects puts each record next to the older records of its oid. Each of these records lands in its own leaf page of `records`, and redb copies each leaf that a write transaction changes. A commit of 100 records of 1 KB that update 100 objects wrote 735 KB to the database file, seven times the bytes of its records, against 195 KB for a commit of 100 new objects (the measurement of ZOE-173). The bytes of a commit are what its sync must make durable, and what the copy of the pages costs in CPU. ZOE-077 measured that a layout keyed by `(tid, oid)` halves the bytes of such a commit.

The transaction index of ADR 0026 had the key `(tid, oid)` already, with an empty value.

## Decision

**The key spaces.**

| Table | Key | Value |
| --- | --- | --- |
| `records` | `(tid, oid)`, 16 bytes | the record frame of ADR 0020, checksum tag 1 |
| `object_index` | `(oid, tid)`, 16 bytes | empty |
| `blobs` | `(tid, oid)`, 16 bytes | the blob record of ADR 0023, checksum tag 2 |
| `txns`, `sequences`, `meta` | unchanged | unchanged |

Both halves of a key are big-endian `u64`, so the byte order of the keys is the numeric order. The table `txn_oids` is gone: `records` itself gives the oids of each transaction in oid order. The checksum of a record frame and of a blob frame covers the 16 bytes of the new key (ADR 0029), so a frame written under the old key does not verify under the new one.

`object_index` has one key for each record, a deletion included, written and removed in the backend write transaction of its record. Its value is empty. It has no frame and no checksum.

A commit appends its records at the end of `records`, because its tid is after every tid of the store. Only the keys of `object_index`, 16 bytes each, go next to the older keys of their oids.

**The reads.**

- `loadBefore(oid, tid)`: a reverse scan of `object_index` from `(oid, tid - 1)` gives the tid of the record; then a point read of `records` at `(tid, oid)`. The forward scan from `(oid, tid)` gives the next serial. `load` is `loadBefore` at `Tid::MAX`. The vote reads the current records in the same way.
- `loadSerial(oid, tid)` and `record_info`: a point read of `records`.
- `history(oid)`: a reverse scan of `object_index` over the oid, and a point read of each record.
- `record_iternext` and the scan of the pack (`Backend::record_keys`, `Backend::current_from`): scans of `object_index` in oid order.
- The iterator (`Backend::txn_records`): a scan of `records` over the tid. Each record must have its key in `object_index`, and the records must match the oid count of the row.
- `Backend::txn_oids` and `Backend::indexed_tids`: a scan of the keys of `records` over the tid range.
- The blob reads: point reads of `blobs`.

**The writes.** `WriteTxn::put_records` writes each record and its key of `object_index`. `WriteTxn::remove_record` removes both and fails when one is missing. The pack removes records through it, in batches, as before. The object count reads `object_index` to tell if an oid has a record; an oid at or after `next_oid` has none.

**Corruption.**

- A key of `object_index` without its record: a read that follows the key raises `CorruptRecord` with the oid and the tid. The iterator raises `CorruptTxn` for the transaction, because its records do not match the oid count of its row.
- A record without its key of `object_index`: `load`, `loadBefore` and `history` do not see it. The iterator raises `CorruptTxn` for its transaction.
- Each scan checks that each key is inside its range and after the key before it, as before. A key of `records` outside the scan raises `CorruptTxn`, and a key of `object_index` outside the scan raises `CorruptRecord`.

**The format name.** `ZOE format 3`. The open of a store with another name in `meta` fails with `Format`, and the message names the format of the store and `ZOE format 3`. A store of `ZOE format 1` or `ZOE format 2` does not open, for writes or for reads. No tool converts it: `zodbconvert` writes a new store from the source database.

## Why not keep `records` and add the copy

A second table of records keyed by `(tid, oid)`, with `records` kept for the loads, doubles the bytes of each commit. The object index costs 16 bytes of key for each record.

## Why no checksum in `object_index`

The transaction index of ADR 0026 had none either. A flipped bit in a key of `object_index` is a flipped bit in a page of redb outside a frame. Such a flip is outside the bit-flip gate already: it can give a hidden or a stale read, and the bit-flip harness counts these reads and does not assert them.

## Consequences

- An `update` commit writes about half of the bytes, or less: the test `commit_bytes.rs` gives the number.
- A load reads two keys: one in `object_index`, then the record. A load of a cold store reads the pages of both trees. The pages of `object_index` are dense, so they stay in the page cache longer than the pages of records.
- `written_oids` and `indexed_tids` read the leaf pages of `records` over their tid range, which hold the frames, and no longer pages of keys only. Their cost grows with the bytes of the records of the range, not with the store.
- Every store written before this change must be written again from its source. The table of names in the specification no longer says that a format bumps only with a migration tool.
- ADR 0026 holds for the iterator, with `records` in place of `txn_oids`. ADR 0033 holds, without the upgrade open of format 1 and its exception to N7.
