# Spike: a write-ahead log with a background checkpoint, and what it gains

Card ZOE-184, requirements N4, N2 and N7. The prototype and its driver are in the scratch crate `walproto` (see Method); no code of it merges.

## Verdict

TBD

## Question

Does a write path where a commit appends its records to a log and syncs only the log, with a checkpoint that folds many commits at once into the `(oid, tid)` tree, buy enough of the commit rows of the phase 3 gate to pay for its complexity, and what does it cost on the reads, at recovery and in the rest of the store?

## Method

**Machine.** The cloud virtual machine of `performance-levers.md`: 4 vCPUs (Intel Xeon at 2.10 GHz), 15.7 GB of memory, Linux 6.18, the store on the root disk, a virtual block device with ext4. An `fdatasync` after an append costs 0.17 ms for 4 KB, 0.30 ms for 40 KB, 0.50 ms for 120 KB and 1.15 ms for 500 KB on it (median of 40), so the cost of a sync grows with the bytes it makes durable, unlike on the NVMe of the reference machine, where a small sync costs 0.3 ms and the size matters less. The numbers give the shape of the gain, not the gate.

**The layouts.**

- `main`: `zoe_core::Store` of the branch `claude/performance-optimization-igkin7` (b7a38ee plus the three levers of `performance-levers.md`: one zstd context per thread, a vote that reads serials, a next serial without decode), opened through a `Vfs` whose database file counts its writes and syncs, as `finish_time.rs` does. A commit is `tpc_begin`, 100 `store`, `vote`, `finish`; the time is that of `finish`.
- `log`: the prototype. A commit joins a group: the leader takes every waiting member, gives each a tid, encodes the frames of its records exactly as the core does (the same 32-byte header, the same xxh3-64 checksum over the key space tag, the key, the header and the payload, zstd at level 3 when smaller), appends one batch record per member to the log segment (a magic number, the tid, the transaction row, each record with its oid and frame, then an xxh3-64 checksum of the batch), calls `fdatasync` once, then puts the frames in the in-memory table (a `BTreeMap` keyed by `(oid, tid)` like the tree) and the batches in the queue of the checkpoint, and wakes the members. The checkpoint thread wakes when the queue holds 64 MB (`threshold`), takes every queued batch, writes them in one redb write transaction into the tables of the core (`records`, `txn_oids`, `txns`, `meta` with the last tid and the next oid), commits with a sync, then removes the folded keys from the table and deletes the log segments that the fold covers (a segment rotates at 256 MB). A leader whose queue holds more than 1 GB waits for the checkpoint (back-pressure). A load reads the table first, then the tree, so a record that the checkpoint moves is seen in one of the two. The open replays the segments after the last tid of `meta`, stops at the first torn or damaged batch, and rebuilds the table and the queue.
- `log-nofold`: the prototype with the checkpoint held until the end of the run, so the finish numbers hold no fold and the final fold folds every commit of the run at once: the best case of the amortisation, and the commit cost without interference.
- `index`: the layout of ZOE-174 (`card/ZOE-174`, rejected): the records keyed by `(tid, oid)` with a second index. Its bytes per commit come from `commit_bytes.rs` on that branch and the ADR draft there; its finish was not measured in this spike.

The prototype does not maintain the object count (the core looks up each written oid that is not fresh, at the finish); with a log, that lookup moves to the fold. It does not take the per-OID locks, resolve conflicts or write blobs, and its `update` threads write disjoint objects, so no vote waits: the numbers are the write path alone, the same work for both layouts but the lookups of the count.

**The runs.** `add` writes 100 new objects in each transaction, `update` writes again the 100 objects that each thread owns. Records of 1 KB and 100 KB, half text that zstd shrinks, half pseudo-random bytes, as a zodbshootout record. Concurrency 1, 16 and 64 threads, each with its own transactions in a loop, so the group commit forms its batches as the core does. 200 transactions per thread at 1 KB and concurrency 1, 20 at 16, 6 at 64; 30, 4 and 2 at 100 KB. Two rounds of each cell, the layouts in turn; the table gives the better round of each (the rounds differ by up to 15 % on this machine). The finish is measured in the caller: at concurrency above 1 it holds the wait for the leader of the batch.

**The checkpoint, the reads and recovery.** The checkpoint numbers come from the runs above and from `log-nofold`. The reads: 10,000 objects folded into the tree, then a table of 0, 10,000 or 1,000,000 records that no checkpoint folded (100-byte records for the largest, so the table fits in memory), and 20,000 loads of tree records (a miss in the table, then the tree) and of table records (a hit); and the same loads of tree records while a writer thread commits at full speed with a 4 MB threshold, so the checkpoint folds throughout. Recovery: 64 MB and 256 MB of log that no checkpoint folded, the open that replays it, and the fold after it.

**What did not run.** The gate set of the harness (`just bench run --gate`) did not run: the prototype is a scratch crate, not `zoe serve`. The spans of N17 did not run.

## The commit

TBD

## The checkpoint

TBD

## The reads

TBD

## Recovery

TBD

## The rest of the store

What a log changes outside the commit, from the code of the core as it is. None of it is in the prototype.

- **The format.** The log segments are on-disk format: a new file kind in the store directory, with its own framing (the batch record, its checksum), and `meta` gets the folded tid. A store whose log holds unfolded commits cannot be opened by a build without the log, so the open must refuse it: a format version. The frames inside the log are the frames of the records, unchanged, and the tree is unchanged, so a store whose log is empty after a clean close stays a store of the format of today. ADR 0020 and the format section of the specification change.
- **The reads.** Every read that reaches the tree gets a second source: `load_before`, `load_serial`, `load_current` and `current_serials` of the vote, `history`, `record_iternext` and `current_from`, the iterator (`txn_oids`, `indexed_tids`, `iter_txns`, `txn_records`), `has_blobs`, `load_blob`. Each must read the table first and the tree second, and merge in key order where it scans. The vote reads serials from the keys of the table, which is cheap. The iterator over a tid range that straddles the folded tid reads the tree then the queue of batches, which are in tid order.
- **Pack.** A pack at tid T removes records at or before T that no reader at T can reach. It reads the references of the current records, some of which are in the table. The simple rule: a pack first waits for a fold up to its tid, then packs the tree alone; the records after T are never removed, so the table after the fold never holds what a pack removes. The watermark stays in `meta`.
- **Blobs.** Blob records travel with their records in the log; the blob files are placed and synced before the log append, as before the tree commit today, so a blob record never lacks its file. The intent files of a deferred restore stay as they are.
- **The object count and the next oid.** The count needs a lookup per touched oid that is not fresh; the fold does them, so the count in `meta` is the count at the folded tid, and `object_count()` adds the delta of the queue (each batch knows its fresh oids). The next oid goes to `meta` at the fold and to memory at the commit; the open takes the larger of `meta` and the log.
- **Sequences and `sync`.** They write the tree in a write transaction of their own, with a sync. They keep doing so: the tree and the log are independent, and a sequence reservation is durable on its own.
- **Deferred restores.** `commit_deferred` writes the tree without a sync, and the next durable commit covers it. With a log, a deferred batch appends without `fdatasync`; the next synced batch covers it, as today.
- **Crash points.** A torn tail of a segment: the open cuts the log at the first batch whose checksum fails or whose length is short, and the commits after it were never acknowledged. A crash during a fold: the redb commit is atomic, and the segments are deleted only after the commit is durable, so the open replays batches that the fold already wrote; the replay must skip a batch whose tid is at or below the folded tid of `meta`, which it reads first. A crash between the commit of the fold and the deletion of a segment: the same. The crash simulation of the store (`every_crash_image_...` in the core) is the test: each prefix and reorder of the writes of the log and of the tree must open to a store with every acknowledged tid.
- **Memory (N6).** The table holds the unfolded records, bounded by the back-pressure limit, and the queue holds the same frames once (the table and the queue share them). The limit is a setting, and a commit that meets it waits for the fold: a stall, which the metrics count.
- **The read-only open and `zoe verify`.** A read-only open replays the log into memory and folds nothing; `zoe verify` walks the tree and the log. The repair after a killed writer (ADR 0033) folds first.
- **The server.** `tpc_finish` returns when the batch is durable in the log, as today when the tree is durable; the invalidations and the publication order do not change. `zoe serve` gets one more thread, the checkpoint, and a metric for the queue and the stalls.
- **The simulator (N18).** The checkpoint thread is a blocking thread of the core, like a sync; the simulator schedules it, and the fold's writes go through the `Vfs`.

## Cards

TBD
