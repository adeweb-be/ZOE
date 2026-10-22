# 0033. The store counts its objects: `object_count` in `meta`, ZOE format 2

Status: accepted. The human decided the count in `meta` on #283 and approved this format change. The human settled the name `ZOE format 2`, the read-only open of a format-1 store, and the upgrade open as the one exception to N7 on #287. On #523 the human removed the upgrade open and the read-only open of a format-1 store: nothing runs ZOE in production, so no store of format 1 needs them.

## Context

ZODB's `IStorage` gives `__len__`, the number of objects. FileStorage gives the length of its index: each oid that has a record, a deletion included, until a pack removes every record of the oid. ZEO's `ClientStorage` takes its `len()` from the `length` of `get_info` at its registration, then from the `info {length, size}` that the server sends to the client that commits, and to every client after a pack.

ZOE kept no count. `len(zoe.Storage)` and the `length` of `zoe serve` were 0, which `IStorage` allows, because it asks only for an approximate length. ZODB's doctest `IExternalGC.test`, which ZEO's suite runs against its server, checks `len(storage)`: 3 objects before a `deleteObject`, and 1 after the pack. The phase 2 gate (ZOE-098) runs ZEO's suite, so the doctest must pass.

A count by a scan of `records` at each call reads every key of the store. A count in memory that the open builds reads every key at each open, which breaks N7.

## Decision

**The row.** `meta` gets the row `object_count`: an 8-byte big-endian number, as `last_tid`, `next_oid` and `pack_watermark`. In every snapshot it is the number of distinct oids that have at least one row in `records`, a deletion included. `meta` stays a key space without frames, so the row has no checksum.

**Commits.** Each commit batch updates the count in its own backend write transaction, the one that writes its records. The redb write notes each oid that it puts a record of and reads, before its first put of the oid, if the oid has a record. At its commit, it adds 1 for each noted oid that had none. So a normal commit, a restore and a deferred restore count each new oid once, and a transaction or a batch that writes an oid twice counts it once. The batch write runs under the commit sequencer, and redb has one writer, so the read sees every earlier write, the batches of a pack included. A pack batch that removes every record of an oid between the vote and the finish of a commit that writes the oid lowers the count, and the finish then counts the oid again. The vote decides nothing about the count.

**Packs.** Each pack batch updates the count in its own backend write transaction. The write notes each oid that it removes a record of and reads, before its first removal, if the oid has a record. At its commit, it subtracts 1 for each noted oid that has no record left. This holds with and without garbage collection. A dry run opens no write, so it changes nothing. A pack that removes old revisions only keeps the count.

**Other writes.** A sequence reservation, a restore of a sequence end and `Store::sync` change no record, so they write no count. A close writes no count.

**Reads.** The backend keeps the count of the last write that changed records in memory, and stores it after the commit of that write, under the commit sequencer. `Store::object_count()` gives it as `Some(count)` and reads no row. So after `finish` returns, the count includes the commit. `len(zoe.Storage)` and the `length` of `get_info`, of the `info` after a commit and after a pack, and of `get_size_info` give the count.

**The format name.** The name becomes `ZOE format 2`. A new store starts with format 2 and a count of 0. A store of format 2 without `object_count`, or with a row that is not 8 bytes, fails the open with `Format`. Another name, `ZOE format 1` included, fails the open with `Format`. A build from before this change refuses a store of format 2, because it compares the name. So no older writer changes the records of a store and leaves its count wrong.

**A store of format 1.** An open refuses a store of `ZOE format 1` with `Format`, as it refuses any other name, and writes nothing. No tool upgrades it. A read-only open of a store of format 2 reads the row.

**`zoe verify`.** The walk of the record keys counts the distinct oids. When the store has `object_count` and the walk read every key, a count that differs is one finding.

## Why the count is not decided at the vote

The vote reads the current record of each written oid, so it knows which oids have no record. A pack batch can remove every record of a written oid after the vote, so the finish must correct the vote with the notes of the pack. The write of the batch reads the records under the commit sequencer instead. It sees each pack batch before it, and it counts the same way for a commit, a restore, a deferred restore and the converters. The cost is one lookup of the oid in `records` for each written oid, in a write transaction whose pages the vote read just before.

## Why a new format name

The naming table says that the format name bumps only with a migration tool. The upgrade open was that migration until #523 removed it. Without a new name, a build from before this change would open a store of the new format and commit without changing the count.

## Consequences

- `len(zoe.Storage)` and the `length` of `zoe serve` are exact, as FileStorage's. ZODB's doctest `IExternalGC.test` passes through ZEO's suite.
- Each write that changes records reads at most one key of `records` for each oid that it touches (none for an oid at or above `next_oid`, which has no record yet), and a pack batch reads one more for each oid that it removes a record of. The write of the row is one more `meta` row in a write that already writes `last_tid`.
- An open refuses a store of format 1. No open reads every key of `records` to count it, so N7 has no exception for the count.
- A flipped bit in the row is outside the bit-flip gate, as for `last_tid` and `next_oid`, because `meta` has no frames. `zoe verify` finds such a count. A later write adds to and subtracts from the damaged value; it never fails for it.
- No tool takes a store back to format 1.
- Only the redb backend counts. The trait method `Backend::object_count` has a default that gives `None`.
