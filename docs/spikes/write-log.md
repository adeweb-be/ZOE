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

TBD

## Cards

TBD
