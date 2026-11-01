# 0056. A write-ahead log with a background fold: ZOE format 3

Status: accepted. Card ZOE-272. The decision to build the log, the name `ZOE format 3` and the refusal of a store of format 2 are settled. This record gives the design, revised after an independent review. It changes the on-disk format and pack, two red zones. It changes no wire message and no public Python API.

## Context

ZOE keeps its records in redb, a copy-on-write B-tree, under the key `(oid, tid)`. Each record of a commit lands in the leaf of its oid. So a commit copies about two pages of 4 KB for each record that it changes, plus 12 to 17 fixed pages (`docs/spikes/server-write-volume.md`). For one B5 operation, `zoe serve` gives 5.7 to 6.6 times more bytes to its write calls than FileStorage appends, and the finish syncs all of them.

The spike ZOE-184 (`docs/spikes/write-log.md`, verdict "Go with conditions") built a write-ahead log with a background checkpoint. For an update of 100 records of 1 KB, the commit synced 63 KB, and the store wrote 168 KB in all, against 696 KB written and synced without the log. The finish of that update fell from 4.5 ms to 1.5 ms at concurrency 1, and the commits per second rose from 274 to 695 at concurrency 16, on a disk whose sync costs by the byte. At 100 KB, the log wrote each record twice, and its checkpoint competed with the commits for the device.

The layout `(tid, oid)` with a second index (ZOE-174) also wrote fewer bytes. A load then needs a second lookup, so that layout was rejected.

The design keeps:

- N1: a finish returns only after its commit is durable. A crash at any point loses no acknowledged commit and leaves no part of a commit.
- N2: a reader sees all of a commit or none of it, and `loadBefore` is consistent at any tid.
- N13: a damaged byte is detected, not read as data. Section 12 gives the one exception, its bound and its report.
- One lookup for each load: a record that the tables do not hold yet comes from memory, with no lookup in redb.
- N6 and N7: settings bound the memory, and an open after a clean close reads a fixed amount of data.
- The invariants of `crates/zoe-core/CLAUDE.md`: a synchronous core, the time and the files only through `Clock` and `Vfs`, the commit sequencer, group commit, the lock order.

## Words

| Word | Meaning |
| --- | --- |
| the log | the write-ahead log: the files of `wal/` in the store directory |
| segment | one file of the log |
| use, epoch | a use is the entries that a segment gets after a write of its header in the state "in use"; the epoch, in that header and in each entry of the use, names the use |
| spare | a segment whose header passes its checksum and says "spare": it holds no entry that counts |
| entry | the record of one commit batch in a segment |
| the tables | the redb tables of the store, as before this record |
| the fold | the thread of the core that writes the commits of the log into the tables; the spike calls it the checkpoint, a word that `CONTEXT.md` keeps for a snapshot |
| slice | one backend write of the fold |
| folded tid, F | the last tid that the tables hold durably |
| published tid, P | the last tid whose commit the unfolded table holds whole |
| backlog | the commits after F: the log and the unfolded table hold them |
| unfolded table | the index in memory of the records and the namespace writes of the backlog |
| direct batch | a commit batch that the leader writes into the tables, as each batch before this record |

## Decision

A finish appends its commit batch to a segment of the log and syncs that segment alone. The records of the batch go into the unfolded table, which every read path reads before the tables. The fold writes the commits of the log into the tables later, in slices of a few MB, each a durable backend write: when the backlog reaches a size, and at the latest when its oldest commit reaches an age. The backlog is bounded: a finish that meets the bound waits for the fold. A batch with mostly large records, a batch larger than half of the bound and a batch of deferred restores are direct batches. A clean close folds the whole log. The format name becomes `ZOE format 3`. The log is on by default.

### 1. The files of the log

- The log is the directory `wal/` in the store directory. A writer open creates it when it is missing, after it checked the format (section 21). A read-only open reads a missing `wal/` as an empty log.
- A segment is a regular file `wal/<n>.wal`, where `<n>` is 16 lower-case hexadecimal digits. A new file takes one more than the greatest number in `wal/`. The name tells nothing else: the headers order the segments.
- A new segment is prepared under the name `wal/<n>.new`: the file is created, gets a spare header with a new epoch, grows with `set_len` and is synced, then it is renamed to `wal/<n>.wal` and `wal/` is synced. So a `.wal` file always had a valid header on the disk. A writer open removes the `.new` files that a crash left. The open leaves every other entry of `wal/` as it is.
- Each operation on the log goes through the `Vfs`. A segment is a file of `Vfs::open_log`, a new provided method whose default is `Vfs::open`. On `OsVfs` it writes at once, with no held copy of the bytes (`OsFile` holds the writes of the database file in memory, and an entry can be large), and a growth through `set_len` gets zeros. The crate trait `Dir` gets `open_log`. A read-only open reads the segments with positional reads of the files, through `std::fs`, as `blob::ReadOnlyDir` reads blob files.

### 2. A segment

The header takes bytes 0 to 31. Integers are little-endian.

| Bytes | Field |
| --- | --- |
| 0–7 | magic `ZOE-WAL\0` |
| 8–11 | version, `u32`: 1 |
| 12–15 | state, `u32`: 1 in use, 2 spare |
| 16–23 | epoch, `u64` |
| 24–31 | xxh3-64, seed 0, of bytes 0 to 23 |

- A spare is an explicit state. A header that fails its checksum is never read as a spare: the open treats it as a damaged header (section 11), and the store never reuses that file.
- A header that passes with another version fails the open with `Format`.
- The epochs never repeat. Each new epoch is one more than the greatest epoch that the store has seen since its open, with a checked add: the epochs of the headers and of the entries that the open read, and each epoch that the store gave since. An add that overflows fails the new use, and the store halts.
- The epochs order the segments: a segment used later has a greater epoch. The entries of a use are those whose epoch is the epoch of the header.
- A segment in use holds entries from byte 32.

### 3. An entry

One entry is one commit batch: each member of the batch that got a tid, in batch order. An entry starts at a multiple of 8.

| Bytes | Field |
| --- | --- |
| 0–3 | magic `ZWAL` |
| 4–7 | op count, `u32` |
| 8–15 | body length, `u64` |
| 16–23 | epoch, `u64`: the epoch of the use |
| 24–31 | previous tid, `u64`: the last tid of the store before the batch |
| 32–39 | last tid, `u64`: the tid of the last commit of the batch |
| 40–47 | body checksum: xxh3-64, seed 0, of bytes 0 to 39 and the body |
| 48–55 | header checksum: xxh3-64, seed 0, of bytes 0 to 47 |

The body follows, then zeros up to the next multiple of 8. The body is a list of ops. A frame in an op is the value that the tables store under its key, byte for byte, with its own checksum.

| Kind | Op | Layout after the kind byte |
| --- | --- | --- |
| 1 | transaction row | tid `u64`, length `u32`, the frame of `txns` under the tid |
| 2 | record | oid `u64`, length `u32`, the frame of `records` under `(oid, tid)` |
| 3 | blob record | oid `u64`, length `u32`, the frame of `blobs` under `(oid, tid)` |
| 4 | ephemeral row | key length `u16`, key, length `u32`, the frame of `kv_ephemeral` under the key |
| 5 | durable row | key length `u16`, key, length `u32`, the frame of `kv_durable` under the key |
| 6 | removal of an ephemeral row | key length `u16`, key |
| 7 | removal of a durable row | key length `u16`, key |
| 8 | expiry key | key length `u16`, the key of `kv_expiry` |

- Each commit of the batch starts with its transaction row, and each op after it belongs to that commit: a record and a blob record have the tid of the row. Then come the records of the commit, its blob records and its namespace writes, in the order of the backend write before this record.
- The transaction rows come in tid order. The first tid is after the previous tid, and the last one is the last tid of the header.
- An entry that passes both checksums and breaks a rule, or holds another kind, fails the open with `Format`: a build that this build cannot read wrote it.
- The frames are the frames of a backend write. So the frame of an attached payload is the frame of an inline encode (invariant 66 of the core), and the fold copies the bytes as they are.
- The epoch in each entry lets an open read a use whose header is damaged, and lets it reject the bytes of an earlier use of the same file.
- The header checksum lets the replay trust the body length before it reads the body. With a passing header, the replay goes to the next entry by the length, never by a scan.

### 4. Where a batch goes

The leader takes one of two paths, under the commit sequencer, after it gave the tids. It decides from the lengths that the index of the staging holds, before it encodes anything. The estimate of a batch is its size in the backlog from these lengths (section 10).

- The log, by default.
- A direct batch: one backend write of the tables, with the code of a slice. A batch is direct when:
  1. each member with a tid is a deferred restore. The write commits without a sync (`commit_deferred`), as before.
  2. the tables hold a deferred commit that no sync covered yet. The write commits with a sync, which makes that commit durable, as before.
  3. the batch has records of at least `large_record` bytes (64 KiB by default), they hold at least half of the pickle bytes of the batch, and the backlog is empty. A batch with no large record, a batch of namespace writes only or of deletions only, goes to the log.
  4. the estimate of the batch is more than half of `backlog_bytes`.
  5. the log is off (`WriteLog::enabled` false).
- A direct batch needs an empty backlog, so that the tables hold each commit before it. The backlog is empty when the unfolded table holds no commit: a slice removes its commits from the table before it removes them from the backlog. So a direct batch never leaves an older version of an oid in the table above its own version in the tables. In cases 1, 4 and 5, the leader waits until the fold has emptied the backlog (section 10). Case 2 always finds an empty backlog: a deferred commit is direct, and no batch went to the log since. In case 3, a batch that finds a backlog goes to the log and asks for a fold, so the next large batch finds an empty backlog.
- Large records skip the log because a large record fills its own pages: the tables write it about once (1.28 times its frame in the spike), and the log would write it a second time. The spike measured more bytes and a slower finish at 100 KB with the log. A direct batch writes the bytes of today, with one sync.
- A batch over half of the bound skips the log because the unfolded table holds each frame of the backlog in memory: such a batch would take the memory of the backlog alone.
- Deferred restores skip the log because the converter of `Data.fs` writes millions of records with them, and the log would write each record twice. Also, each entry of the log is synced before the next entry is written. The replay relies on this rule (section 11), and an entry without a sync would break it.

### 5. A finish

The leader, under the commit sequencer:

1. Gives the tids and checks them, as before.
2. Chooses the path. On the log path, it waits while the backlog is not empty and the backlog and the estimate would pass `backlog_bytes` (section 10).
3. Places the blob files: the intent file in `blobs/tmp/`, the renames and the syncs of their directories, as before the backend write.
4. Encodes the frames of each member into one buffer, the entry. An attached payload goes into its frame as it is.
5. Starts a new use of a segment when the entry does not fit, and grows the segment when its length is too small for the entry (section 6).
6. Writes the entry with one write, then syncs the segment file (`sync_data`). This sync makes each member durable. A mark "a log sync runs" covers the write and the sync; a guard clears it on each path out, an error and a panic included.
7. Publishes the batch: under one write lock of the unfolded table, the records, the blob records, the transaction rows and the namespace writes of each member go into the table and the backlog, and the new oids into the object count (section 19). Then it stores the published tid P, the last tid of the batch, with release ordering, and then the last tid of the store.
8. Removes the intent files, notes the oids for a running pack and gives the members their outcomes, as before. The observer gets `batch_synced`.
9. Wakes the fold when the backlog was empty before the batch, so the fold waits for the age of its oldest commit, or when the backlog reaches `fold_bytes`.

A failure in steps 3 to 6 fails each member and halts the store, as a failed backend write does. The table gets none of the members. Their outcome is unknown until the next open, which replays the entry when it is whole on the disk.

A direct batch runs steps 1 to 3, then writes the tables in one backend write, with or without a sync, as before. Then F and P become its last tid, and the leader stores the last tid.

### 6. Segments: preparation, growth, new uses and spares

- An entry goes into the segment of the current use when it is the first entry of the use, or when its end stays within `segment_bytes`. Else the leader starts a new use. So a segment holds at least one entry, and an entry larger than `segment_bytes` makes its segment larger.
- A new use takes a ready spare. It writes the header (state "in use", a new epoch) with one write, then the first entry at byte 32 with a second write, then syncs the file once. A torn write of the header leaves a header that fails: the open then reads the use from its first entry (section 11).
- The first entry after an open always starts a new use. So no entry ever follows the bytes that a crash left at the end of a segment.
- The fold thread prepares the spares (section 1), off the commit path: the writer open prepares one when no spare is ready, and the fold prepares the next one when the segment of the current use passes half of `segment_bytes` and no spare is ready. When the leader finds no spare, it prepares one itself, on the commit path: a fold that falls behind costs this.
- The fold thread grows the segment of the current use ahead of the leader, with `set_len` and a sync of its own: it keeps the length at least a headroom beyond the end of the last entry. The headroom is the bytes of the current use, at least 1 MiB and at most 8 MiB, and the length stays within `segment_bytes`. On `OsVfs` a growth gets zeros, so a later append changes no block map, and its sync writes no metadata of the file system. The commit path writes no zeros.
- The leader grows a segment itself, with `set_len`, only when an entry does not fit the length of the file: an entry larger than the headroom, or a spare shorter than the entry. Such a growth writes zeros on the commit path. It is rare, because entries over half of the backlog bound are direct batches.
- A segment retires when the fold has folded and removed from the table each entry of its use, and it is not the segment of the current use. It becomes a spare in memory: its header stays on the disk until a new use or the close writes over it. At most two spares stay. The fold removes the other retired files, with no sync of `wal/`: a removed file that a crash brings back holds folded entries only.
- A segment whose header failed at the open is never reused: once the fold has folded its entries, the fold removes it.
- The spares exist so that the log writes no zeros once it reaches its size.

### 7. The unfolded table and the backlog

- The backlog holds the commits after F in tid order: for each commit, its transaction row, the oids of its records, its namespace writes, the monotonic time of its publication and its size.
- The size of the backlog, which `Store::backlog_bytes` gives and `backlog_bytes` bounds: each entry buffer in memory, whole, until the fold frees it, and 64 bytes for each op for the indexes. A buffer counts until its last commit leaves the table.
- The unfolded table is one `RwLock` over:
  - the records: one `BTreeMap<Oid, versions>`. The versions of an oid are those of the backlog, in tid order. Each gives its tid and the places of its frame and of its blob record.
  - the namespace rows: one `BTreeMap` from the flavour and the key of a row to the last op of the backlog on it, a write or a removal, with its tid.
  - the expiry keys of the backlog, with their tids.
  - the backlog itself.
- Beside the lock, atomic counters: the commits of the backlog, its blob records, its namespace writes of each flavour, P and F.
- One lock: the leader inserts a whole batch under one write lock, so a reader sees all of it or none, and a scan reads one ordered map. A read takes the read lock once. Under 16 threads, a read lock costs an atomic add on one shared word, tens of nanoseconds, against 1.3 µs for a hit of the table and 2.6 µs for a load from redb in the spike. When the backlog is empty, a read takes no lock: it reads one counter. Shards by hash spread the lock word but make each range scan visit every shard. Shards by oid range keep the scans but put the new objects, which are the hot ones, in one shard. The card measures loads from 16 threads; a miss of its target is a stop-and-ask, not a silent redesign.
- The frames stay in the buffer of their entry. The commits of an entry share it through one `Arc`, and the table holds offsets into it. No frame is copied after its encode, and the file of the segment holds no copy either (section 1). The buffer leaves memory when the last of its commits leaves the table.
- The suffix rule: for each oid, each version in the table has a greater tid than each version of the tables that the table does not also hold. The fold keeps the rule: it folds commits in tid order, and a commit leaves the table only after the tables hold it durably. So when the table has a version of an oid before a tid, the newest version of the oid before that tid is in the table.

### 8. The reads

Three rules hold for each read path:

- A read of the table loads P (acquire ordering) first, and ignores each version, row, key and commit of the table after P. The leader stores P after the whole batch is in the table, so a reader sees a prefix of the commits: never one oid of a commit new and another oid of it old. This holds for the reads of the current state too: `load`, `kv_get`, `kv_scan`, `history`, `record_iternext`, `undo_log`.
- The table first, then the tables. A commit leaves the table only after the tables hold it, so a read that misses a commit in the table finds it in the tables. A read of the tables first could miss a commit that a slice moves between its two reads. The fold folds only commits at or before P, so the tables never hold a commit that the table hides.
- A scan merges the two sources in key order, and a commit or a version that both give counts once.

| Read | Rule |
| --- | --- |
| `load_before`, `load_before_next` | The versions of the oid in the table give the newest before the tid and the first at or after it. When the table has a version before the tid, the read ends there, with no lookup in redb. Else one read transaction of the tables gives the record and its next serial, as before, and the next serial is the lower of the two sources. |
| `load_serial`, `record_info`, `load_blob` | A point read of the table, then of the tables. |
| `current_serials`, the vote | The newest version of each oid in the table; one read of the tables for the other oids of a chunk, as before. |
| `history` | The versions of the table, newest first, then those of the tables after them. |
| `current_from`, `record_iternext` | The first oid at or after the bound in the table and in the tables; the lower one wins. Its newest record comes from the table when the table holds the oid. |
| `record_keys` | The keys of both sources in key order; the list still stops after the first oid that brings it to `limit` keys. |
| `iter_txns`, `indexed_tids`, `txn_oids`, `txns_before`: the iterator, the undo log | The backlog gives the commits after F in tid order, and the tables the others. A merge copies the part of the backlog in its range first, then reads the tables. A slice of `indexed_tids` still holds at most `limit` tids, so the iterator keeps its bound of memory. A commit without a record has no index key in either source. |
| `txn_records`: the iterator, undo | A whole commit from one source: the backlog when it holds the tid, else the tables. Never the row from one source and records from the other. |
| `has_blobs`, `has_kv` | The tables, or the counters of the table. |
| `kv_row` | The last op of the row in the table: a write gives its frame, a removal gives no row. Else the tables. |
| `kv_rows` | The rows of the tables in the range, with the ops of the table on them: a write replaces or adds a row, a removal hides one. The call gives `limit` rows unless the range holds fewer: when the table hides rows, it reads more rows of the tables. `Store::kv_scan` relies on this, because a chunk with fewer rows than it asked for ends its scan. |
| `kv_expiry_has` | The table, then the tables. |
| `object_count`, `last_tid` | Section 19. |
| `pack_watermark`, `sequence_end`, `sequence_names` | The tables alone: no commit of the log writes them. |

- Each frame from the table passes its checksum, under its key space and its key, before a read uses it, as a frame of the tables does (invariant 99 of the core).
- A read-only store reads the tables alone: its open checks that the log holds no commit after F (section 14).
- The hooks of `zoe_core::testing` that read or change stored bytes first fold the whole backlog, then work on the tables as before.

### 9. The fold

- The fold runs on one thread for each writable store, which the open starts with `Clock::spawn` (section 25). A read-only store has none.
- It starts when the backlog reaches `fold_bytes`; when the oldest commit of the backlog reaches `fold_age`, on the monotonic time of the store clock; or on a request: `Store::fold`, a pack, a leader that waits for the fold, a large batch that went to the log, `Store::kv_restore_durable` and the close. It waits on a waiter of the store clock, with the time to the next age as its timeout.
- `fold_age` is 5 s by default. It bounds the time that an acknowledged commit stays in the log alone, which bounds the exposure of section 12, and it empties the backlog of a quiet store, so its large batches go direct and its reads take no lock. Under load, the backlog reaches `fold_bytes` before the age, so the age changes nothing where the bytes matter. At the load of B5 at 4 users, 5 to 7 commits per second, a fold takes 25 to 35 commits: the fixed pages of a redb commit and the hot leaves of the catalog are shared by all of them, where a commit today pays them alone. A bound under a second would cut that sharing to a few commits; a bound of a minute would leave a minute of commits exposed.
- A slice takes the oldest commits of the backlog at or before P, whole commits, until their bytes reach `slice_bytes`, and at least one commit. It checks the checksum of each frame under its key space and its key. One backend write of the tables stores the frames as they are, with a new method of `WriteTxn` that also writes the key of the transaction index, notes the oid for the count and raises the next oid for a record, as `put_payloads` does. It stores the namespace rows, removals and expiry keys, then `last_tid`, `next_oid` and `object_count` in `meta`, and commits with a sync. The write counts the objects as a batch write did before (ADR 0033), so the count in `meta` stays exact. A frame that fails its checksum fails the slice.
- After the commit of a slice, F becomes the last tid of the slice. Then, under the write lock, the slice leaves the table (each version, row and key whose tid is in the slice) and the backlog. Then the object count gets its correction (section 19), the segments whose entries are all folded retire, and the waiters wake. Then, outside the guard of section 20, the observer gets `slice_folded`: an observer that panics does not halt the store. The fold goes on while the backlog is at or above `fold_bytes`, while a commit is older than `fold_age`, or while a request waits.
- The fold also prepares the spares and grows the current segment (section 6).
- The fold holds the one writer of redb for one slice at a time. So a sequence reservation, a sweep step, a pack batch or `Store::sync` waits for one slice at most, and a slice stays far below the page cache of redb. The spike measured 1.16 to 1.91 bytes in the tables for each byte of frame with folds of 200 to 384 commits. A slice of 4 MiB holds about 60 commits of 100 records of 1 KB with zstd.
- Before the commit of a slice, the fold waits for the end of a log sync that runs, if one runs. It does not wait for the next one. So the sync of a slice does not start during the sync of a finish, and a run of finishes cannot starve the fold. When the fold falls behind, the bound of the backlog stops the finishes.
- The fold never takes the commit sequencer.

### 10. A full backlog

- A leader on the log path waits while the backlog is not empty and the backlog and its batch would pass `backlog_bytes`. It estimates the size of the batch in the backlog before it encodes: the lengths of the pickles, of the namespace writes and of the metadata, with the fixed part of each op and of the entry, and 64 bytes for each op. A frame is never longer than its pickle and its header, so the estimate is never below the size, and the backlog never passes its bound. The leader asks for a fold and waits on a waiter of the store clock, which each slice wakes. It keeps the commit sequencer while it waits, as during a sync: the next batch would wait for the same room, and the commits keep their arrival order. A full backlog stalls the finishes and fails none of them.
- A leader that waits for an empty backlog (section 4) waits the same way.
- The observer gets `backlog_waited` with the time of each wait, on the monotonic time of the store clock.
- A failed fold wakes the waiters, and their batches fail with `Halted` (section 20).

### 11. The open of a writer

1. Open `zoe.redb` and check the format, as before (section 21). F is the last tid of the tables: the greater of `last_tid` in `meta` and the key of the last transaction row.
2. Create `wal/` when it is missing. Remove the `.new` files of `wal/`.
3. Read the header of each `.wal` file:
   - It passes and says "spare": a spare.
   - It passes and says "in use": a segment in use with the epoch of the header.
   - It fails: the header is damaged, and the file is never reused (section 6). When the entry at byte 32 passes both checksums, the segment is in use with the epoch of that entry, and the replay reads it. Else the open scans the file for evidence (below), with any epoch. An entry found there with a commit after F fails the open with `CorruptTxn`; else the file holds no commit after F.
4. Replay each segment in use, in epoch order, from byte 32, one entry after the other:
   - The header passes and its epoch is the epoch of the segment: the header gives the body length. An entry whose last tid is at or below F is folded: the replay goes to the next entry by the length, and reads no body.
   - The body of an entry with a commit after F must pass its checksum, and the entry must follow the commit before it. The first such entry holds F as its previous tid or as the tid of one of its commits. Each later entry has as its previous tid the last tid of the entry before it. The commits after F go into the table and the backlog, as a publication does.
   - A broken chain fails the open with `CorruptTxn`, with the tid that the entry names as its previous tid and that the log does not hold. A segment that someone removed gives this, and so does a damaged last entry of a segment that is not the last one.
   - An entry D whose header passes and whose body fails, or that the end of the file cuts: D is cut, and the open scans for evidence from the end that the length of D gives.
   - A header that fails, or that has another epoch: the open scans for evidence from the next multiple of 8.
   - The scan for evidence: from its start to the end of the file, at each multiple of 8, an entry of the epoch of the segment whose header and body pass. The replay never replays an entry that a scan found: such an entry can lie inside the body of a damaged one. When the scan finds an entry with a commit after F, the open fails with `CorruptTxn`: with the last tid of D when the header of D passed, else with the previous tid of the entry found when it is after F, else with its last tid. When the scan finds none, the segment ends at the failure: the bytes from there on are its torn tail.
   - These rules hold because the leader writes an entry only after the sync of the entry before it, and starts a new use only after the sync of the last entry of the use before. So only the last entry of the log can be cut by a crash, and a valid entry after a damaged one proves the damage.
5. An I/O error on any read of `wal/` fails the open with that error. It is never read as a torn tail.
6. When the open cut an entry D whose header passed and whose last tid is after F, it logs the tids of D, from its previous tid to its last tid, at ERROR, and `Store::cut_at_open` gives them: the commits of a finish that a crash stopped, or of an acknowledged entry that a damage of the disk destroyed. The open cannot tell them apart (section 12).
7. A segment in use whose entries are all at or below F retires: it is a spare, unless its header failed. The last tid of the store and P are the greater of F and the last tid of the replay. The next oid is the greater of `next_oid` in `meta` and one more than the greatest oid of the replay.
8. The recovery of `blobs/tmp/` runs after the replay, so a blob record of the log counts: the open keeps the file of such a record.
9. Prepare a spare when no spare is ready, and start the fold. When the backlog is at or above `fold_bytes`, or a replayed commit is older than `fold_age`, the fold starts at once.

Reads go on while the fold runs: the table serves the replayed commits. The replay reads the entries after F, at most the bound of the backlog and one entry, and the headers of the folded entries of the segments in use. The spike measured 1.3 to 1.6 ms for each MB of log, so a full backlog of 256 MiB replays in about 0.4 s. After a clean close, the open reads 32 bytes of each segment file.

The replay writes nothing in `wal/` but the removal of `.new` files. A crash during the open, during the fold after it or right after it loses nothing: the log still holds each commit after F, and F moves only with a durable slice.

### 12. A damaged last entry: the one exception, bounded and reported

With one checksum for each body, a crash during the write of the last entry and a damage of the disk in that entry after its sync give the same bytes. The open cuts such an entry. So a damaged byte in the last entry of the log, after a crash, loses the commits of that entry with no error at their load. This is the one exception to N13. Three bounds and a report hold:

- The fold folds each commit within `fold_age` (5 s) of its publication. While the process runs, the fold reads the frames from memory, so a damage of the disk in the log changes nothing. A damage counts only with a crash before the fold of the entry: the commits of the last 5 s at most.
- Only the last entry of the log is exposed. A damaged entry with any valid entry after it fails the open with `CorruptTxn` (section 11).
- An I/O error is never read as a torn tail.
- The report: when the header of the cut entry passes, the open logs its tids at ERROR and `Store::cut_at_open` gives them. `zoe serve` logs them with the name of the storage at ERROR after its open. A clean close folds the log, so a store at rest has no such entry.

Stamps in each sector of an entry would let the open tell a torn write from a damage, at the cost of a format with a stamp every 512 bytes and of the copies to place them. This record takes the bounded and reported route instead.

### 13. The close

The drop of a writable store that did not halt:

1. Stops the fold thread after its slice. The thread drops each `Arc` that it holds of the backend and of the log before it wakes the close, so the database closes before the drop returns, and an open right after it does not get `Locked`.
2. Folds the whole backlog on the thread of the drop, slice by slice.
3. Writes the state "spare" into each header that says "in use", the headers of retired segments included, with its epoch, and syncs each such file.
4. Removes the spare files beyond two.
5. Closes the database, as before.

A halted store, or a failure in steps 2 to 4, skips the remaining steps of the log: the log stays, and the next open replays it. A WARN event names the error. The drop never panics for it.

So after a clean close, the tables hold each commit, and each header of the log says "spare". The headers keep their epochs. A crash during the close loses nothing: a header says "spare" only after the fold has made the tables durable.

### 14. The read-only opens

- A read-only open reads the headers of `wal/` with positional reads, and walks the headers of the entries of each segment in use, as the replay does, with the rules of section 11 for a damaged header. It reads a body only where those rules need it. It writes nothing.
- `Store::open_read_only`, the strict open, fails with `NotClosedCleanly` when a segment in use holds a commit after F, and with `CorruptTxn` when the log is damaged. After a clean close each segment is spare, so the open reads 32 bytes of each file.
- `Store::open_read_only_repairing`, the open of the tools (ADR 0028), takes the writer path of its repair when redb needs its repair or when the log holds a commit after F. It opens `zoe.redb` for writes, and redb repairs the file. Then it replays the log and folds it into `zoe.redb` slice by slice with syncs, and the database closes cleanly. Then the read-only open runs, and `repaired` gives true. The repair writes only `zoe.redb`: it leaves the segment files, `blobs/tmp/` and `staging/` as they are. The entries of the log are then all at or below F, so a later strict open reads the store. On a damaged log, the open fails with `CorruptTxn` after the repair of redb, and folds nothing.
- `Store::open_read_only_tables` is a new strict read-only open that does not read the log. Only `zoe verify` uses it: when the open of the tools fails with `CorruptTxn` on the log, `zoe verify` reports the damage as a finding, under the key `log` with the tid and the text of the error, then opens the tables with this call and verifies them. It exits with 1.
- So `zoe backup`, `zoe convert --to-data-fs`, `zoe pack --dry-run` and `zoe mcp` read the tables alone, refuse a damaged log, and their code does not change.

### 15. Pack

- Under the commit sequencer, the pack reads the last tid L and starts the notes of the finishes, as before. Then it releases the sequencer, asks for a fold, and waits until each commit at or before L is folded and removed from the table. Then it marks and removes as before.
- So the tables hold each record at or before T, the table holds none of them, and the pack removes rows of the tables only. It keeps each record after T, in the backlog or in the tables. It reads the records that the finishes note during the pack through the merged reads.
- A pack batch holds the commit sequencer and waits for the one writer of redb, which a slice can hold. So a finish can wait for one batch of a pack and, through it, for one slice. `Store::sync` gives the same wait.
- The dry run asks for no fold. It reads through the merged reads and writes nothing.
- The pack watermark stays in `meta`, and `check_undone` reads it as before.
- The wait is in `src/pack/`, so the card touches that red zone too.

### 16. Blobs

- The blob files go into place before the write of the entry, the intent file first, as before the backend write. The blob records travel in the entry. So a durable blob record always has its file.
- The finish removes its intent file after the sync of its entry.
- The open replays the log before the recovery of `blobs/tmp/`, so a file that an intent file names stays when the log or the tables hold its blob record.
- A blob read starts from the blob record, in the table, then in the tables, and checks the file as before. A blob undo links the file as before.
- A pack removes blob files after its batch, as before: each of its records is folded.

### 17. Group commit, the lock hand-off, deferred restores and `Store::sync`

- Group commit does not change: one leader, one entry and one sync for each batch. A batch is whole or absent in the log, as it is in the tables.
- `batch_synced` gives the commits that one sync of an entry or of a direct batch made durable.
- The lock hand-off (ADR 0044): a commit in flight is durable when its entry is synced, and its hand-off settles then. A vote that waits for that commit then reads the table.
- Deferred restores are direct batches (section 4). The first batch after them that has another member is a direct batch with a sync.
- `Store::sync` does not change: the barrier write of the tables, with a sync, under the commit sequencer. It can wait for one slice. The log holds no entry that a sync does not cover.
- The members of a batch release their locks after the sync, and the invalidations go out after the acknowledgement, as before.

### 18. KV namespaces, sequences and the sweep

- The namespace writes of a member go into its entry after its records, as ops 4 to 8, with the frames that the leader wrote into the tables before: the expiry of an ephemeral row is the time of the write of the batch plus its TTL, and each durable namespace that the member writes gets its generation row.
- A read gives the last committed value: the last op in the table at or before P, else the tables. An expired row reads as absent, also from the table.
- A sweep step stays a backend write of its own outside the commit sequencer, and reads and removes rows of the tables only. A row that the backlog writes again is in the table, and reads see it; the fold then writes it. A removal in the backlog of a row that a sweep removed removes an absent row, which stays absent.
- `Store::kv_restore_durable` takes the commit sequencer, asks for a fold, waits for an empty backlog, writes its rows and releases the sequencer. No commit comes between the wait and the write, so an older write of the log never lands over a restored row.
- Sequences never go through the log: a reservation and `Sequence::restore_end` stay backend writes of their own. Each waits for one slice at most.

### 19. The object count, the next oid and the last tid

- The backend keeps one count in memory, as before. A log batch adds, for each member, its records whose written previous serial is `Tid::ZERO`: the serial that the entry holds, after the leader put the tid of the holder in place of the placeholder of a record merged onto a commit in flight (`onto` in `commit/group.rs`). The vote checks the serial of each written oid under its lock, so only an oid without a record gets such a record. Each backend write, a slice, a direct batch or a pack batch, adds its own change of the count, less the oids that the log already counted for the records of the write. The count in `meta` stays exact in each snapshot (ADR 0033), because the slice reads the records as a batch write did.
- One case leaves the count in memory wrong for a time. A pack batch removes every record of an oid X after a transaction voted on X with a previous serial other than `Tid::ZERO`, and before that transaction finishes through the log. The log counts no new oid for X, so from the publication of that commit to the commit of the slice that folds it, the count in memory is one lower than the number of oids with a record. The slice counts X, and the count is exact again. Only `len()` and the `length` of `zoe serve` show it.
- A finish raises the next oid in memory before its write, as before. The slice writes `next_oid` to `meta`, and the open takes the greater of `meta` and the replay.
- The leader stores the last tid in memory after P for a log batch, and after the backend commit of a direct batch. `last_tid` in `meta` is F: only a slice and a direct batch write it.

### 20. Errors and the halt

- A failed write or sync of an entry, a failed growth or a failed new use fails the members and halts the store, as a failed backend write does: the commit sequencer is empty. The table gets none of the members.
- A failed slice marks the fold failed, wakes the waiters and stops the fold thread. The next lock of the commit sequencer sees the mark and halts the store. Later finishes, packs and calls of `Store::fold` fail with `Halted`. The table keeps each commit, so the reads go on, and the next open replays the log. The fold never takes the commit sequencer, so a leader that waits for the fold under the sequencer never waits for itself.
- A panic of the fold thread does the same: a guard of the thread marks the failure as it unwinds. The calls of the observer are outside that guard.
- No step retries a failed sync: the state of the data is unknown after it, and the next open replays what the disk holds.
- An open fails with `CorruptTxn` on a damaged log that hides an acknowledged commit or whose chain breaks, with `Format` on a log of another version, and with `Io` on an I/O error of a read of `wal/`. No new error variant: `CorruptTxn` names the transaction that the store cannot give.

### 21. The format name

- `format::FORMAT_NAME` becomes `ZOE format 3`. A new store starts with it.
- A writer open reads the format of `zoe.redb` before it creates `wal/`, prepares a spare, or syncs anything for the log. An open of a store of `ZOE format 2`, or of any other name, fails with `Format`, and creates no file and no directory in the store. The sync of the store directory that makes an earlier creation of `zoe.redb` durable stays as before. No migration exists: `zoe convert` through `Data.fs` moves the data.
- A build of format 2 refuses a store of format 3 by its name. So no older build writes the tables of a store whose log holds commits that the tables do not hold.
- The tables, their key spaces, the frames and `meta` keep their layout. The name, `wal/` and its files are new.

### 22. The settings

`Options::write_log` is a `WriteLog`:

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | the log path; false makes each batch direct |
| `segment_bytes` | 64 MiB | the size at which a segment stops to grow, at least 4,096 |
| `fold_bytes` | 16 MiB | the backlog at which the fold starts |
| `fold_age` | 5 s | the age of the oldest commit of the backlog at which the fold starts |
| `slice_bytes` | 4 MiB | the most bytes of entries for one slice |
| `backlog_bytes` | 256 MiB | the bound of the backlog, at least `fold_bytes` |
| `large_record` | 64 KiB | the pickle size of a large record |

- An open with settings that do not hold together fails with an `InvalidInput` I/O error.
- With `enabled` false, the open still replays and folds a log that exists.
- The log is on by default. The gates of the project (the native gate, B5) measure `zoe serve` with its defaults, so a log off by default would show nothing there. The crash simulation must cover the direct path in any case. One default keeps one matrix of tests.
- The skins do not expose the settings in this card: no key of `<zoe>`, no flag of `zoe serve`, no keyword of `zoe.Storage`. Tests and benchmarks set them through `Options`.
- New public items of the core: `Store::fold`, `Store::backlog_bytes`, `Store::cut_at_open`, `Store::open_read_only_tables`, and the provided methods `Vfs::open_log` and `Clock::spawn`.

### 23. Memory (N6)

- Each writable store holds at most `backlog_bytes` of log entries and their indexes in memory, 256 MiB by default, plus the entry that the leader writes. The entry buffers count whole until they are freed (section 7).
- The segment files hold no copy of an entry (section 1).
- The specification adds this to its list of the memory of `zoe serve`.

### 24. Metrics, spans and logs

- The observer gets two methods, each with a default that does nothing: `slice_folded(commits, bytes, duration)` after each slice, and `backlog_waited(wait)` after each wait of a leader for the fold. `batch_synced` stays.
- `zoe serve` exports them with the label `storage`: `zoe_fold_seconds`, a histogram of the slices; `zoe_fold_commits_total`; `zoe_backlog_wait_seconds`, a histogram of the waits; and the gauge `zoe_backlog_bytes`, read from `Store::backlog_bytes` at each scrape. After its open, it logs `Store::cut_at_open` at ERROR.
- The span `fsync` covers the write and the sync of an entry or of a direct batch, as before. A new INFO span `fold` with the field `commits` covers each slice.
- A failed slice and a close that cannot fold give a WARN event. An entry that the open cut while its header passed gives an ERROR event with its tids: the one exception to "WARN events only" of the core. No event holds bytes of a record.

### 25. Threads, locks and the clock

- `Clock` gets `spawn(name, work)`, with a default that starts a thread of the system. The simulator of zoe-server gives its own, so the fold runs under its baton. The fold waits only on waiters of the store clock.
- The fold thread holds the shared state of the log and the backend, never the store, so the drop of the store runs. The close waits for the end of the thread on a waiter, not on a join handle, after the thread dropped its references (section 13).
- The new locks: the mutex of the segments (the current use, the spares, the files to grow), the lock of the unfolded table, and the mutex of the requests to the fold. The leader takes the mutex of the segments and the lock of the table under the commit sequencer, one at a time. The fold takes each of them without the sequencer, one at a time, and takes no lock under them. So the lock order of the core gains two edges: the commit sequencer, then the mutex of the segments; the commit sequencer, then the lock of the table. No cycle exists, because nothing takes the sequencer under them.
- The mark "a log sync runs" is an atomic with a guard that clears it on each path out of the write and the sync, an error and a panic included.
- A finish can wait for one slice, through a pack batch or `Store::sync` that holds the sequencer and waits for the writer of redb. A direct batch never waits for a slice: it needs an empty backlog, so no slice runs.
- The core stays synchronous: the fold is a thread of the system, with no async code.

### 26. The crash simulation and the simulator

- Each file operation of the log goes through the `Vfs`. So `MemVfs::crash_images` covers the writes and the syncs of the entries, the growths, the preparation of spares, the headers of the new uses and of the close, and the removals.
- The acceptance tests open each crash image of the finishes across segments and into a spare, of a slice, of an entry that F cuts in two, of a path from the log to a direct batch and back, of a pack that waits for the fold while finishes run, of the open that replays a log and of the fold after it, and of a clean close.
- The `SimVfs` of the simulator of zoe-server treats each file of `wal/` as it treats the database file: the writes wait in a cache until the sync, and a sync gives the baton back and is a crash point. `SimClock::spawn` runs the fold as a thread of the simulator. The crash phase "finishing" is then the sync of an entry.

### 27. Where the code goes

- `src/format/wal.rs`, a red zone: the layout of a segment header, of an entry and of its ops, with their encode, decode and checks. `FORMAT_NAME` in `src/format/mod.rs`.
- `src/wal/`: the logged backend. It implements `Backend` over the redb backend, and holds the segments, the unfolded table, the backlog, the replay and the fold.
- `Backend` gets one method with a default, for the leader: it takes the plan of a batch (its pickle bytes, the bytes of its large records, its estimate, deferred or not), runs the waits of section 10, and returns the write of the path of the batch. The default returns `begin_write`, so the redb backend alone and the test backends of the crate keep the direct path.
- `WriteTxn` gets one method that stores a frame as it is, with a default that decodes the frame and calls the methods that exist. The redb write implements it, with the key of the index, the note of the count and the next oid for a record.
- `src/redb/write.rs`: that method, the correction of the count, and the mark of a deferred commit that no sync covered.
- `src/commit/group.rs`: the plan of a batch and the publication. `src/store.rs`: the open, the close, the read-only opens, `fold`, `backlog_bytes`, `cut_at_open` and the settings. `src/pack/mod.rs`: the wait. `src/kv.rs`: the wait of `kv_restore_durable`. `src/clock.rs`: `spawn`. `src/vfs/`: `Vfs::open_log`, `Dir::open_log`, and the mode of `OsFile` that writes at once and grows with zeros. `src/testing.rs`: the fold before each hook. `src/commit/observer.rs`: the two methods.
- zoe-server: the `SimVfs` and the `SimClock` of the simulator, the metrics and the ERROR line of a cut entry. zoe-convert: the finding `log` of `zoe verify`. zoe-bench: `tests/finish_time.rs` counts the files of `wal/` as it counts `zoe.redb`, and runs with `enabled` false as its baseline.

## Alternatives

1. **Records keyed by `(tid, oid)` with a second index (ZOE-174).** A load then needs a second lookup. Rejected before this record.
2. **The append-only backend of the specification.** It rebuilds its index at each open (N7), holds an index of every record in memory (N6), and needs its own verify and backup (`docs/spikes/record-store.md`). Here redb stays the store, and the log holds only the last commits.
3. **The log off by default, behind an option first, as the spike proposed.** No gate would measure it, and the crash simulation must cover both paths anyway.
4. **One entry for each transaction, as the prototype wrote them.** A batch would no longer be whole or absent. Two entries of one sync could land in any order, so a valid entry after a failed one would no longer prove that the failed one was synced.
5. **Large records skip the log one by one.** A batch would then be in two places, with no atomic commit across them. A direct batch moves a whole batch.
6. **All records through the log, with a slower fold at 100 KB.** Each record is then written twice, and under a steady load of large records the disk commits about half as many of them.
7. **Deferred restores through the log without a sync, as the spike proposed.** An entry without a sync would break the rule of the replay, and the converter would write each record twice.
8. **A row `folded_tid` in `meta`, as the spike proposed.** `last_tid` already is F, because only a slice and a direct batch write the tables of the records.
9. **Payloads instead of frames in the log.** The fold would build and check each frame again, and the reads of the table would build frames.
10. **A table of offsets into the segments, read with `pread`.** A load of the backlog would read a file. The frames of the backlog stay in memory instead, and `backlog_bytes` bounds them.
11. **64 shards of the table by hash, as this record first proposed.** They spread the lock word, but each range scan visits every shard, and the publication of a batch spans 64 locks. One lock is simpler, and the published tid keeps the reads correct whatever the locks.
12. **A read-only open that replays the log into memory, as the spike proposed.** Each tool would hold the backlog in memory and merge its reads, and the strict open could not tell a clean close. The clean close folds instead, and the open of the tools folds into `zoe.redb`, which ADR 0028 already lets it write.
13. **A close that leaves the log.** The close would be faster, but each read-only open would then meet a log.
14. **The leader folds after its acknowledgement.** The finish of that leader would pay a slice.
15. **The skins drive the fold, as they drive the sweep.** A store without such a thread, as in a test, the converter or zoe-bench, would fill its backlog and stall for ever.
16. **`fallocate` for the growth.** ext4 makes such blocks unwritten extents, and the first write into one changes metadata, which the sync then writes. Zeros give written blocks.
17. **A new file for each use, with no spares.** Each new file costs its zeros again.
18. **Slices without a sync, and one sync at the end of a fold.** F could then move only at the end of the fold, and redb keeps the pages of its last durable commit, so its file grows during a long fold.
19. **A new error variant for a damaged log.** The skins map each variant to an error of their clients. `CorruptTxn` already names a transaction that the store cannot give.
20. **A pack that folds up to T only.** The merged reads would give the records after T, but a pack whose mark and plan read the tables alone up to L is simpler to check.
21. **A header that fails reads as a spare, as this record first proposed.** The open would replay nothing of that segment and lose its acknowledged commits in silence, and the next use would write over the evidence.
22. **Two copies of the segment header.** They survive a damage of one copy, but two copies that a torn write leaves different need their own rule. The epoch in each entry gives the use of a damaged header from its first entry.
23. **A stamp in each sector of an entry**, to tell a torn write from a damage. Section 12 gives the reason for the bounded and reported route.
24. **A shorter `fold_age`, or none.** Section 9 gives the reason for 5 s.
25. **A duty cycle for the fold, which sleeps as long as it wrote.** It gives the device to the finishes for half of the time, but lets the backlog reach its bound under a steady load. The fold waits for the end of a sync that runs and no more; the measure of the card tells if more is needed.

## Where this design departs from the spike

1. The prototype wrote one entry with its own checksum for each member of a batch. Here a batch is one entry, so it stays whole or absent, as it is in the tables.
2. The prototype read the log up to the first bad entry. Here a damaged entry with a valid entry after it fails the open, a damaged header is read from its first entry, and an I/O error fails the open. Only the last entry can be cut, and the open reports it (section 12).
3. The prototype had no chain between the entries, so a missing segment lost commits in silence. Here each entry names the tid before it.
4. The prototype folded the whole queue in one backend write. Here a slice holds 4 MiB, so the fold stays far below the page cache of redb and holds the one writer of redb for a short time. The fold also runs on age.
5. The spike proposed that a read-only open replays the log into memory, and that `zoe verify` walks the tree and the log. Here a clean close folds the log, the strict open refuses a log with commits after F, the open of the tools folds the log into `zoe.redb`, and `zoe verify` reports a damaged log and verifies the tables.
6. The spike proposed a new folded tid in `meta`. Here it is `last_tid`.
7. The spike proposed the log behind an option first. Here it is on by default.
8. The spike left the large records open. Here a batch of mostly large records is a direct batch when the backlog is empty.
9. The prototype had one `BTreeMap` under one lock, and published it record by record. Here the table is under one lock too, a batch goes in under one write lock, and the published tid bounds each read.
10. The spike proposed that a deferred batch appends without a sync. Here a deferred batch is a direct batch, as before.
11. The spike proposed that `object_count()` adds the delta of the queue. Here one count in memory takes the new oids of a log batch at its finish, and each slice corrects it.
12. The prototype appended to a file that grows with each write, so each sync also wrote metadata of the file system. Here the fold grows a segment ahead of the leader and prepares spares, and the segments are reused.

## Consequences

- An `update` commit of 100 records of 1 KB with zstd syncs its entry alone, about 63 KB, and the store writes about 170 KB for it in all, against about 700 KB without the log. `crates/zoe-core/tests/write_log_bytes.rs` bounds these bytes.
- The work on the B-tree leaves the commit path: the leader encodes, writes and syncs one buffer.
- A load of a folded record pays one atomic read when the backlog is empty, and one read lock of the table when it is not. A load of a record of the backlog makes no lookup in redb.
- Memory: up to `backlog_bytes` of entries and indexes for each store, 256 MiB by default, on top of the page cache of redb. Settings bound it (N6).
- Disk: the log takes up to the backlog plus three segments for each store, about 450 MiB at most with the defaults. A quiet store takes less, because a segment grows only ahead of its entries.
- An open after a crash replays up to the bound of the backlog, about 0.4 s for 256 MiB; after a kill, the repair of redb comes first, as before (N7). After a clean close it reads the headers.
- A close takes the time of the fold of its backlog: at most a few seconds with the defaults (the spike folded 256 MB in 2.9 s), which the stop of `zoe serve` allows. With `fold_age`, a quiet store has almost nothing to fold.
- Each writable store has one more thread.
- Tests and fixtures that copy or damage the files of a store must include `wal/`, or fold first with `Store::fold`. The hooks of `zoe_core::testing` fold first.
- The specification changes in the data model (`wal/`, the format name), durability, N13, group commit, the database file, memory and startup, the storage trait, the read-only opens, `zoe verify` and the metrics. The invariants of `crates/zoe-core/CLAUDE.md` change for the finish, the open, the close, the reads, pack, the count, the threads, the logs and the format. ADR 0020 (the log carries the frames), ADR 0028 (the repair folds the log) and ADR 0033 (the count in memory) get a line each.

## Known limits

- The exception of section 12: after a crash, a damaged last entry of the log is cut and reported, not detected as damage. At most `fold_age` of commits are exposed.
- A pack waits for the fold of the backlog before its mark: at most the fold of `backlog_bytes`, a few seconds.
- A batch of large records that finds a backlog goes to the log, so a mixed load still writes some large records twice.
- The syncs of the fold and of the finishes share the device. The fold avoids only the overlap with a sync that runs.
- Old bytes of folded entries stay in the spares until a new use writes over them, as freed pages of redb keep old bytes. A pack does not erase them.
- A store whose log is damaged opens for no writer and no tool but `zoe verify`. No tool cuts the log at the damage: a restore from the dumps is the way back.
