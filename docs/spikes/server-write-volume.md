# Spike: the bytes that `zoe serve` writes for each B5 operation

Card ZOE-223; requirements N1 to N5 and the column "server KB written/op" of B5.

**Verdict.** Most of the gap is in the measure, not in the store. B5 takes `write_bytes` of `/proc/<pid>/io`, and on this kernel (Linux 7.0, ext4) that counter charges the whole page-cache folio when a write dirties a clean folio: a 4 KB `pwrite` into a file that a large copy put in the page cache counts as 2,048 KB, while the device writes 4 KB. The store of a run is such a copy, so B5 shows 8,300 to 16,700 KB per operation for `zoe serve`, about 8 to 10 times the bytes that it really passes to its store file. ZEO is inflated too, about 3 times, for another reason: FileStorage writes each transaction to `Data.fs.tmp` first, and the kernel cancels most of those bytes when the file is truncated. Counted at the system call, `zoe serve` writes 1,000 to 1,830 KB per operation to `zoe.redb`, against 156 to 292 KB that FileStorage appends to `Data.fs`: 5.7 to 6.6 times more, not 20. That remaining factor is redb: each record that a commit changes rewrites about 2 pages of 4 KB (1.7 to 2.1 in B5, 1.9 in the micro-benchmark), plus about 12 pages per commit, for records of about 1.1 KB. Queued indexing adds no bytes: its indexer writes the same catalog records that the save wrote before, and its keys cost about one page per commit. The resolution log writes 0.7 KB per operation, and the lease writes no commit of its own in these runs. The levers: count the bytes truthfully in B5 (a card), let the indexer batch more saves into one commit (a card, −27 % to −59 % of the indexer bytes), and, only if the bytes matter more than their price, a record layout or the append-only backend of the specification, both format changes.

## Question

Where do the bytes that `zoe serve` writes for one B5 operation go: the store file, the resolution log, other files; for the store, which kind of commit (a save, a batch of the indexer, the lease); how many commits, syncs and pages per commit? Is the comparison with ZEO apples to apples? Which levers would reduce the bytes, at which cost? The runs of ZOE-219 showed 9,300 to 21,200 KB per operation on `zoe-native-qi` with `--split-merges on`, against 440 to 1,020 KB on `zeo`.

## Method

- **Code.** `main` at `582fb0a`, release build of `zoe`, the Plone instances of this worktree (`just plone zeo build`, `just plone zoeclient build`) and the small fixture `small-ba01ff255313` (`just bench plone prepare`).
- **How B5 counts.** The runner samples `/proc/<pid>/io` of each process once a second (`plonebench/sampler.py`); the report divides the `write_bytes` delta of the process `server` in the step window by the operations of the step. The process `server` is the ZEO server (one Python process, `probe/zeoserver/launch.py`) or `zoe serve` (one process). So B5 counts per process, every file that the process writes, not a disk. Each run also records the counters of the block device under the store (`resources.disk`), but that device holds the home directory of the machine, and other jobs wrote to it during every run (see the numbers).
- **The B5 runs.** `just bench plone run --stack S --scenario C --steps 4:60 --port-offset 300 --keep-store` under `systemd-inhibit --what=sleep:idle`, one run at a time, with the default warmup (60 s, 8 users) and trim (5 s): `zeo`, `zoe-native`, `zoe-native-qi --split-merges off` and `zoe-native-qi --split-merges on`, on `edit` and `create-spread`. Every run passed its verification.
- **A side sampler.** Beside each run, a script polled the server process once a second: every field of `/proc/<pid>/io` (`wchar`, `syscw`, `write_bytes`, `cancelled_write_bytes`) and the size of each regular file that the process held open.
- **strace.** Four more runs (`zoe-native` and `zoe-native-qi --split-merges on`, on both scenarios) with `ZOE_BINARY` set to a script outside the repository that runs `zoe serve` under `strace --seccomp-bpf -f -y -ttt -e trace=pwrite64,write,writev,fdatasync,fsync,ftruncate,fallocate,rename,unlink,...`. A parser took the step window, summed the bytes of the write calls per file, and cut the writes to `zoe.redb` at each `fdatasync`: the bytes, the 4 KB pages and the calls of each sync. strace slows the server, and the edit run of `zoe-native` made 2.31 operations per second against 3.55, but the records per operation stay the same, so the bytes per operation compare.
- **The kind of each commit.** A script read the transactions of each kept store in the step window (`zoe.Storage(path, read_only=True).iterator()`, and FileStorage for `zeo`): the note `zoe.plone: indexing queue` marks a batch of the indexer, a transaction without records a KV-only write (the lease), the rest a save. Each sync of the strace log took the transactions whose tid time lies before it and after the sync before it. The server metrics (`zoe_commits_total`, `zoe_fsync_batch_size`) of `result.json` gave the commits and the syncs.
- **A resolution log.** One more run of `zoe-native-qi --split-merges on`, `create-spread`, with `ZOE_BINARY` set to a script that adds `--resolution-log` to `zoe serve`, as the runs of ZOE-219 did.
- **The page cache.** A Python test (below) wrote a 64 MB file in several ways, synced it, then made 200 `pwrite`s of 4 KB at random page offsets, each followed by `fdatasync`, and read `write_bytes`, `wchar` and the sectors that the device wrote.
- **A micro-benchmark** (outside B5, in-process, in the interpreter of the zoeclient instance): on a copy of the fixture store, 200 commits that each store a new revision of 1, 10 or 70 existing objects picked at random, with their current pickle, on `zoe.Storage` and on a copy of the `Data.fs` of the fixture with `FileStorage`. It reads `/proc/self/io` before and after: in one process with no socket, `wchar` is the bytes passed to the store files. The same with 20 and 100 durable KV keys set and the keys of the commit before deleted, as the indexing queue does.
- **The machine.** Intel Core Ultra 7 155H, 22 logical CPUs, 30 GiB, Linux 7.0, ext4 on LVM on dm-crypt, NVMe SSD, `taskset`. CPython 3.13.15 with the GIL in the instances. Other agents built and tested on the machine: the 1-minute load was 2.9 to 7.8. The bytes do not depend on the load; the rates do.
- The kept stores were deleted after the reads. The results and the scripts stay outside the repository.

## Numbers

### B5, one step of 4 users, the counters as B5 reads them

Per operation of the step window (55 s). "B5 KB/op" is the `write_bytes` of the server process. "File growth" is the growth of the files that the server held open. "Records" is the bytes of the records that the transactions of the window wrote. "Txns" is the transactions per operation, by kind. The batch size is `zoe_fsync_batch_size` (sum ÷ count).

| Scenario | Stack | Ops | ok ops/s | B5 KB/op | File growth KB/op | Records KB/op | Txns/op | Commits per sync | Server CPU ms/op |
| --- | --- | ---: | ---: | ---: | --- | ---: | --- | ---: | ---: |
| `edit` | `zeo` | 182 | 3.22 | 496 | `Data.fs` 156 | 153 | 1.93 save | | 14.5 |
| `edit` | `zoe-native` | 200 | 3.55 | 10,791 | `zoe.redb` 329 | 155 | 1.97 save | 1.00 | 13.4 |
| `edit` | `zoe-native-qi`, split off | 191 | 3.47 | 10,039 | `zoe.redb` 344 | 150 | 1.97 save, 0.75 indexer | 1.06 | 10.2 |
| `edit` | `zoe-native-qi`, split on | 203 | 3.69 | 8,477 | `zoe.redb` 324 | 151 | 1.98 save, 0.76 indexer | 1.06 | 10.4 |
| `create-spread` | `zeo` | 152 | 2.56 | 1,006 | `Data.fs` 292 | 285 | 0.91 save | | 24.0 |
| `create-spread` | `zoe-native` | 175 | 3.02 | 16,696 | `zoe.redb` 376 | 282 | 0.95 save | 1.00 | 21.4 |
| `create-spread` | `zoe-native-qi`, split off | 174 | 3.16 | 14,885 | `zoe.redb` 378 | 248 | 1.00 save, 0.80 indexer | 1.00 | 15.5 |
| `create-spread` | `zoe-native-qi`, split on | 183 | 3.33 | 13,684 | `zoe.redb` 360 | 230 | 1.02 save, 0.70 indexer | 1.00 | 14.4 |
| `create-spread` | `zoe-native-qi`, split on, resolution log | 208 | 3.78 | 12,764 | `zoe.redb` 316, log 0.7 | 210 | 1.00 save, 0.62 indexer | 1.00 | 11.4 |

- The B5 numbers of `zoe-native-qi` are of the order of those of ZOE-219 (9,300 to 21,200 KB, over steps of 4 and 16 users).
- Both stacks write the same records: 150 to 155 KB per operation in `edit`, 210 to 285 in `create-spread`. The records of `zoe serve` are raw (ADR 0041), as FileStorage's are.
- `zoe-native` (no queued indexing, the store as before ADR 0047) writes as many bytes as `zoe-native-qi`. Split merges change no byte count.
- `zoe_commits_total` agrees with the transactions of the window within 4 %. A sync held 1.00 to 1.06 commits on average: at 4 users, finishes seldom overlap, and group commit has little to join.
- The block device under the store wrote 2,200 to 7,200 KB per operation in every run, `zeo` included, with other jobs writing to it. It does not separate the server from the rest of the machine.

### The page cache charges whole folios

`write_bytes` per `pwrite` of 4 KB followed by `fdatasync`, 200 times at random page offsets of a 64 MB file:

| How the file was written before | `write_bytes` per pwrite | `wchar` per pwrite |
| --- | ---: | ---: |
| one `write` of 64 MB | 2,048 KB | 4 KB |
| 64 writes of 1 MB | 1,024 KB | 4 KB |
| 16,384 writes of 4 KB | 4 KB | 4 KB |
| one `write` of 64 MB, then dropped from the page cache | 4 KB | 4 KB |

On the file written in one `write`, the device wrote 4.1 KB per `pwrite` and `fdatasync` (sectors of `dm-1`), while `write_bytes` counted 2,048 KB. The kernel accounts a write when a page-cache folio turns dirty, by the size of the folio, and ext4 on this kernel caches large files in folios of up to 2 MB. The writeback writes only the dirty blocks.

A B5 run copies the store of the fixture (67 MB of `zoe.redb`) and the server opens it at once, so most pages of `zoe.redb` sit in large folios. `zoe-core` also zero-fills the growth of the database file in chunks of 1 MB (`OsFile::set_len`), which makes 1 MB folios. Each commit dirties pages spread over the file, and each one costs B5 up to 2 MB. In the micro-benchmark, on a warm copy, `write_bytes` was 12.6 to 13.4 times `wchar`; after the copy was dropped from the page cache, 1.05 to 1.21 times.

FileStorage appends to the end of `Data.fs`, where new pages make small folios, so the folios do not inflate it. Its count has another surplus: FileStorage writes the records of each transaction to `Data.fs.tmp` first and copies them to `Data.fs` at the vote. In `edit`, `cancelled_write_bytes` of the ZEO server was 330 KB per operation of its 496: dirty pages that the truncation of the temporary file dropped before any writeback. What ZEO writes for good is the growth of `Data.fs`, 156 KB.

### What `zoe serve` really writes

From the strace runs, bytes per operation passed to the write calls of each file, in the step window. "Store" is `zoe.redb`, and it is the only file that the server writes, apart from the resolution log when it has one. The socket bytes are the answers to the clients, which `wchar` counts too.

| Scenario | Stack | Store KB/op | Syncs/op | KB per sync, mean (median) | Pages per sync, mean | Socket KB/op |
| --- | --- | ---: | ---: | --- | ---: | ---: |
| `edit` | `zoe-native` | 1,035 | 1.89 | 546 (826) | 137 | 95 |
| `edit` | `zoe-native-qi`, split on | 1,004 | 2.39 | 420 (112) | 106 | 44 |
| `create-spread` | `zoe-native` | 1,832 | 0.98 | 1,872 (1,940) | 469 | 175 |
| `create-spread` | `zoe-native-qi`, split on | 1,677 | 1.80 | 934 (176) | 234 | 80 |

Against the growth of `Data.fs` on `zeo` (156 and 292 KB), `zoe serve` writes 6.4 to 6.6 times more in `edit` and 5.7 to 6.3 times more in `create-spread`. B5 showed 14 to 22 times more.

By kind of commit, on `zoe-native-qi` (a sync holds the commits that the group commit wrote with it):

| Scenario | Kind | Syncs | KB per sync | Record KB per sync | KB/op | Records/op | Pages per record |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `edit` | indexer batch | 116 | 1,266 | 204 | 757 | 97 (with the next row) | 2.1 |
| `edit` | indexer batch with a save | 12 | 1,177 | 198 | 73 | | |
| `edit` | save | 335 | 95 | 8.7 | 164 | 29 | 1.4 |
| `edit` | no transaction found | 1 | 2,028 | 0 | 10 | | |
| `create-spread` | indexer batch | 158 | 1,921 | 265 | 1,510 | 213 | 1.8 |
| `create-spread` | save | 199 | 160 | 29 | 158 | 34 | 1.2 |
| `create-spread` | no transaction found | 4 | 448 | 0 | 9 | | |

On `zoe-native`, every sync is a save: 1.9 pages per record in `edit` (135 records per operation) and 1.7 in `create-spread` (269). A record of these runs holds 0.55 to 1.4 KB, 1.1 KB on average.

- The indexer writes 83 % (`edit`) and 90 % (`create-spread`) of the store bytes of `zoe-native-qi`: the catalog records that the save wrote itself before ADR 0047. The saves keep 158 to 164 KB per operation.
- The lease: the iterator found no transaction without records in the windows, and `zoe_commits_total` agrees with the saves and the batches within 4 %, so KV-only commits, the lease included, are at most a few per cent of the commits. The indexer writes the lease again in the transaction of each batch (`zoe/plone/indexing.py`, `Indexer`), which costs no commit of its own; the thread writes it alone only when it takes the lease.
- The few syncs with no transaction are at most 1.3 % of the bytes: commits whose tid lies before the cut of the parser, or KV-only commits.
- The zero fill of the file growth wrote 0 to 5 KB per operation.
- The resolution log: 0.7 KB per operation, 341 lines in the run of `create-spread`. B5 counts it, because it counts every file of the process, wherever the file is; it changes nothing that matters.

### The micro-benchmark

200 commits on a copy of the fixture store, each a new revision of N objects at random, with the pickle they hold (413 bytes on average).

| Store | Objects per commit | Record bytes per commit | `wchar` per commit | Write calls per commit | `write_bytes`, warm copy | `write_bytes`, cold copy |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| `zoe.Storage` | 1 | 414 | 57 KB (14.3 pages) | 14.8 | 722 KB | 62 KB |
| `zoe.Storage` | 10 | 4,270 | 146 KB (36.5 pages) | 36.2 | 1,961 KB | 154 KB |
| `zoe.Storage` | 70 | 32,669 | 611 KB (152.7 pages) | 136.7 | 7,790 KB | 739 KB |
| FileStorage | 1 | 414 | 0.5 KB | 2.1 | 4.5 KB | |
| FileStorage | 10 | 4,270 | 8.7 KB | 4.6 | 8.7 KB | |
| FileStorage | 70 | 32,669 | 70 KB | 13.8 | 39 KB | |

- A commit of redb costs about 12 pages, plus 1.94 pages for each record that it changes ((152.7 − 36.5) ÷ 60). The pages of each record are the leaf of `records` that holds the revisions of its oid, since the key is the oid and then the tid, and the branch page above that leaf: the records of one commit are spread over the oids, so their leaves and their branches differ. The fixed pages are the header, the tables `txns`, `txn_oids` and `meta`, and the system tables of redb that track allocated and freed pages. This split comes from the layout of the keys; the spike did not count pages per table.
- FileStorage writes each record once to `Data.fs.tmp` and once to `Data.fs`, so `wchar` is about twice the records; what stays on the device is the append to `Data.fs`.
- Durable KV keys: 20 keys set and 20 deleted add 4 KB per commit (one page), 100 keys add 22 KB.
- These numbers agree with the spike `record-store.md` (34 to 39 KB for one object of 1 KB, 6 to 8 times the pickle bytes for 100 objects).

## Where the bytes go

For one B5 operation of `zoe-native-qi`, at 4 users, before the inflation of the page cache:

1. **The indexer's batches**, 830 KB (`edit`) or 1,510 KB (`create-spread`): 97 or 213 catalog records per operation, at about 2 pages each. One batch serves about one save at this load (0.7 to 0.8 batches per operation), so the same catalog buckets are written again by the next batch.
2. **The saves**, 158 to 164 KB: 29 to 34 records, at 1.2 to 1.4 pages each, and the fixed pages of 1 to 2 commits.
3. **Nothing else of weight**: the queue keys (about one page per commit, in the commits above), the lease (no commit of its own), the resolution log (0.7 KB), the zero fill (0 to 5 KB).

What B5 shows on top, 7,300 to 12,000 KB per operation, is the folio accounting of the page cache. It never reaches the device.

The same ratio holds on `zoe-native`: about 2 pages per record against one append of the record on FileStorage. At 1.1 KB per record, that is 6 to 7 times the bytes.

## Levers

| Lever | Gain | Cost and risk | What it needs |
| --- | --- | --- | --- |
| 1. B5 counts the bytes that each server passes to its store files | the column tells the truth: 1,000 to 1,830 KB against 156 to 292, in place of 8,300 to 16,700 against 496 to 1,006 | small. A counter of the bytes that `OsFile` writes to the database file, exported by `zoe serve` (a new metric, not a wire or format change); for ZEO, the growth of `Data.fs`, or a count in the ZEO probe of the bytes written to `Data.fs`. Keep `write_bytes` under its own name: it is the page-cache charge | proposed card A |
| 2. Drop the store copy from the page cache before the server starts (bench only) | removes most of the inflation in B5 without code in the server | partial: the 1 MB zero fill of each growth makes new large folios, so the count still drifts above the truth as the file grows. Lever 1 is the fix | inside card A, as a stopgap if wanted |
| 3. The indexer batches the keys of several saves | records of the indexer: −27 % (`edit`) and −43 % (`create-spread`) when a batch takes the keys of 1 s, −42 % and −59 % for 2 s (a count of the distinct oids of the batches of the kept stores; the bytes follow the records, at about 2 pages each) | each saved operation waits longer for the catalog: the delay of the indexer (p50 0.2 to 0.5 s now) grows by the window, and the request after a save, with the cookie, waits up to `WAIT` (2 s) for it. At 16 users the batches grow by themselves, so the gain is smaller there. No format or wire change: `zoe.plone` only, and ADR 0047 | proposed card B |
| 4. A record layout that rewrites fewer pages: for example the current revision in a table by oid and the older ones in a table by tid and oid, so that the history of one commit fills one leaf at the end | at most the leaf of the history per record; the leaf of the current revision stays, so about one page per record in place of two (an estimate, not measured) | a format change (red zone): a new key space, conversion of every store, `loadBefore` and pack and undo over two tables | a spike and the human, if lever 3 and the price of the bytes ask for it |
| 5. The append-only log backend of the specification (backend table of "Storage core") | 1.00 to 1.13 bytes per record byte (`record-store.md`): the level of FileStorage | a new backend, its own verify, backup and index rebuild; `record-store.md` found that it fails N6 and N7 as built there. The specification gates it on redb's write path being the bottleneck, and B5 shows no throughput cost: `zoe serve` makes as many or more operations per second than ZEO at these loads | not now |
| 6. Group commit | none at 4 users: 1.00 to 1.06 commits per sync. Saves that overlap already share a sync and its pages | built | — |
| 7. redb settings | none found. Quick repair adds bytes to every commit (`record-store.md`). Durability `None` breaks N1. The page size of redb (`Builder::set_page_size`) is fixed when a file is made: a format change, not measured | — | — |
| 8. zstd records (ADR 0041) | smaller records; a record of 1.1 KB already fits in a page, so the pages per record stay about 2. Not measured | CPU on each read and write | — |
| 9. The resolution log, the lease, the queue keys | under 1 % each | — | — |

### Proposed card A: B5 counts the bytes of the store files

Goal: the column "server KB written/op" of B5 gives the bytes that each server passes to its store files. Steps: `OsFile` of `zoe-core` counts the bytes of the writes to the database file (the writes of `make` and of the zero fill), `zoe serve` exports the count as a metric, and B5 takes its delta in the step window; for `zeo`, B5 takes the growth of `Data.fs` (FileStorage only appends during a run without pack) or counts the writes to `Data.fs` in the ZEO probe. The report keeps `write_bytes` as a second column, "page cache KB/op", with a note on the folios. Scope: `crates/zoe-core/src/vfs.rs`, the metrics of `crates/zoe-server`, `bench/plone`, the specification (the metrics list and B5). No format, wire or Python API change. Acceptance: on one B5 run of `edit` on `zoe-native`, the new column is within 5 % of the strace sum of `pwrite64` bytes to `zoe.redb`, and the `zeo` column equals the growth of `Data.fs`.

### Proposed card B: the indexer batches several saves

Goal: fewer, larger commits of the indexer, so a catalog bucket that several saves change is written once. Steps: the thread of `zoe.plone.indexing` starts a batch when the oldest key of the queue is older than a setting (for example 1 s) or the queue holds `BATCH_SIZE` keys; a request with the cookie keeps its wait. Measure the indexer bytes (with card A), the delay of the indexer and the latency of the request after a save, on B5 `edit` and `create-spread` at 4 and 16 users. Scope: `python/zoedb/src/zoe/plone/indexing.py`, its tests, ADR 0047. Risk: a longer wait after a save; the setting lets a site choose.

## What was not measured

- The step of 16 users. The bytes per record do not depend on the load; the batches of the indexer grow at 16 users, so lever 3 gains less there.
- The bytes that reach the device for each server alone: the device of the store is shared with the rest of the machine. The page-cache test shows that the folio charge does not reach it.
- The pages per table of a redb commit, and lever 4.
- macOS: `proc_pid_rusage` counts other bytes, and APFS has no such folios. The finding is about Linux.
- zstd records and other page sizes of redb.
