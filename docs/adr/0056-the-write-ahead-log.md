# 0056. A write-ahead log with a background fold: ZOE format 3

Status: proposed. Card ZOE-272. The decision to build the log, the name `ZOE format 3` and the refusal of a store of format 2 are settled. This record gives the design for review. It changes the on-disk format and pack, two red zones. It changes no wire message and no public Python API.

## Context

ZOE keeps its records in redb, a copy-on-write B-tree, under the key `(oid, tid)`. Each record of a commit lands in the leaf of its oid. So a commit copies about two pages of 4 KB for each record that it changes, plus 12 to 17 fixed pages (`docs/spikes/server-write-volume.md`). For one B5 operation, `zoe serve` gives 5.7 to 6.6 times more bytes to its write calls than FileStorage appends, and the finish syncs all of them.

The spike ZOE-184 (`docs/spikes/write-log.md`, verdict "Go with conditions") built a write-ahead log with a background checkpoint. For an update of 100 records of 1 KB, the commit synced 63 KB, and the store wrote 168 KB in all, against 696 KB written and synced without the log. The finish of that update fell from 4.5 ms to 1.5 ms at concurrency 1, and the commits per second rose from 274 to 695 at concurrency 16, on a disk whose sync costs by the byte. At 100 KB, the log wrote each record twice, and its checkpoint competed with the commits for the device.

The layout `(tid, oid)` with a second index (ZOE-174) also wrote fewer bytes. A load then needs a second lookup, so that layout was rejected.

The design keeps:

- N1: a finish returns only after its commit is durable. A crash at any point loses no acknowledged commit and leaves no part of a commit.
- N2: a reader sees all of a commit or none of it, and `loadBefore` is consistent at any tid.
- One lookup for each load: a record that the tables do not hold yet comes from memory, with no lookup in redb.
- N6 and N7: settings bound the memory, and an open after a clean close reads a fixed amount of data.
- The invariants of `crates/zoe-core/CLAUDE.md`: a synchronous core, the time and the files only through `Clock` and `Vfs`, the commit sequencer, group commit, the lock order.

## Words

| Word | Meaning |
| --- | --- |
| the log | the write-ahead log: the files of `wal/` in the store directory |
| segment | one file of the log |
| use, epoch | a use is the entries that a segment gets after a write of its header; the epoch is the number in that header, which names the use |
| entry | the record of one commit batch in a segment |
| the tables | the redb tables of the store, as before this record |
| the fold | the thread of the core that writes the commits of the log into the tables; the spike calls it the checkpoint, a word that `CONTEXT.md` keeps for a snapshot |
| slice | one backend write of the fold |
| folded tid, F | the last tid that the tables hold durably |
| backlog | the commits after F: the log and the unfolded table hold them |
| unfolded table | the index in memory of the records and the namespace writes of the backlog |
| direct batch | a commit batch that the leader writes into the tables, as each batch before this record |

## Decision

A finish appends its commit batch to a segment of the log and syncs that segment alone. The records of the batch go into the unfolded table, which every read path reads before the tables. The fold writes the commits of the log into the tables later, in slices of a few MB, each a durable backend write. The backlog is bounded: a finish that meets the bound waits for the fold. A batch whose records are mostly large, a batch larger than half of the bound and a batch of deferred restores are direct batches. A clean close folds the whole log. The format name becomes `ZOE format 3`. The log is on by default.

### 1. The files of the log

- The log is the directory `wal/` in the store directory. A writer open creates it when it is missing, before the sync of the store directory that each writer open makes. A read-only open reads a missing `wal/` as an empty log.
- A segment is a regular file `wal/<n>.wal`, where `<n>` is 16 lower-case hexadecimal digits. A new file takes one more than the greatest number in `wal/`. The name tells nothing else: the headers order the segments. The open leaves every other entry of `wal/` as it is.
- Each operation on the log goes through the `Vfs`. A segment is a file of `Vfs::open`: on `OsVfs`, it holds its writes until the sync, and a growth gets zeros. The crate trait `Dir` gets `open`. A read-only open reads with `Vfs::read_file`.

### 2. A segment

The header takes bytes 0 to 31. Integers are little-endian.

| Bytes | Field |
| --- | --- |
| 0–7 | magic `ZOE-WAL\0` |
| 8–11 | version, `u32`: 1 |
| 12–15 | state, `u32`: 1 in use, 2 spare |
| 16–23 | epoch, `u64` |
| 24–31 | xxh3-64, seed 0, of bytes 0 to 23 |

- A header that fails its checksum, zeros included, is a spare without an epoch. A header that passes with another version fails the open with `Format`.
- Each new use takes an epoch greater than each epoch that the store gave before: one more than the greater of the greatest epoch seen since the open and the last tid of the store. The open reads the greatest epoch from each header that passes, spares included. The epoch salts the checksums of the entries of its use, so an entry of an earlier use of the same file never passes in a later use. The epochs also order the segments: a segment used later has a greater epoch.
- A segment in use holds entries from byte 32. A spare holds no entry that counts.

### 3. An entry

One entry is one commit batch: each member of the batch that got a tid, in batch order. An entry starts at a multiple of 8.

| Bytes | Field |
| --- | --- |
| 0–3 | magic `ZWAL` |
| 4–7 | op count, `u32` |
| 8–15 | body length, `u64` |
| 16–23 | previous tid, `u64`: the last tid of the store before the batch |
| 24–31 | last tid, `u64`: the tid of the last commit of the batch |
| 32–39 | body checksum: xxh3-64, seed 0, of the epoch (8 bytes), bytes 0 to 31 and the body |
| 40–47 | header checksum: xxh3-64, seed 0, of the epoch and bytes 0 to 39 |

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
- The header checksum lets the replay trust the body length before it reads the body, and lets the scan of section 11 find the next entry after a damaged one.

### 4. Where a batch goes

The leader takes one of two paths, under the commit sequencer, after it gave the tids. It decides from the lengths of the staged pickles, which the index of the staging holds, before it encodes anything.

- The log, by default.
- A direct batch: one backend write of the tables, with the code of a slice. A batch is direct when:
  1. each member with a tid is a deferred restore. The write commits without a sync (`commit_deferred`), as before.
  2. the tables hold a deferred commit that no sync covered yet. The write commits with a sync, which makes that commit durable, as before.
  3. records of at least `large_record` bytes (64 KiB by default) hold at least half of the pickle bytes of the batch, and the backlog is empty.
  4. the pickles of the batch hold more than half of `backlog_bytes`.
  5. the log is off (`WriteLog::enabled` false).
- A direct batch needs an empty backlog, so that the tables hold each commit before it. The backlog is empty when the table holds no commit: a slice removes its commits from the table before it removes them from the backlog. So a direct batch never leaves an older version of an oid in the table above its own version in the tables. In cases 1, 4 and 5, the leader waits until the fold has emptied the backlog (section 10). Case 2 always finds an empty backlog: a deferred commit is direct, and no batch went to the log since. In case 3, a batch that finds a backlog goes to the log and asks for a fold, so the next large batch finds an empty backlog.
- Large records skip the log because a large record fills its own pages: the tables write it about once (1.28 times its frame in the spike), and the log would write it a second time. The spike measured more bytes and a slower finish at 100 KB with the log. A direct batch writes the bytes of today, with one sync.
- A batch over half of the bound skips the log because the unfolded table holds each frame of the backlog in memory: such a batch would take the memory of the backlog alone.
- Deferred restores skip the log because the converter of `Data.fs` writes millions of records with them, and the log would write each record twice. Also, each entry of the log is synced before the next entry is written. The replay relies on this rule (section 11), and an entry without a sync would break it.

### 5. A finish

The leader, under the commit sequencer:

1. Gives the tids and checks them, as before.
2. Chooses the path. On the log path, it waits while the backlog is not empty and the backlog and the batch would pass `backlog_bytes` (section 10).
3. Places the blob files: the intent file in `blobs/tmp/`, the renames and the syncs of their directories, as before the backend write.
4. Encodes the frames of each member into one buffer, the entry. An attached payload goes into its frame as it is.
5. Starts a new use of a segment when the entry does not fit, and grows the segment when it must (section 6).
6. Writes the entry with one write, then syncs the segment file (`sync_data`). This sync makes each member durable.
7. Publishes the batch: the records, the blob records, the transaction rows and the namespace writes go into the unfolded table and the backlog, and the new oids into the object count (section 18). Then it stores the last tid. A reader that sees a tid sees each commit up to it.
8. Removes the intent files, notes the oids for a running pack and gives the members their outcomes, as before. The observer gets `batch_synced`.
9. Wakes the fold when the backlog reaches `fold_bytes`.

A failure in steps 3 to 6 fails each member and halts the store, as a failed backend write does. The table gets none of the members. Their outcome is unknown until the next open, which replays the entry when it is whole on the disk.

A direct batch runs steps 1 to 3, then writes the tables in one backend write, with or without a sync, as before. Then the folded tid becomes its last tid, and the leader stores the last tid.

### 6. Segments: growth, new uses and spares

- An entry goes into the segment of the current use when it is the first entry of the use, or when its end stays within `segment_bytes`. Else the leader starts a new use. So a segment holds at least one entry, and an entry larger than `segment_bytes` makes its segment larger.
- A new use takes a spare when one exists, else a new file. The leader creates a new file, then syncs `wal/`, before its first entry. A new use writes the header (state 1, a new epoch) and its first entry at byte 32 with one write, so the sync of the entry covers both.
- The first entry after an open always starts a new use. So no entry ever follows the bytes that a crash left at the end of a segment.
- Before a write past the end of the file, the leader sets a new length: the old length plus a step, and at least the end of the entry. The step is the old length, at least 1 MiB and at most 8 MiB. The new length stays at or below `segment_bytes` unless the entry needs more. On `OsVfs` a growth of a file of `Vfs::open` gets zeros, so a later append changes no block map, and its sync writes no metadata of the file system. A reused segment keeps its length and does not grow.
- A segment retires when the fold has folded each entry of its use and the segment is not the one of the current use. It becomes a spare in memory: its header stays on the disk until a new use writes over it. At most two spares stay. The fold removes the other retired files, with no sync of `wal/`: a removed file that a crash brings back holds folded entries only.
- The spares exist so that the log writes no zeros once it reaches its size.

### 7. The unfolded table and the backlog

- The backlog holds the commits after F in tid order: for each commit, its transaction row, the oids of its records, its namespace writes and its size.
- The size of the backlog, which `Store::backlog_bytes` gives and `backlog_bytes` bounds: for each commit, the bytes of its ops in the entry, 64 bytes for each op for the indexes in memory, and, with the first commit of an entry, the header and the alignment of the entry.
- The unfolded table:
  - the records: 64 shards, by `oid % 64`, each a `RwLock<BTreeMap<Oid, versions>>`. The versions of an oid are those of the backlog, in tid order. Each gives its tid and the places of its frame and of its blob record.
  - the namespace rows: one `RwLock<BTreeMap>` from the flavour and the key of a row to the last op of the backlog on it, a write or a removal, with its tid.
  - the expiry keys of the backlog, with their tids.
  - counters: the commits of the backlog, its blob records, and its namespace writes of each flavour.
- The frames stay in the buffer of their entry. The commits of an entry share it through one `Arc`, and the table holds offsets into it. No frame is copied after its encode. The buffer leaves memory when the fold has folded its last commit.
- The suffix rule: for each oid, each version in the table has a greater tid than each version of the tables that the table does not also hold. The fold keeps the rule: it folds commits in tid order, and a commit leaves the table only after the tables hold it durably. So when the table has a version of an oid before a tid, the newest version of the oid before that tid is in the table.
- When the backlog is empty, a read checks one atomic counter and reads the tables, with no lock of the table.

### 8. The reads

Two rules hold for each read path:

- The table first, then the tables. A commit leaves the table only after the tables hold it, so a read that misses a commit in the table finds it in the tables. A read of the tables first could miss a commit that a slice moves between its two reads.
- A scan merges the two sources in key order, and a commit or a version that both give counts once.

| Read | Rule |
| --- | --- |
| `load_before`, `load_before_next` | The versions of the oid in the table give the newest before the tid and the first at or after it. When the table has a version before the tid, the read ends there, with no lookup in redb. Else one read transaction of the tables gives the record and its next serial, as before, and the next serial is the lower of the two sources. |
| `load_serial`, `record_info`, `load_blob` | A point read of the table, then of the tables. |
| `current_serials`, the vote | The newest version of each oid in the table; one read of the tables for the other oids of a chunk, as before. |
| `history` | The versions of the table, newest first, then those of the tables after them. |
| `current_from`, `record_iternext` | The first oid at or after the bound in the table, from a range read of each shard, and in the tables; the lower one wins. Its newest record comes from the table when the table holds the oid. |
| `record_keys` | The keys of both sources in key order; the list still stops after the first oid that brings it to `limit` keys. |
| `iter_txns`, `indexed_tids`, `txn_oids`, `txn_records`, `txns_before`: the iterator, the undo log, undo | The backlog gives the commits after F in tid order, and the tables the others. A merge copies the part of the backlog in its range first, then reads the tables. A slice of `indexed_tids` still holds at most `limit` tids, so the iterator keeps its bound of memory. A commit without a record has no index key in either source. |
| `has_blobs`, `has_kv` | The tables, or the counters of the table. |
| `kv_row` | The last op of the row in the table: a write gives its frame, a removal gives no row. Else the tables. |
| `kv_rows` | The rows of the tables in the range, with the ops of the table on them: a write replaces or adds a row, a removal hides one. The call gives `limit` rows unless the range holds fewer: when the table hides rows, it reads more rows of the tables. `Store::kv_scan` relies on this, because a chunk with fewer rows than it asked for ends its scan. |
| `kv_expiry_has` | The table, then the tables. |
| `object_count`, `last_tid` | Section 18. |
| `pack_watermark`, `sequence_end`, `sequence_names` | The tables alone: no commit of the log writes them. |

- Each frame from the table passes its checksum, under its key space and its key, before a read uses it, as a frame of the tables does (invariant 99 of the core).
- A read-only store reads the tables alone: its open checks that the log holds no commit after F (section 13).
- The hooks of `zoe_core::testing` that read or change stored bytes first fold the whole backlog, then work on the tables as before.

### 9. The fold

- The fold runs on one thread for each writable store, which the open starts with `Clock::spawn` (section 23). A read-only store has none.
- It starts when the backlog reaches `fold_bytes`, or on a request: `Store::fold`, a pack, a leader that waits for the fold, a large batch that went to the log, `Store::kv_restore_durable` and the close.
- A slice takes the oldest commits of the backlog, whole commits, until their bytes reach `slice_bytes`, and at least one commit. One backend write of the tables stores their frames as they are, with a new method of `WriteTxn` that also writes the key of the transaction index, notes the oid for the count and raises the next oid for a record, as `put_payloads` does. It stores the namespace rows, removals and expiry keys, then `last_tid`, `next_oid` and `object_count` in `meta`, and commits with a sync. The write counts the objects as a batch write did before (ADR 0033), so the count in `meta` stays exact.
- After the commit of a slice, the folded tid becomes the last tid of the slice. Then the slice leaves the table (each version, row and key whose tid is in the slice) and the backlog. Then the object count gets its correction (section 18), the segments whose entries are all folded retire, the waiters wake, and the observer gets `slice_folded`. The fold goes on with the next slice while the backlog is at or above `fold_bytes` or a request waits.
- The fold holds the one writer of redb for one slice at a time. So a sequence reservation, a sweep step or a pack batch waits for one slice at most, and a slice stays far below the page cache of redb. The spike measured 1.16 to 1.91 bytes in the tables for each byte of frame with folds of 200 to 384 commits. A slice of 4 MiB holds about 60 commits of 100 records of 1 KB with zstd.
- Before the commit of a slice, the fold waits for the end of a log sync that runs, if one runs. It does not wait for the next one. So the sync of a slice does not start during the sync of a finish, and a run of finishes cannot starve the fold. When the fold falls behind, the bound of the backlog stops the finishes.
- The fold never takes the commit sequencer.

### 10. A full backlog

- A leader on the log path waits while the backlog is not empty and the backlog and its batch would pass `backlog_bytes`. It estimates the size of the batch in the backlog before it encodes: the lengths of the pickles, of the namespace writes and of the metadata, with the fixed part of each op and of the entry. A frame is never longer than its pickle and its header, so the estimate is never below the size, and the backlog never passes its bound. The leader asks for a fold and waits on a waiter of the store clock, which each slice wakes. It keeps the commit sequencer while it waits, as during a sync: the next batch would wait for the same room, and the commits keep their arrival order. A full backlog stalls the finishes and fails none of them.
- A leader that waits for an empty backlog (section 4) waits the same way.
- The observer gets `backlog_waited` with the time of each wait, on the monotonic time of the store clock.
- A failed fold wakes the waiters, and their batches fail with `Halted` (section 19).

### 11. The open of a writer

1. Open `zoe.redb` and check the format, as before. F is the last tid of the tables: the greater of `last_tid` in `meta` and the key of the last transaction row.
2. Create `wal/` when it is missing.
3. Read the header of each segment. The segments whose header passes and says "in use" go in epoch order; the others are spares.
4. Replay each segment in use, in epoch order, from byte 32, one entry after the other:
   - A header that passes its checksum gives the body length. An entry whose last tid is at or below F is folded: the replay checks its header only and goes to the next entry.
   - An entry with a commit after F must pass its body checksum, and must follow the commit before it. The first such entry holds F as its previous tid or as the tid of one of its commits. Each later entry has as its previous tid the last tid of the entry before it. Its commits after F go into the table and the backlog, as a publication does.
   - A broken chain fails the open with `CorruptTxn`, with the tid that the entry names as its previous tid and that the log does not hold. A segment that someone removed gives this.
   - At an entry that fails a checksum, or that the end of the file cuts, the replay looks for the next entry of the same epoch in the rest of the segment: at each multiple of 8, an entry whose two checksums pass. When it finds none, the bytes from the failed entry on are the torn tail of the segment, and the replay goes to the next segment. When the next entry has a previous tid at or below F, the failed bytes held folded commits only, and the replay goes on from that entry. When the next entry has a previous tid after F, the failed bytes held synced commits, and the open fails with `CorruptTxn`, with that previous tid.
   - These rules hold because the leader writes an entry only after the sync of the entry before it, and starts a new use only after the sync of the last entry of the use before. So only the last entry of the log can be cut by a crash, and a valid entry after a damaged one proves the damage.
5. A segment in use whose entries are all at or below F retires: it is a spare. The last tid of the store is the greater of F and the last tid of the replay. The next oid is the greater of `next_oid` in `meta` and one more than the greatest oid of the replay. The greatest epoch comes from all the headers.
6. The recovery of `blobs/tmp/` runs after the replay, so a blob record of the log counts: the open keeps the file of such a record.
7. Start the fold. When the backlog is at or above `fold_bytes`, the fold starts at once.

Reads go on while the fold runs: the table serves the replayed commits. The replay reads the entries after F, at most the bound of the backlog and one entry, and the headers of the folded entries of the segments in use. The spike measured 1.3 to 1.6 ms for each MB of log, so a full backlog of 256 MiB replays in about 0.4 s (N7). After a clean close, the open reads 32 bytes of each segment file.

The replay writes nothing. A crash during the open, during the fold after it or right after it loses nothing: the log still holds each commit after F, and F moves only with a durable slice.

### 12. The close

The drop of a writable store that did not halt:

1. Stops the fold thread after its slice, and waits for its end.
2. Folds the whole backlog on the thread of the drop, slice by slice.
3. Writes the state "spare" into each header that says "in use", the headers of retired segments included, with its epoch, and syncs each such file.
4. Removes the spare files beyond two.
5. Closes the database, as before.

A halted store, or a failure in steps 2 to 4, skips the remaining steps of the log: the log stays, and the next open replays it. A WARN event names the error. The drop never panics for it.

So after a clean close, the tables hold each commit, and each header of the log says "spare". The headers keep their epochs, so the next uses get greater ones. A crash during the close loses nothing: a header says "spare" only after the fold has made the tables durable.

### 13. The read-only opens

- A read-only open reads the headers of `wal/`, and walks the entries of each segment in use, header by header, as the replay does. It writes nothing.
- `Store::open_read_only`, the strict open, fails with `NotClosedCleanly` when a segment in use holds a commit after F. After a clean close each segment is spare, so the open reads 32 bytes of each file.
- `Store::open_read_only_repairing`, the open of the tools (ADR 0028), takes the writer path of its repair when redb needs its repair or when the log holds a commit after F. It opens `zoe.redb` for writes, redb repairs the file, the log is replayed and folded into `zoe.redb` slice by slice with syncs, and the database closes cleanly. Then the read-only open runs, and `repaired` gives true. The repair writes only `zoe.redb`: it leaves the segment files, `blobs/tmp/` and `staging/` as they are. The entries of the log are then all at or below F, so a later strict open reads the store.
- So `zoe verify`, `zoe backup`, `zoe convert --to-data-fs`, `zoe pack --dry-run` and `zoe mcp` read the tables alone, and their code does not change.

### 14. Pack

- Under the commit sequencer, the pack reads the last tid L and starts the notes of the finishes, as before. Then it releases the sequencer, asks for a fold, and waits until F is at or after L. Then it marks and removes as before.
- So the tables hold each record at or before T, and the pack removes rows of the tables only. It keeps each record after T, in the backlog or in the tables. It reads the records that the finishes note during the pack through the merged reads.
- A pack batch and a slice share the one writer of redb. A finish never waits for a slice.
- The dry run asks for no fold. It reads through the merged reads and writes nothing.
- The pack watermark stays in `meta`, and `check_undone` reads it as before.
- The wait is in `src/pack/`, so the card touches that red zone too.

### 15. Blobs

- The blob files go into place before the write of the entry, the intent file first, as before the backend write. The blob records travel in the entry. So a durable blob record always has its file.
- The finish removes its intent file after the sync of its entry.
- The open replays the log before the recovery of `blobs/tmp/`, so a file that an intent file names stays when the log or the tables hold its blob record.
- A blob read starts from the blob record, in the table, then in the tables, and checks the file as before. A blob undo links the file as before.
- A pack removes blob files after its batch, as before: each of its records is folded.

### 16. Group commit, the lock hand-off, deferred restores and `Store::sync`

- Group commit does not change: one leader, one entry and one sync for each batch. A batch is whole or absent in the log, as it is in the tables.
- `batch_synced` gives the commits that one sync of an entry or of a direct batch made durable.
- The lock hand-off (ADR 0044): a commit in flight is durable when its entry is synced, and its hand-off settles then. A vote that waits for that commit then reads the table.
- Deferred restores are direct batches (section 4). The first batch after them that has another member is a direct batch with a sync.
- `Store::sync` does not change: the barrier write of the tables, with a sync. The log holds no entry that a sync does not cover.
- The members of a batch release their locks after the sync, and the invalidations go out after the acknowledgement, as before.

### 17. KV namespaces, sequences and the sweep

- The namespace writes of a member go into its entry after its records, as ops 4 to 8, with the frames that the leader wrote into the tables before: the expiry of an ephemeral row is the time of the write of the batch plus its TTL, and each durable namespace that the member writes gets its generation row.
- A read gives the last committed value: the last op in the table, else the tables. An expired row reads as absent, also from the table.
- A sweep step stays a backend write of its own outside the commit sequencer, and reads and removes rows of the tables only. A row that the backlog writes again is in the table, and reads see it; the fold then writes it. A removal in the backlog of a row that a sweep removed removes an absent row, which stays absent.
- `Store::kv_restore_durable` asks for a fold and waits for an empty backlog before its write. So an older write of the log never lands over a restored row.
- Sequences never go through the log: a reservation and `Sequence::restore_end` stay backend writes of their own. Each waits for one slice at most.

### 18. The object count, the next oid and the last tid

- The backend keeps one count in memory, as before. A log batch adds, for each member, its records whose previous serial is `Tid::ZERO`: the vote checks the serial of each written oid under its lock, so only an oid without a record gets such a record. Each backend write, a slice, a direct batch or a pack batch, adds its own change of the count, less the oids that the log already counted for the records of the write. The count in `meta` stays exact in each snapshot (ADR 0033), because the slice reads the records as a batch write did.
- One case leaves the count in memory wrong until the fold: a pack batch that removes every record of an oid between the vote and the finish of a log commit that writes the oid. The log then counts no new oid, and the slice that folds the commit counts it. Only `len()` and the `length` of `zoe serve` show the difference, for that time.
- A finish raises the next oid in memory before its write, as before. The slice writes `next_oid` to `meta`, and the open takes the greater of `meta` and the replay.
- The leader stores the last tid in memory after the publication of a log batch, or after the backend commit of a direct batch. `last_tid` in `meta` is F: only a slice and a direct batch write it.

### 19. Errors and the halt

- A failed write or sync of an entry, a failed growth or a failed new use fails the members and halts the store, as a failed backend write does: the commit sequencer is empty. The table gets none of the members.
- A failed slice marks the fold failed, wakes the waiters and stops the fold thread. The next lock of the commit sequencer sees the mark and halts the store. Later finishes, packs and calls of `Store::fold` fail with `Halted`. The table keeps each commit, so the reads go on, and the next open replays the log. The fold never takes the commit sequencer, so a leader that waits for the fold under the sequencer never waits for itself.
- A panic of the fold thread does the same: a guard of the thread marks the failure as it unwinds.
- No step retries a failed sync: the state of the data is unknown after it, and the next open replays what the disk holds.
- An open with a damaged log that hides an acknowledged commit fails with `CorruptTxn`, and a log of another version with `Format`. No new error variant: `CorruptTxn` names the transaction that the store cannot give.

### 20. The format name

- `format::FORMAT_NAME` becomes `ZOE format 3`. A new store starts with it.
- An open of a store of `ZOE format 2`, or of any other name, fails with `Format` before it creates `wal/` or writes anything, as for a store of format 1. No migration exists: `zoe convert` through `Data.fs` moves the data.
- A build of format 2 refuses a store of format 3 by its name. So no older build writes the tables of a store whose log holds commits that the tables do not hold.
- The tables, their key spaces, the frames and `meta` keep their layout. The name, `wal/` and its files are new.

### 21. The settings

`Options::write_log` is a `WriteLog`:

| Field | Default | Meaning |
| --- | --- | --- |
| `enabled` | true | the log path; false makes each batch direct |
| `segment_bytes` | 64 MiB | the size at which a segment stops to grow, at least 4,096 |
| `fold_bytes` | 16 MiB | the backlog at which the fold starts |
| `slice_bytes` | 4 MiB | the most bytes of entries for one slice |
| `backlog_bytes` | 256 MiB | the bound of the backlog, at least `fold_bytes` |
| `large_record` | 64 KiB | the pickle size of a large record |

- An open with settings that do not hold together fails with an `InvalidInput` I/O error.
- With `enabled` false, the open still replays and folds a log that exists.
- The log is on by default. The gates of the project (the native gate, B5) measure `zoe serve` with its defaults, so a log off by default would show nothing there. The crash simulation must cover the direct path in any case (large records, deferred restores). One default keeps one matrix of tests.
- The skins do not expose the settings in this card: no key of `<zoe>`, no flag of `zoe serve`, no keyword of `zoe.Storage`. Tests and benchmarks set them through `Options`.
- `Store::fold` and `Store::backlog_bytes` are new public methods of the core.

### 22. Metrics, spans and logs

- The observer gets two methods, each with a default that does nothing: `slice_folded(commits, bytes, duration)` after each slice, and `backlog_waited(wait)` after each wait of a leader for the fold. `batch_synced` stays.
- `zoe serve` exports them with the label `storage`: `zoe_fold_seconds`, a histogram of the slices; `zoe_fold_commits_total`; `zoe_backlog_wait_seconds`, a histogram of the waits; and the gauge `zoe_backlog_bytes`, read from `Store::backlog_bytes` at each scrape.
- The span `fsync` covers the write and the sync of an entry or of a direct batch, as before. A new INFO span `fold` with the field `commits` covers each slice.
- A failed slice and a close that cannot fold give a WARN event with the tid and the error, never bytes of a record.

### 23. Threads, locks and the clock

- `Clock` gets `spawn(name, work)`, with a default that starts a thread of the system. The simulator of zoe-server gives its own, so the fold runs under its baton. The fold waits only on waiters of the store clock.
- The fold thread holds the shared state of the log and the backend, never the store, so the drop of the store runs. The close waits for the end of the thread on a waiter, not on a join handle.
- The new locks: the mutex of the current segment and of the spares (the leader under the commit sequencer, the fold to retire a segment), the locks of the shards, of the namespace rows, of the expiry keys and of the backlog, and the mutex of the requests to the fold. Each is held with no other lock taken under it, so the lock order of the core (invariant 98) gets no new edge. The leader takes the shard locks one at a time.
- The core stays synchronous: the fold is a thread of the system, with no async code.

### 24. The crash simulation and the simulator

- Each file operation of the log goes through the `Vfs`. So `MemVfs::crash_images` covers the writes and the syncs of the entries, the growths, the creations of files and of `wal/`, the headers of the new uses and of the close, and the removals.
- The acceptance tests open each crash image of the finishes across segments and into a spare, of a slice, of the open that replays a log and of the fold after it, and of a clean close.
- The `SimVfs` of the simulator of zoe-server treats each file of `wal/` as it treats the database file: the writes wait in a cache until the sync, and a sync gives the baton back and is a crash point. `SimClock::spawn` runs the fold as a thread of the simulator. The crash phase "finishing" is then the sync of an entry.

### 25. Where the code goes

- `src/format/wal.rs`, a red zone: the layout of a segment header, of an entry and of its ops, with their encode, decode and checks. `FORMAT_NAME` in `src/format/mod.rs`.
- `src/wal/`: the logged backend. It implements `Backend` over the redb backend, and holds the segments, the unfolded table, the backlog, the replay and the fold.
- `Backend` gets one method with a default, for the leader: it takes the plan of a batch (its pickle bytes, the bytes of its large records, its size estimate, deferred or not), runs the waits of section 10, and returns the write of the path of the batch. The default returns `begin_write`, so the redb backend alone and the test backends of the crate keep the direct path.
- `WriteTxn` gets one method that stores a frame as it is, with a default that decodes the frame and calls the methods that exist. The redb write implements it, with the key of the index, the note of the count and the next oid for a record.
- `src/redb/write.rs`: that method, the correction of the count, and the mark of a deferred commit that no sync covered.
- `src/commit/group.rs`: the plan of a batch and the publication. `src/store.rs`: the open, the close, the read-only opens, `fold`, `backlog_bytes` and the settings. `src/pack/mod.rs`: the wait. `src/kv.rs`: the wait of `kv_restore_durable`. `src/clock.rs`: `spawn`. `src/vfs/mod.rs`: `Dir::open`. `src/testing.rs`: the fold before each hook. `src/commit/observer.rs`: the two methods.
- zoe-server: the `SimVfs` and the `SimClock` of the simulator, and the metrics. zoe-bench: `tests/finish_time.rs` counts the files of `wal/` as it counts `zoe.redb`, and runs with `enabled` false as its baseline.

## Alternatives

1. **Records keyed by `(tid, oid)` with a second index (ZOE-174).** A load then needs a second lookup. Rejected before this record.
2. **The append-only backend of the specification.** It rebuilds its index at each open (N7), holds an index of every record in memory (N6), and needs its own verify and backup (`docs/spikes/record-store.md`). Here redb stays the store, and the log holds only the last commits.
3. **The log off by default, behind an option first, as the spike proposed.** No gate would measure it, and the crash simulation must cover both paths anyway.
4. **One entry for each transaction, as the prototype wrote them.** A batch would no longer be whole or absent. Two entries of one sync could land in any order, so a valid entry after a failed one would no longer prove that the failed one was synced: a damaged entry could pass for a torn tail.
5. **Large records skip the log one by one.** A batch would then be in two places, with no atomic commit across them. A direct batch moves a whole batch.
6. **All records through the log, with a slower fold at 100 KB.** Each record is then written twice, and under a steady load of large records the disk commits about half as many of them.
7. **Deferred restores through the log without a sync, as the spike proposed.** An entry without a sync would break the rule of the replay, and the converter would write each record twice.
8. **A row `folded_tid` in `meta`, as the spike proposed.** `last_tid` already is F, because only a slice and a direct batch write the tables of the records.
9. **Payloads instead of frames in the log.** The fold would build and check each frame again, and the reads of the table would build frames.
10. **A table of offsets into the segments, read with `pread`.** A load of the backlog would read a file. The frames of the backlog stay in memory instead, and `backlog_bytes` bounds them.
11. **One lock for the whole table, as the prototype had.** Each load of each thread would take the same lock word. 64 shards spread it. A hash map in each shard would serve a point read faster, but `record_iternext` and `record_keys` need the order of the oids.
12. **A read-only open that replays the log into memory, as the spike proposed.** Each tool would hold the backlog in memory and merge its reads, and the strict open could not tell a clean close. The clean close folds instead, and the open of the tools folds into `zoe.redb`, which ADR 0028 already lets it write.
13. **A close that leaves the log.** The close would be faster, but each read-only open would then meet a log.
14. **The leader folds after its acknowledgement.** The finish of that leader would pay a slice.
15. **The skins drive the fold, as they drive the sweep.** A store without such a thread, as in a test, the converter or zoe-bench, would fill its backlog and stall for ever.
16. **`fallocate` for the growth.** ext4 makes such blocks unwritten extents, and the first write into one changes metadata, which the sync then writes. Zeros give written blocks.
17. **A new file for each use, with no spares.** Each new file costs its zeros again.
18. **Slices without a sync, and one sync at the end of a fold.** F could then move only at the end of the fold, and redb keeps the pages of its last durable commit, so its file grows during a long fold.
19. **A new error variant for a damaged log.** The skins map each variant to an error of their clients. `CorruptTxn` already names a transaction that the store cannot give.
20. **A pack that folds up to T only.** The merged reads would give the records after T, but a pack whose mark and plan read the tables alone up to L is simpler to check.
21. **The epoch as the first tid of its use.** After a crash that lost the first entry of a use, the next use can get the same value, and the old bytes of that file could pass. The epoch is above each epoch and each tid that the store gave.
22. **A duty cycle for the fold, which sleeps as long as it wrote.** It gives the device to the finishes for half of the time, but lets the backlog reach its bound under a steady load. The fold waits for the end of a sync that runs and no more; the measure of the card tells if more is needed.

## Where this design departs from the spike

1. The prototype wrote one entry with its own checksum for each member of a batch. Here a batch is one entry, so it stays whole or absent, as it is in the tables.
2. The prototype read the log up to the first bad entry. Here a damaged entry with a valid entry after it fails the open, so a damaged byte never hides synced commits in silence, except in the last entry (Known limits).
3. The prototype had no chain between the entries, so a missing segment lost commits in silence. Here each entry names the tid before it.
4. The prototype folded the whole queue in one backend write. Here a slice holds 4 MiB, so the fold stays far below the page cache of redb and holds the one writer of redb for a short time.
5. The spike proposed that a read-only open replays the log into memory, and that `zoe verify` walks the tree and the log. Here a clean close folds the log, the strict open refuses a log with commits after F, and the open of the tools folds the log into `zoe.redb`.
6. The spike proposed a new folded tid in `meta`. Here it is `last_tid`.
7. The spike proposed the log behind an option first. Here it is on by default.
8. The spike left the large records open. Here a batch of mostly large records is a direct batch when the backlog is empty.
9. The prototype had one `BTreeMap` under one lock. Here the table has 64 shards.
10. The spike proposed that a deferred batch appends without a sync. Here a deferred batch is a direct batch, as before.
11. The spike proposed that `object_count()` adds the delta of the queue. Here one count in memory takes the new oids of a log batch at its finish, and each slice corrects it.
12. The prototype appended to a file that grows with each write, so each sync also wrote metadata of the file system. Here a segment grows by steps of zeros and is reused.

## Consequences

- An `update` commit of 100 records of 1 KB with zstd syncs its entry alone, about 63 KB, and the store writes about 170 KB for it in all, against about 700 KB without the log. `crates/zoe-core/tests/write_log_bytes.rs` bounds these bytes.
- The work on the B-tree leaves the commit path: the leader encodes, writes and syncs one buffer.
- A load of a folded record pays one atomic read when the backlog is empty, and a range read of one shard when it is not. A load of a record of the backlog makes no lookup in redb.
- Memory: up to `backlog_bytes` of frames and indexes for each store, 256 MiB by default, on top of the page cache of redb. Settings bound it (N6).
- Disk: the log takes up to the backlog plus three segments for each store, about 450 MiB at most with the defaults. A quiet store takes less, because a segment grows only as entries fill it.
- An open after a crash replays up to the bound of the backlog, about 0.4 s for 256 MiB. After a clean close it reads the headers (N7).
- A close takes the time of the fold of its backlog: at most a few seconds with the defaults (the spike folded 256 MB in 2.9 s), which the stop of `zoe serve` allows.
- Each writable store has one more thread.
- Tests and fixtures that copy or damage the files of a store must include `wal/`, or fold first with `Store::fold`. The hooks of `zoe_core::testing` fold first.
- The specification changes in the data model (`wal/`, the format name), durability, group commit, the database file, memory and startup, the storage trait, the read-only opens and the metrics. The invariants of `crates/zoe-core/CLAUDE.md` change for the finish, the open, the close, the reads, pack, the count, the threads and the format. ADR 0020 (the log carries the frames), ADR 0028 (the repair folds the log) and ADR 0033 (the count in memory) get a line each.

## Known limits

- A damaged byte in the last entry of the log, or in the header of the segment of the last use, cuts that entry as a torn tail: there a crash during a write and a later damage give the same bytes, so the open cannot tell them apart. The commits of that entry are then lost with no error. A clean close folds the log, so a store at rest has no such entry. The pages of redb outside the frames have a limit of the same kind.
- A pack waits for the fold of the backlog before its mark: at most the fold of `backlog_bytes`, a few seconds.
- A batch of large records that finds a backlog goes to the log, so a mixed load still writes some large records twice.
- The syncs of the fold and of the finishes share the device. The fold avoids only the overlap with a sync that runs.
- Old bytes of folded entries stay in the spares until a new use writes over them, as freed pages of redb keep old bytes. A pack does not erase them.
- The size of the backlog counts 64 bytes for each op for the indexes in memory. A backlog of many tiny records can use more.
