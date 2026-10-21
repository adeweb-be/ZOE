# 0026. The transaction index: a `(tid, oid)` key space

Status: accepted. The human confirmed the choice of a key space over an oid list in the `txns` row, with the signature change of `Backend::txn_records` below.

## Context

F1 asks for `IStorageIteration`, and F10 for the iterator that backup, verification and `zodbconvert` use. Undo, `lastInvalidations` and replication tailing read one transaction at a time. A record key is `(oid, tid)`, so the records of one transaction spread over the whole `records` key space, and the oids of a transaction come only from a scan of every record key. A key space added later needs a migration of every store, and no real store exists yet.

## Decision

**The key space.** `txn_oids` is a redb table with one 16-byte key for each record, a deletion included: the tid, then the oid, each a big-endian `u64`. It is the `records` key with its halves swapped. So a range scan over one tid gives its oids in ascending order, and a scan over a tid range gives its transactions in tid order. The value is the unit type `()` of redb, with a fixed width of 0. The row: specification, "Storage core", "Data model".

**Writes.** `WriteTxn::put_record` stages the record and its index key in one write transaction, for a commit and for a restore, so a crash leaves both or neither. A blob record has no key of its own: the record of the same `(oid, tid)` has it.

**Reads.** `Backend::txn_oids(from, to)` and `Backend::indexed_tids(from, to, limit)` scan only the index keys. The iterator reads the rows of its range and the tids of the index a slice at a time, each slice one snapshot of at most 1,024 tids, and never reads the index of a tid before its rows. `Backend::txn_records(tid)` reads the row, the index keys and the records of one transaction in one snapshot. No read path scans `records` for the oids of a transaction. `load`, `loadBefore` and `history` do not read the index.

**A trait signature changes.** `Backend::txn_records(tid, oids)` becomes `Backend::txn_records(tid)`: it reads the oids from the index, in the snapshot of the row and the records. Nothing outside `zoe-core` implements `Backend`.

**Corruption.** The index has no frame and no checksum: the checksum of each record frame covers its `(oid, tid)` key. The iterator gives `CorruptTxn` for a transaction when:

- its index keys do not match the oid count of its row;
- an index key has no record, also when the oid count went down with the record;
- index keys of its tid have no row, and the range of the iterator has a row;
- a scan gives a key outside its range, or not after the key before it: a damaged branch page of redb can move a seek.

A failed scan of the index gives its error once. The iterator then reads the rest of its range without the check for keys of a tid that has no row, and `txn_records` still checks each transaction. A record that no row counts and no key names is outside what the iterator reads: only a scan of `records`, such as a scrub, finds it.

**Open.** A store without the `txn_oids` table fails to open with `Format`. No migration exists: the converters write the first real stores, with the index.

**Pack.** `WriteTxn::remove_record` removes the index key with the record, in the batch write of ADR 0025. It refuses a record without its index key with `CorruptTxn`. The batch write then fails and removes nothing, and the store halts until it reopens, so pack never erases the evidence of a damaged store. The crash simulation checks this on every crash image of a pack.

**Why the value is empty.** The key holds all that the index knows. A frame would add a 32-byte header to a 16-byte key, and its checksum would cover only that key. With the unit type, a leaf entry of redb costs its 16 key bytes; a byte-slice value adds a 4-byte offset.

## Consequences

- A read of one transaction or of a tid range reads nothing that grows with the rest of the store, except a deeper tree. In the acceptance test, `crates/zoe-core/tests/txn_index.rs`, a read of one commit of 8 records reads 124 KiB, and 160 KiB in a store with 8 times more records.
- Each record costs 16 bytes more in a leaf page. In a store of 500,000 records of about 270 bytes, the index takes 2,003 pages of 4 KiB: 13% more pages.
- The iterator holds one transaction, one slice of row tids and one slice of index tids. Only a run of rows that do not read stays in one slice and grows the memory.
- Every backend writes the key in `put_record`, removes it in `remove_record`, and reads it in `txn_oids`, `indexed_tids` and `txn_records`. The trait default of `indexed_tids` holds every oid of the range, so a backend overrides it to bound the memory of the iterator. The redb backend does.

## Alternatives

- **An oid list in the `txns` row.** A large transaction gets 8 bytes per oid in its row, which `undoLog` and `undoInfo` read. The list must be built from the staged records, and pack rewrites the whole list. RelStorage also indexes its objects by tid.
