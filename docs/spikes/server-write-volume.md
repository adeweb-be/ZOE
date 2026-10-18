# Spike: the bytes that `zoe serve` writes for each B5 operation

Card ZOE-223; requirements N1 to N5 and the column "server KB written/op" of B5.

**Verdict.** Most of the gap is in the measure, not in the store. B5 takes `write_bytes` of `/proc/<pid>/io`, and on this kernel (Linux 7.0, ext4) that counter charges the whole page-cache folio when a write dirties a clean folio: a 4 KB `pwrite` into a file that a large copy put in the page cache counts as 2,048 KB, while the device writes 4 KB. The store of a run is such a copy, so B5 shows 8,500 to 17,600 KB per successful operation for `zoe serve`, about 8 to 10 times the bytes that it really passes to its store file. ZEO is inflated too, about 3 times, for another reason: FileStorage writes each transaction to `Data.fs.tmp` first, and the kernel cancels most of those bytes when the file is truncated. Counted at the system call, `zoe serve` writes 1,000 to 1,830 KB per operation to `zoe.redb`, against 156 to 292 KB that FileStorage appends to `Data.fs`: 5.7 to 6.6 times more, not 20. This is a ratio of the bytes given to write calls; at the device a small append of FileStorage costs at least one 4 KB block and a journal commit, so the device ratio is smaller, and this spike did not measure it. That remaining factor is redb: each record that a commit changes rewrites about 2 pages of 4 KB (1.9 in the micro-benchmark; 1.2 to 2.1 per record in B5, fixed pages included), plus 12 to 17 pages per commit, for records of about 1.1 KB. Queued indexing adds no bytes: its indexer writes the same catalog records that the save wrote before, and its keys cost about one page per commit. The resolution log writes 0.7 KB per operation, and the lease writes no commit of its own in these runs. The levers: count the bytes truthfully in B5 (card A, on the board as ZOE-227, #460), let the indexer batch more saves into one commit (−27 % to −43 % of the indexer bytes with a window of 1 s; the human did not take this card to the board), and, only if the bytes matter more than their price, the write-ahead log of `write-log.md` or the append-only backend of the specification, both format changes.

## Question

Where do the bytes that `zoe serve` writes for one B5 operation go: the store file, the resolution log, other files; for the store, which kind of commit (a save, a batch of the indexer, the lease); how many commits, syncs and pages per commit? Is the comparison with ZEO apples to apples? Which levers would reduce the bytes, at which cost? The runs of ZOE-219 showed 9,300 to 21,200 KB per operation on `zoe-native-qi` with `--split-merges on`, against 440 to 1,020 KB on `zeo`.

## Method

- **Code.** `main` at `582fb0a`, release build of `zoe`, the Plone instances of this worktree (`just plone zeo build`, `just plone zoeclient build`) and the small fixture `small-ba01ff255313` (`just bench plone prepare`).
- **How B5 counts.** The runner samples `/proc/<pid>/io` of each process once a second (`plonebench/sampler.py`); the report divides the `write_bytes` delta of the process `server` in the step window by the successful operations of the step (`report.py`, `server_write_kb_per_op`). The process `server` is the ZEO server (one Python process, `probe/zeoserver/launch.py`) or `zoe serve` (one process). So B5 counts per process, every file that the process writes, not a disk. Each run also records the counters of the block device under the store (`resources.disk`), but that device holds the home directory of the machine, and other jobs wrote to it during every run (see the numbers).
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

Per operation of the step window (55 s), all operations counted, except "B5 KB/op": the `write_bytes` of the server process divided by the successful operations, as the report of B5 does (the two divisions differ by up to 9 %, on the runs with failures). "File growth" is the growth of the files that the server held open. "Records" is the bytes of the records that the transactions of the window wrote. "Txns" is the transactions per operation, by kind. The batch size is `zoe_fsync_batch_size` (sum ÷ count).

| Scenario | Stack | Ops | ok ops/s | B5 KB/op | File growth KB/op | Records KB/op | Txns/op | Commits per sync | Server CPU ms/op |
| --- | --- | ---: | ---: | ---: | --- | ---: | --- | ---: | ---: |
| `edit` | `zeo` | 182 | 3.22 | 510 | `Data.fs` 156 | 153 | 1.93 save | | 14.5 |
| `edit` | `zoe-native` | 200 | 3.55 | 11,067 | `zoe.redb` 329 | 155 | 1.97 save | 1.00 | 13.4 |
| `edit` | `zoe-native-qi`, split off | 191 | 3.47 | 10,039 | `zoe.redb` 344 | 150 | 1.97 save, 0.75 indexer | 1.06 | 10.2 |
| `edit` | `zoe-native-qi`, split on | 203 | 3.69 | 8,477 | `zoe.redb` 324 | 151 | 1.98 save, 0.76 indexer | 1.06 | 10.4 |
| `create-spread` | `zeo` | 152 | 2.56 | 1,085 | `Data.fs` 292 | 285 | 0.91 save | | 24.0 |
| `create-spread` | `zoe-native` | 175 | 3.02 | 17,601 | `zoe.redb` 376 | 282 | 0.95 save | 1.00 | 21.4 |
| `create-spread` | `zoe-native-qi`, split off | 174 | 3.16 | 14,885 | `zoe.redb` 378 | 248 | 1.00 save, 0.80 indexer | 1.00 | 15.5 |
| `create-spread` | `zoe-native-qi`, split on | 183 | 3.33 | 13,684 | `zoe.redb` 360 | 230 | 1.02 save, 0.70 indexer | 1.00 | 14.4 |
| `create-spread` | `zoe-native-qi`, split on, resolution log | 208 | 3.78 | 12,764 | `zoe.redb` 316, log 0.7 | 210 | 1.00 save, 0.62 indexer | 1.00 | 11.4 |

- The B5 numbers of `zoe-native-qi` are of the order of those of ZOE-219 (9,300 to 21,200 KB, over steps of 4 and 16 users).
- Both stacks write the same records: 150 to 155 KB per operation in `edit`, 210 to 285 in `create-spread`. The records of `zoe serve` are raw (ADR 0041), as FileStorage's are.
- `zoe-native` at `582fb0a` (no queued indexing) stands in for the store before ADR 0047: the stack writes the catalog in the transaction of the save, as it did then. It writes as many bytes as `zoe-native-qi`. Split merges change no byte count.
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

Against the growth of `Data.fs` on `zeo` (156 and 292 KB), `zoe serve` writes 6.4 to 6.6 times more in `edit` and 5.7 to 6.3 times more in `create-spread`. B5 showed 13 to 22 times more. These ratios are of the bytes given to write calls; the device ratio is smaller (see the verdict) and was not measured.

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

- A commit of redb costs 12 to 17 fixed pages (14.3 − 1.94 and 36.5 − 10 × 1.94), plus 1.94 pages for each record that it changes ((152.7 − 36.5) ÷ 60). The pages of each record are the leaf of `records` that holds the revisions of its oid, since the key is the oid and then the tid, and the branch page above that leaf: the records of one commit are spread over the oids, so their leaves and their branches differ. The fixed pages are the header, the tables `txns`, `txn_oids` and `meta`, and the system tables of redb that track allocated and freed pages. This split comes from the layout of the keys; the spike did not count pages per table.
- FileStorage writes each record once to `Data.fs.tmp` and once to `Data.fs`, so `wchar` is about twice the records; what stays on the device is the append to `Data.fs`.
- Durable KV keys: 20 keys set and 20 deleted add 4 KB per commit (one page), 100 keys add 22 KB.
- These numbers are of the same order as those of the spike `record-store.md` (34 to 39 KB for one object of 1 KB, 6 to 8 times the pickle bytes for 100 objects). The fixture holds the oids of a whole site and their history, so its trees are deeper than the stores of that spike, and a commit writes a few more pages.

## Where the bytes go

For one B5 operation of `zoe-native-qi`, at 4 users, before the inflation of the page cache:

1. **The indexer's batches**, 830 KB (`edit`) or 1,510 KB (`create-spread`): 97 or 213 catalog records per operation, at about 2 pages each. One batch serves about one save at this load (0.7 to 0.8 batches per operation), so the same catalog buckets are written again by the next batch.
2. **The saves**, 158 to 164 KB: 29 to 34 records, at 1.2 to 1.4 pages each, and the fixed pages of 1 to 2 commits.
3. **Nothing else of weight**: the queue keys (about one page per commit, in the commits above), the lease (no commit of its own), the resolution log (0.7 KB), the zero fill (0 to 5 KB).

What B5 shows on top, 7,500 to 12,000 KB per operation, is the folio accounting of the page cache. It never reaches the device.

The same ratio holds on `zoe-native`: about 2 pages per record against one append of the record on FileStorage. At 1.1 KB per record, that is 6 to 7 times the bytes.

## Levers

| Lever | Gain | Cost and risk | What it needs |
| --- | --- | --- | --- |
| 1. B5 counts the bytes that each server passes to its store files | the column tells the truth: 1,000 to 1,830 KB against 156 to 292, in place of 8,500 to 17,600 against 510 to 1,085 | small, in the bench only: the `wchar` of the server minus the bytes that its TCP sockets sent (see card A); for ZEO, the growth of `Data.fs`. A counter in `OsFile` would need a change of the trait `Observer` (see card A). Keep `write_bytes` under its own name: it is the page-cache charge | card A, ZOE-227 (#460) |
| 2. Drop the store copy from the page cache before the server starts (bench only) | removes most of the inflation in B5 without code in the server | partial: the 1 MB zero fill of each growth makes new large folios, so the count still drifts above the truth as the file grows. Lever 1 is the fix | inside card A, as a stopgap if wanted |
| 3. The indexer batches the keys of several saves | records of the indexer: −27 % (`edit`) and −43 % (`create-spread`) when a batch takes the keys of 1 s, the usable gain (a count of the distinct oids of the batches of the kept stores; the bytes follow the records, at about 2 pages each). A window of 2 s would give −42 % and −59 %, but it reaches `WAIT` | each saved operation waits longer for the catalog: the delay of the indexer (p50 0.20 to 0.27 s in these runs, `indexing` of `result.json`) grows by the window. The request after a save, with the cookie, waits at most `WAIT` (2.0 s, `zoe/plone/indexing.py`), and `request_started` does not wake the indexer, so the window must stay well under `WAIT`, or `WAIT` must rise, which lengthens the worst wait of a user. At 16 users the batches grow by themselves, so the gain is smaller there. No format or wire change: `zoe.plone` only, and ADR 0047 | card B; the human did not take it to the board |
| 4. A record layout that rewrites fewer pages: for example the records keyed by `(tid, oid)` with a second index, so that the records of one commit fill leaves at the end | about one page per record in place of two (an estimate, not measured here). This is close to the layout of ZOE-174, which the human rejected: the write-ahead log writes fewer bytes for an `update` commit (168 KB in all against 195 KB) with no second lookup on the read path (`write-log.md`, "Cards", item 3) | format change, red zone: a new key space, conversion of every store, `loadBefore`, pack and undo over two tables | rejected; not proposed again |
| 5. The write-ahead log with a background checkpoint of `write-log.md` | for 100 records of 1 KB, 168 KB written in all and 63 KB synced by the commit, against 696 KB written and synced today; the verdict there is "Go with conditions" | format change, red zone: a new format version, every read path reads the table of unfolded records first, pack waits for a fold, the crash simulation covers the log. At 100 KB the log writes each record twice and its checkpoint competes with the commits for the device | the card that `write-log.md` proposes |
| 6. The append-only log backend of the specification (backend table of "Storage core") | 1.00 to 1.13 bytes per record byte (`record-store.md`): the level of FileStorage | format change, red zone. A new backend, its own verify, backup and index rebuild; `record-store.md` found that it fails N6 and N7 as built there. The specification gates it on redb's write path being the bottleneck, and B5 shows no throughput cost: `zoe serve` makes as many or more operations per second than ZEO at these loads | not now |
| 7. Group commit | none at 4 users: 1.00 to 1.06 commits per sync. Saves that overlap already share a sync and its pages | built | — |
| 8. redb settings | none found. Quick repair adds bytes to every commit (`record-store.md`). Durability `None` breaks N1. The page size of redb (`Builder::set_page_size`) is fixed when a file is made; not measured | format change, red zone, for the page size | — |
| 9. zstd records (ADR 0041) | smaller records; a record of 1.1 KB already fits in a page, so the pages per record stay about 2. Not measured | CPU on each read and write | — |
| 10. The resolution log, the lease, the queue keys | under 1 % each | — | — |

### Card A: B5 counts the bytes of the store files (ZOE-227, #460)

A decision of the human put this card on the board as ZOE-227 (#460). Its steps, as written there, count in `OsFile` and export a metric from `zoe serve`: that route meets the stop below. The bench route after it reaches the same column without a change of the core.

Goal: the column "server KB written/op" of B5 gives the bytes that each server passes to its store files.

**No route through the core without a stop.** The core keeps no metric and has no metrics dependency (`crates/zoe-core/CLAUDE.md`, the paragraph on the observer). A counter of `OsFile` would leave the core through the trait `Observer`, which `zoe-server` implements: a new method changes a trait signature that another crate uses, a stop-and-ask item, even with a default that does nothing.

**A route without a core change.** The server process gives every byte of its write calls in `wchar` of `/proc/<pid>/io`, and its only files are the store and, when it has one, the resolution log. In the four strace runs, `wchar` of the side sampler was 0.6 % to 2.3 % below the strace sum of all write calls (store, sockets, eventfd); the two windows differ by the sampling interval. The bytes that the TCP sockets of the server sent are in `bytes_sent` of `ss -ti` for the sockets on the server port, and the instances keep their connections for the whole run. So B5 can take, in the step window, the delta of `wchar` minus the delta of `bytes_sent` (minus the growth of the resolution log, if any) for `zoe serve`, and the growth of `Data.fs` for `zeo` (FileStorage only appends during a run without pack; its `wchar` also holds the writes to `Data.fs.tmp`, which the kernel cancels). The report keeps `write_bytes` as a second column, "page cache KB/op", with a note on the folios. On macOS, `proc_pid_rusage` has no `wchar`; the column stays absent there, as the sampler does for the other counters that macOS lacks.

Cost: small, `bench/plone` only (the sampler, the runner and the report), and the B5 paragraph of `bench/README.md`. No change of the core, the server, the format, the wire or a Python API. Risk: the socket counters miss bytes still in the send queue at a sample (small on loopback), and a connection that closes inside the window takes its count with it; a run can check that the connections of the window stay the same. Acceptance: on one B5 run of `edit` on `zoe-native`, the new column is within 5 % of the strace sum of `pwrite64` bytes to `zoe.redb`, and the `zeo` column equals the growth of `Data.fs`.

### Card B: the indexer batches several saves (not on the board)

A decision of the human did not take this card to the board. It stays here as a lever.

Goal: fewer, larger commits of the indexer, so a catalog bucket that several saves change is written once. Steps: the thread of `zoe.plone.indexing` starts a batch when the oldest key of the queue is older than a setting, well under `WAIT` (for example 1 s against 2.0 s), or the queue holds `BATCH_SIZE` keys; a request with the cookie keeps its wait. Measure the indexer bytes (with card A), the delay of the indexer and the latency of the request after a save, on B5 `edit` and `create-spread` at 4 and 16 users. Scope: `python/zoedb/src/zoe/plone/indexing.py`, its tests, ADR 0047. Cost: a longer wait after a save, and a raise of `WAIT` if the window comes near it.

## What was not measured

- The step of 16 users. The bytes per record do not depend on the load; the batches of the indexer grow at 16 users, so lever 3 gains less there.
- The bytes that reach the device for each server alone: the device of the store is shared with the rest of the machine. The page-cache test shows that the folio charge does not reach it.
- The pages per table of a redb commit, and lever 4.
- macOS: `proc_pid_rusage` counts other bytes, and APFS has no such folios. The finding is about Linux.
- zstd records and other page sizes of redb.
