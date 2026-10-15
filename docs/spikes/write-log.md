# Spike: a write-ahead log with a background checkpoint, and what it gains

Card ZOE-184, requirements N4, N2 and N7. The prototype and its driver are in the scratch crate `walproto` (see Method); no code of it merges.

## Verdict

**Go with conditions.** The log removes one term of the commit, and it is the term of `update`: the copies of the leaf pages of the `(oid, tid)` tree, 696 KB written and synced for 100 records of 1 KB today, against 63 KB synced and 168 KB written in all with the log. On the machine of this spike that turns the finish of an `update` at concurrency 1 from 4.5 ms into 1.5 ms, and the commits per second at concurrency 16 from 274 into 695. `add` gains less, because new records already append in the tree: 2.2 ms into 1.5 ms at concurrency 1, a quarter more commits per second at 16, nothing at 64. At 100 KB the log does not pay on this disk: it writes every record twice, and its checkpoint competes with the commits for the one device, so the finish gets worse unless the fold is held back.

The conditions:

1. **ZOE-172 first.** At concurrency 16 and 64 the leader of a batch compresses the records of every member in series, and that caps both layouts at about 650 commits per second here. With the compression on the member's thread, the prototype reaches 1,940 to 2,150 commits per second at concurrency 16, three times the core today, and the log's own gain shows on top of it. The compression is the larger lever, and it is a card already.
2. **A fold that does not compete with the commits.** The checkpoint must write in slices and sync them apart from the commit's sync (or wait while a commit syncs), and stay under the page cache of redb; the 100 KB rows must not get worse than today, and that is part of the acceptance of the card.
3. **A bounded log.** The queue of unfolded commits is bounded (256 MB by default), so the memory of the table (N6) and the replay at the next open (N7: 1.3 to 1.6 ms per MB of log, then the fold) stay bounded, and a commit that meets the bound waits for the fold.
4. **Measured on the reference machine before the card is Ready.** On this virtual disk a sync costs by the byte, which flatters the log; on the NVMe of the reference machine a sync costs 0.3 ms whatever its size, and the gain is the inserts and the page copies that ZOE-173 measured at 0.3 to 0.7 ms of a 1.3 ms finish. The gain is real there too, and smaller.
5. **A format change.** The log segments and the folded tid in `meta` are on-disk format: a red zone, for the human, with a format version that an older build refuses while the log is not empty.

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

Each row is the better of its rounds (two at 1 KB, one at 100 KB). `bytes per commit` is everything the layout writes for one commit: the database file for `main` and `index`; the log plus the share of the tree writes of the folds for `log`, so the double write shows. `synced per commit` is what the commit's own sync makes durable: the whole of it for `main`, the log batch for `log`. The `index` rows are the numbers of ZOE-174 (`commit_bytes.rs` on `card/ZOE-174`, in the simulated filesystem), not a measurement of this spike; that layout's finish was not measured. `log-pre` is the prototype with the zstd compression of each record done on the thread of the member before it joins the group, the move that ZOE-172 makes for `main`; its finish holds no compression, so its p50 at concurrency 1 understates the time of the transaction by the 0.9 ms of the 100 compressions.

| scenario | object size | concurrency | layout | bytes per commit | synced per commit | finish p50 | finish p99 | commits/s |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| add | 1 KB | 1 | main | 117,936 | 117,936 | 2,166 µs | 3,566 µs | 406 |
| add | 1 KB | 1 | log | 134,327 | 62,892 | 1,557 µs | 2,068 µs | 631 |
| add | 1 KB | 1 | log-nofold | 134,327 | 62,892 | 1,415 µs | 2,857 µs | 678 |
| add | 1 KB | 1 | log-pre | 134,327 | 62,892 | 511 µs | 1,618 µs | 700 |
| add | 1 KB | 1 | index | 191,316 | 191,316 | — | — | — |
| add | 1 KB | 16 | main | 84,572 | 84,572 | 23,982 µs | 28,600 µs | 649 |
| add | 1 KB | 16 | log | 143,046 | 62,892 | 19,060 µs | 27,866 µs | 801 |
| add | 1 KB | 16 | log-nofold | 143,763 | 62,892 | 20,061 µs | 30,245 µs | 758 |
| add | 1 KB | 16 | log-pre | 149,843 | 62,892 | 5,798 µs | 10,463 µs | 1,940 |
| add | 1 KB | 64 | main | 85,706 | 85,706 | 101,685 µs | 135,885 µs | 597 |
| add | 1 KB | 64 | log | 147,383 | 62,892 | 95,660 µs | 165,357 µs | 592 |
| add | 1 KB | 64 | log-nofold | 146,711 | 62,892 | 101,303 µs | 144,610 µs | 585 |
| add | 1 KB | 64 | log-pre | 153,516 | 62,892 | 33,650 µs | 61,634 µs | 1,389 |
| update | 1 KB | 1 | main | 695,964 | 695,964 | 4,509 µs | 7,327 µs | 210 |
| update | 1 KB | 1 | log | 168,161 | 62,893 | 1,681 µs | 3,656 µs | 560 |
| update | 1 KB | 1 | log-nofold | 168,161 | 62,893 | 1,493 µs | 2,333 µs | 633 |
| update | 1 KB | 1 | log-pre | 168,161 | 62,893 | 634 µs | 2,531 µs | 611 |
| update | 1 KB | 1 | index | 195,289 | 195,289 | — | — | — |
| update | 1 KB | 16 | main | 495,796 | 495,796 | 59,793 µs | 75,839 µs | 274 |
| update | 1 KB | 16 | log | 168,288 | 62,892 | 21,838 µs | 36,179 µs | 695 |
| update | 1 KB | 16 | log-nofold | 168,416 | 62,892 | 20,264 µs | 58,292 µs | 693 |
| update | 1 KB | 16 | log-pre | 168,505 | 62,892 | 5,237 µs | 10,030 µs | 2,152 |
| update | 1 KB | 64 | main | 397,418 | 397,418 | 192,087 µs | 226,285 µs | 338 |
| update | 1 KB | 64 | log | 180,386 | 62,892 | 92,353 µs | 137,812 µs | 630 |
| update | 1 KB | 64 | log-nofold | 180,450 | 62,892 | 94,211 µs | 129,703 µs | 657 |
| update | 1 KB | 64 | log-pre | 180,236 | 62,892 | 23,879 µs | 48,602 µs | 1,608 |
| add | 100 KB | 1 | main | 6,603,481 | 6,603,481 | 21,670 µs | 26,385 µs | 40 |
| add | 100 KB | 1 | log | 11,698,831 | 5,132,092 | 27,419 µs | 120,011 µs | 29 |
| add | 100 KB | 1 | log-nofold | 11,696,079 | 5,132,092 | 16,338 µs | 31,263 µs | 48 |
| add | 100 KB | 1 | log-pre | 11,698,831 | 5,132,092 | 14,208 µs | 117,560 µs | 33 |
| add | 100 KB | 16 | main | 6,567,208 | 6,567,208 | 369,731 µs | 435,146 µs | 42 |
| add | 100 KB | 16 | log | 11,696,981 | 5,132,092 | 693,915 µs | 891,645 µs | 20 |
| add | 100 KB | 16 | log-nofold | 11,694,273 | 5,132,092 | 287,822 µs | 346,568 µs | 47 |
| add | 100 KB | 16 | log-pre | 11,697,109 | 5,132,092 | 315,846 µs | 399,303 µs | 43 |
| add | 100 KB | 64 | main | 6,563,404 | 6,563,404 | 1,400,417 µs | 4,576,300 µs | 21 |
| add | 100 KB | 64 | log | 11,694,659 | 5,132,092 | 2,280,943 µs | 8,403,915 µs | 12 |
| add | 100 KB | 64 | log-nofold | 17,966,078 | 5,132,092 | 2,554,320 µs | 6,675,620 µs | 13 |
| add | 100 KB | 64 | log-pre | 11,694,177 | 5,132,092 | 1,476,177 µs | 4,556,704 µs | 20 |
| update | 100 KB | 1 | main | 6,685,265 | 6,685,265 | 27,355 µs | 34,821 µs | 32 |
| update | 100 KB | 1 | log | 11,702,928 | 5,132,093 | 16,450 µs | 273,872 µs | 33 |
| update | 100 KB | 1 | log-nofold | 11,693,895 | 5,132,093 | 15,904 µs | 26,275 µs | 52 |
| update | 100 KB | 1 | log-pre | 11,709,492 | 5,132,093 | 12,260 µs | 103,231 µs | 42 |
| update | 100 KB | 16 | main | 6,584,168 | 6,584,168 | 349,163 µs | 489,784 µs | 42 |
| update | 100 KB | 16 | log | 11,714,517 | 5,132,092 | 651,511 µs | 883,463 µs | 25 |
| update | 100 KB | 16 | log-nofold | 11,696,001 | 5,132,092 | 412,560 µs | 826,112 µs | 32 |
| update | 100 KB | 16 | log-pre | 11,714,005 | 5,132,092 | 315,636 µs | 466,732 µs | 43 |
| update | 100 KB | 64 | main | 6,686,828 | 6,686,828 | 3,560,798 µs | 5,608,391 µs | 14 |
| update | 100 KB | 64 | log | 11,703,905 | 5,132,092 | 4,287,648 µs | 8,965,572 µs | 10 |
| update | 100 KB | 64 | log-nofold | 16,295,998 | 5,132,092 | 2,629,983 µs | 3,497,820 µs | 20 |
| update | 100 KB | 64 | log-pre | 11,704,385 | 5,132,092 | 4,825,000 µs | 12,934,096 µs | 7 |

**What the table says.**

- **At 1 KB, the log removes the term of `update`: the copies of the leaf pages.** An `update` commit of `main` writes 696 KB for 61 KB of records (each record lands in its own leaf of `records`, next to the older records of its oid, and redb copies each leaf it touches, plus the index and the meta pages); the log writes 63 KB to the log and, through the fold, 105 KB to the tree: 168 KB in all, 4 times less, and the commit syncs 11 times fewer bytes. On this disk, where a sync costs by the byte, the finish falls from 4.5 ms to 1.5 ms at concurrency 1, and the commits per second from 274 to 695 at concurrency 16 and from 338 to 630 at 64. On the NVMe of the reference machine a sync costs 0.3 ms almost whatever its size, so the gain there is the work of the inserts and the page copies, which ZOE-173 measured at 0.3 to 0.7 ms of a 1.3 ms finish: still the larger half of the finish without its sync, but a smaller ratio than here.
- **At 1 KB, `add` gains less: the tree already appends.** New oids land in dense leaves, so `main` writes 118 KB for 61 KB of records; the log writes 134 KB in all (63 KB to the log, 71 KB to the tree at the fold), more bytes than today, 63 KB of them synced by the commit. The finish at concurrency 1 falls from 2.2 ms to 1.5 ms on this disk; the commits per second at 16 rise by a quarter, and at 64 not at all.
- **At concurrency, the leader's compression caps both layouts.** The leader of a batch compresses the 100 records of each member in series: 0.9 ms per member with the kept zstd context. With 16 members a batch takes at least 14 ms, whatever the layout, and that is the p50 of `add` at 16 on both. `log-pre` moves the compression to the member's thread, and the same batch takes 5 to 6 ms: 1,940 to 2,150 commits per second at concurrency 16, three times `main`. That move is ZOE-172, which applies to `main` as well; the share of the log in it is the `log` row against the `main` row.
- **At 100 KB, the log loses on this disk, and the checkpoint is the reason.** The commit writes 5.1 MB to the log and the fold writes 6.6 MB to the tree: 11.7 MB per commit against 6.6 MB, on a disk that writes about 300 MB per second. With the checkpoint running (`log`), the finish at concurrency 1 is 27 ms against 22 ms for `main`, and at 16 it is 690 ms against 370: the fold's sync of tens of MB and the commit's `fdatasync` share one device, and the commit waits. Without the checkpoint during the run (`log-nofold`), the finish is 16 ms against 22 at concurrency 1 and 290 ms against 370 at 16, which is what the log alone buys when the fold runs elsewhere or at another time. So at 100 KB the log pays only with a fold that does not compete with the commits: a throttled fold, or a log on another device.

## The checkpoint

One fold writes every queued batch in one redb write transaction. `records per second` is the records folded over the time of the folds; `amplification` is the bytes the tree writes for one byte of record frames; `commits per fold` says how many commits share one tree transaction.

| scenario | object size | concurrency | layout | folds | commits per fold | records per second | amplification | fold ms (total / longest) | stalls |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| add | 1 KB | 1 | log | 1 | 200 | 174,145 | 1.16 | 115 / 115 | 0 |
| add | 1 KB | 1 | log-nofold | 1 | 200 | 190,330 | 1.16 | 105 / 105 | 0 |
| add | 1 KB | 1 | log-pre | 1 | 200 | 217,124 | 1.16 | 92 / 92 | 0 |
| add | 1 KB | 16 | log | 1 | 320 | 159,216 | 1.30 | 201 / 201 | 0 |
| add | 1 KB | 16 | log-nofold | 1 | 320 | 177,353 | 1.31 | 180 / 180 | 0 |
| add | 1 KB | 16 | log-pre | 1 | 320 | 139,925 | 1.41 | 229 / 229 | 0 |
| add | 1 KB | 64 | log | 1 | 384 | 186,228 | 1.37 | 206 / 206 | 0 |
| add | 1 KB | 64 | log-nofold | 1 | 384 | 159,337 | 1.36 | 241 / 241 | 0 |
| add | 1 KB | 64 | log-pre | 1 | 384 | 150,619 | 1.47 | 255 / 255 | 0 |
| update | 1 KB | 1 | log | 1 | 200 | 115,854 | 1.71 | 173 / 173 | 0 |
| update | 1 KB | 1 | log-nofold | 1 | 200 | 121,207 | 1.71 | 165 / 165 | 0 |
| update | 1 KB | 1 | log-pre | 1 | 200 | 104,160 | 1.71 | 192 / 192 | 0 |
| update | 1 KB | 16 | log | 1 | 320 | 111,818 | 1.71 | 286 / 286 | 0 |
| update | 1 KB | 16 | log-nofold | 1 | 320 | 125,670 | 1.71 | 255 / 255 | 0 |
| update | 1 KB | 16 | log-pre | 1 | 320 | 114,288 | 1.71 | 280 / 280 | 0 |
| update | 1 KB | 64 | log | 1 | 384 | 116,404 | 1.91 | 330 / 330 | 0 |
| update | 1 KB | 64 | log-nofold | 1 | 384 | 145,917 | 1.91 | 263 / 263 | 0 |
| update | 1 KB | 64 | log-pre | 1 | 384 | 122,588 | 1.90 | 313 / 313 | 0 |
| add | 100 KB | 1 | log | 3 | 10 | 5,806 | 1.28 | 517 / 238 | 0 |
| add | 100 KB | 1 | log-nofold | 1 | 30 | 6,256 | 1.28 | 480 / 480 | 0 |
| add | 100 KB | 1 | log-pre | 3 | 10 | 5,544 | 1.28 | 541 / 256 | 0 |
| add | 100 KB | 16 | log | 5 | 13 | 2,469 | 1.28 | 2592 / 934 | 0 |
| add | 100 KB | 16 | log-nofold | 1 | 64 | 3,113 | 1.28 | 2056 / 2056 | 0 |
| add | 100 KB | 16 | log-pre | 5 | 13 | 2,060 | 1.28 | 3107 / 1265 | 0 |
| add | 100 KB | 64 | log | 3 | 43 | 1,235 | 1.28 | 10362 / 8566 | 0 |
| add | 100 KB | 64 | log-nofold | 1 | 128 | 2,213 | 2.50 | 5784 / 5784 | 0 |
| add | 100 KB | 64 | log-pre | 2 | 64 | 1,957 | 1.28 | 6539 / 5374 | 0 |
| update | 100 KB | 1 | log | 3 | 10 | 4,290 | 1.28 | 699 / 410 | 0 |
| update | 100 KB | 1 | log-nofold | 1 | 30 | 6,233 | 1.28 | 481 / 481 | 0 |
| update | 100 KB | 1 | log-pre | 4 | 8 | 5,849 | 1.28 | 513 / 247 | 0 |
| update | 100 KB | 16 | log | 5 | 13 | 3,821 | 1.28 | 1675 / 545 | 0 |
| update | 100 KB | 16 | log-nofold | 1 | 64 | 3,298 | 1.28 | 1941 / 1941 | 0 |
| update | 100 KB | 16 | log-pre | 5 | 13 | 3,682 | 1.28 | 1738 / 485 | 0 |
| update | 100 KB | 64 | log | 2 | 64 | 1,252 | 1.28 | 10222 / 9130 | 0 |
| update | 100 KB | 64 | log-nofold | 1 | 128 | 1,452 | 2.18 | 8818 / 8818 | 0 |
| update | 100 KB | 64 | log-pre | 2 | 64 | 848 | 1.28 | 15099 / 13726 | 0 |

- **Throughput.** At 1 KB the fold writes 110,000 to 220,000 records per second (65 to 130 MB of records per second): 100 commits of `add` fold in 0.1 s. At 100 KB it writes 1,200 to 6,000 records per second, 120 to 600 MB per second: the disk.
- **Amplification.** 1.16 for `add` at 1 KB: the records of 200 commits go into dense leaves, with the index and the transaction rows on top, against 1.9 (118 KB for 61 KB) when each commit is its own transaction. 1.7 to 1.9 for `update`: each record still goes into the leaf of its oid, but the copies of the inner pages and of the meta pages are shared by the commits of the fold, against 11 (696 KB for 61 KB) today. 1.28 at 100 KB, where the frames are the leaves. The largest folds at 100 KB (1.3 GB in one transaction) reach 2.2 to 2.5: the fold outgrows the page cache of redb (1 GiB) and writes pages twice. A fold should stay under the cache.
- **Commits folded per transaction.** With a threshold of 64 MB and 1 KB records, every commit of the run folds in one transaction (200 to 384 commits). At 100 KB, 8 to 64 commits.
- **The effect on the commits that run at the same time.** The commit table shows it: at 1 KB, none that the rounds can tell (`log` against `log-nofold`); at 100 KB, a fold doubles the finish at concurrency 16 and 64, because its sync and the commit's sync share the device. A fold that writes in slices of a few MB with `sync_file_range` between them, or that waits while a commit syncs, is the remedy to measure; the prototype has neither.
- **When it falls behind.** The leader waits when the queue holds more than the limit (1 GB in the runs). No run reached it: at 1 KB the fold is 10 times faster than the commits, and at 100 KB both are the disk. The limit bounds the memory of the table (N6) and the log that a crash leaves to replay (N7).

## The reads

The tree holds 10,000 objects; the table holds 0, 10,000 or 1,000,000 records that no checkpoint folded. A load of a tree record first misses in the table, then reads the tree; a load of a table record is a hit. `main` is the core as it is, on the same records. Median and p99 of 20,000 loads, in µs.

| table records | record | main load p50 / p99 | tree load p50 / p99 (a miss, then the tree) | table load p50 / p99 (a hit) | folds during | writer commits |
| --- | --- | --- | --- | --- | --- | --- |
| 0 | 1024 B | 2.9 / 10.4 µs | 2.6 / 5.3 µs | 0.0 / 0.0 µs | 1 | 0 |
| 10,000 | 1024 B | 4.0 / 11.8 µs | 3.4 / 8.9 µs | 1.3 / 2.1 µs | 1 | 0 |
| 1,000,000 | 100 B | 2.2 / 5.0 µs | 1.6 / 3.1 µs | 1.3 / 2.5 µs | 1 | 0 |
| 4,200 (under a running checkpoint) | 1024 B | 3.6 / 6.9 µs | 2.3 / 6.3 µs | 0.0 / 0.0 µs | 3 | 42 |

- A hit costs 1.3 µs: a range lookup in a `BTreeMap` under a read lock, the checksum and the decompression of the frame. A miss adds about 0.5 µs to a tree load (2.6 µs with an empty table, 3.4 with 10,000 records) and does not grow with the table: 1,000,000 records of 100 bytes give the same miss. The in-process storage would see it; a load through the server, at 70 µs a round trip, would not.
- A running checkpoint changes nothing that the rounds can tell: the fold holds the write lock of the table only to remove the folded keys, after its tree commit.
- Snapshot consistency held in every run: a load reads the table first and the tree second, so a record that the fold moves between the two reads is seen in one of them, and a record is never seen twice with two tids. A reader at a tid T reads the table below T, as the tree. The prototype does not test the race of a fold against a reader on purpose; the core's tests would.
- The memory of the table is the frames of the unfolded records plus the keys: 1,000,000 records of 100 bytes take 170 MB. The limit of the queue bounds it.

## Recovery

A log that no checkpoint folded, then an open that replays it: the open reads each segment, checks the checksum of each batch, and rebuilds the table and the queue; the fold after it writes the tree.

| log | commits | records | replay | replay per MB | open | fold after the open |
| --- | --- | --- | --- | --- | --- | --- |
| 64 MB | 1,068 | 106,800 | 82 ms | 1.28 ms | 83 ms | 564 ms |
| 256 MB | 4,269 | 426,900 | 414 ms | 1.62 ms | 414 ms | 2852 ms |

- The replay costs 1.3 to 1.6 ms per MB of log, reading the file and checking the checksums; the open of a store with 256 MB of unfolded log takes 0.4 s. The fold after it writes 150,000 records per second: 2.9 s for 256 MB. An open that serves reads from the table while the fold runs meets N7 (ready within 2 s) with a log of up to about 1 GB; an open that folds first meets it with a log of up to about 128 MB. So the limit of the queue sets the time of a recovery, and 256 MB is a reasonable default.
- The crash points: a torn last batch is cut at its checksum; a crash during a fold leaves the log, which the next open replays after the folded tid of `meta`; a crash between the commit of a fold and the deletion of a segment replays batches that the tree holds, which the replay skips by tid. The prototype's replay reads the folded tid of `meta` first and skips the batches at or below it, and every open of the runs found the records of every acknowledged commit, by count. The crash simulation of the core (every prefix and reorder of the writes) is what a card must run.

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

1. **ZOE-172 (#361), as it is: compression off the path of the transaction.** Moves: the `add`, `update` and `read_warm` rows at concurrency 16 and 64 of both sizes, where the leader's compression caps the commits at about 650 per second here and `log-pre` shows 1,940 to 2,150. It is a condition of the next card, not part of it.
2. **New card: a write-ahead log with a background checkpoint in `zoe-core`.** Behind an option of the store at first, then the default. The commit appends its batch to a log segment and syncs the log; the table of unfolded records serves the reads before the tree; a checkpoint thread of the core folds the queue into the tables in slices, throttled against the commits, under the page cache, and deletes the segments it covers; the queue is bounded; the open replays the log after the folded tid of `meta` and serves reads while it folds. The format version changes (red zone). Every read path of the backend reads the table first (`load_before`, `load_serial`, `current_serials`, `history`, `record_iternext`, `current_from`, the iterator, the blob records); pack waits for a fold up to its tid; `zoe verify` and the read-only open replay the log; the crash simulation of the core covers the log, the fold and the deletion of a segment. Moves: the `update` and `read_warm` rows at 1 KB at concurrency 16 and 64 (threads and processes), by the leaf copies of the tree; part of the `add` rows at 1 KB at concurrency 1 and 16; none of the 100 KB rows, which it must not make worse. Acceptance: `finish_time.rs` with the log, `commit_bytes.rs` of ZOE-174 for the bytes, the crash simulation green, and the gate set on the reference machine before and after.
3. **Not a card: the `index` layout of ZOE-174.** The log writes fewer bytes for an `update` commit than the second index did (168 KB in all against 195 KB), with no second lookup on the read path, which is why the human rejected ZOE-174. Its numbers stay in the table for the comparison.
