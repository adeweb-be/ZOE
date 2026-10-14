# Spike: one layout of the client cache, for one process, a file and the processes of a node

Card ZOE-152, requirements F28, F38, F9 and N4. The ADR draft is `docs/adr/0034-the-client-cache-file.md`.

**Verdict.** One layout serves the three stages: a header page, a shared index of slots and a FIFO ring of checksummed entries, in one mapping. The mapping is anonymous in one process (ZOE-156), a private file (ZOE-162), or one file that the processes of a node map together (F38). A hit takes no lock and writes nothing. In one thread, a hit of the prototype costs 0.46 µs against 4.05 µs for `ZEO.cache` when Python calls the cache, and 5.6 µs against 9.6 µs through ZODB: ZODB itself, not the cache, is then most of the cost of a hit. With 16 threads, as zodbshootout's `read_hot` counts them, a hit costs 13.9 µs of one thread's time against 585 µs, because `ZEO.cache` releases the GIL in each file read and the threads wait for each other. zstd per entry doubles the cost of a hit of a 2.8 KB pickle, so small entries stay raw. Six worker processes on one file, one of them far behind, with a SIGKILL every 200 ms, served 34.9 million hits in three runs and no stale answer; the same run without the rule for current stores served 408,416 stale answers. A machine restart drops a file that was open; a process that dies drops nothing but its torn entry. The prototypes are on the branch `poc/ZOE-152-client-cache` ([`3fbfb5f`](https://github.com/adeweb-be/ZOE/tree/3fbfb5f/bench/poc/ZOE-152)), not for merge.

## Question

The card asks six questions:

1. **Layout and hit.** How does a lookup of (oid, before) work in a mapping? How does the cache evict under `cache-size`? Is the data compressed (zstd per entry, or none under a size)? What does a hit cost through ZODB, against a hit of `ZEO.cache` on the same machine?
2. **Crash safety.** What happens after a process killed while it writes an entry, a torn page, a file cut short?
3. **Several processes.** Which process writes an entry, and how do writers exclude each other? How does a reader at snapshot S know that a current entry is valid for S? How is each invalidation applied once per node while each process still gives it to its own DB? What happens when one process lags?
4. **Verification** at the open against the server, with one process and with several.
5. **`clear`**, which zodbshootout calls, with several processes.
6. **ZEO's lock.** Which behaviour does the native client keep for one process, and how does F38 depart from it?

## Method

1. **The reference.** `ZEO/cache.py` (`ClientCache`) and `ZEO/asyncio/client.py` (`verify`, `tpc_finish_co`, `invalidateTransaction`, `load_before`) of ZEO 6.2, the version of the benchmark environment.
2. **The prototype.** `bench/poc/ZOE-152/cachex` on the POC branch: the layout in Rust (`src/cache.rs`, about 1,000 lines), with a PyO3 module `_cachex` whose `Cache` has the methods of `ClientCache` that `ZEO.ClientStorage` calls (`loadBefore`, `store`, `invalidate`, `setLastTid`, `getLastTid`, `clear`, `contents`, `close`, `__len__`). `ZEO.ClientStorage` takes a cache object as its `cache` argument, so the stock client runs on the prototype unchanged: the hits go through ZODB, the MVCC adapter and `ClientStorage.loadBefore`, and the I/O thread of ZEO calls `store`, `invalidate` and `setLastTid`.
3. **The model checks.** `sim run` forks worker processes on one shared file. A history of 20,000 transactions over 5,000 oids comes out at one tid per 250 µs; a hot twentieth of the oids takes three quarters of the writes. Each worker reads its own stream of invalidations, applies it to the cache, commits the transactions that it authors (the data goes in at `setLastTid`, as in `tpc_finish_co`), and reads random oids at snapshots up to 30 tids old. A miss stores the reply of a model server: the end of the reply is known only up to the stream of the worker. Worker 0 lags: it reads its stream about one step in 40 rounds. The parent kills a worker other than worker 0 with SIGKILL every 200 ms and starts a new one, which opens the file and verifies it. Every hit is checked against the history: the start tid, the end tid and the bytes. At the end, the parent opens the file alone and checks every oid at three snapshots. `sim damage` fills a private file with the same model, closes it cleanly, then flips random bytes, flips bits in the header, cuts the file at a random length, and mixes the 4 KiB pages of two images of an open file, as a power loss does. Each damaged file is opened and every oid is read at four snapshots.
4. **Canaries.** `CACHEX_NO_SEEN_RULE` turns off the rule for current stores (question 3), and `CACHEX_NO_BOOT_RULE` turns off the boot rule (question 2). A check that never fails proves nothing; with each canary on, the same checks must find stale answers.
5. **Hit cost.** `hit.py` runs a ZEO 6.2 server in a thread (no load reaches it during the rounds) and one `ZEO.ClientStorage` per kind of cache: `zeo` (`ClientCache` on a temporary file, as the `<zeoclient>` of the harness without `client`), `zeo-file` (a named file), `proto-memory`, `proto-file`, `proto-shared`, `proto-shared-zstd` (zstd level 1 on every entry), and `mapping`, a ZODB on `MappingStorage` without ZEO, as a floor. The objects are zodbshootout's `PObject` with its data; for "1 KB" objects the pickles are 2.5 to 3.0 KB. A round does what `read_hot` does: `cacheMinimize`, then read `attr` of each object, so each object costs one `loadBefore` that the cache answers. The kinds take turns inside each round. A second number, "Python calls the cache", calls `cache.loadBefore` in a loop, without ZODB. `micro` measures the lookup in Rust, zstd on real pickles (`dump_pickles.py`) and the writer lock.
6. **Machine and lock.** A developer workstation, not the reference machine: Intel Core Ultra 7 155H (22 hardware threads), 30 GB, Linux 7.0, CPython 3.13.15, the cache files on tmpfs. Another builder used the machine at the same time. Every timing run held an exclusive `flock` on a lock file that the jobs of both builders share, so no job of either builder ran during it. The tables give the median and, in brackets, the range of the rounds.
7. **A disturbed run.** The first timing run (`results/run1`) is kept but not used: during it, a `cargo test --workspace` of a third session, which does not take the lock, ran the zoe-server tests. That run also had two faults of `hit.py` (a negative wall time with threads, and a 20 MB cache too small for the 100 KB cells), fixed for the second. The second run (`results/run2`) is the one in the tables. It started when no `cargo`, `pytest` or test binary ran but one: the third session's `cargo test` (pid 103308) and its zoe-server test binary (pid 123628), which had hung in two tests for 26 minutes. It was nice'd (nice 5) and had used 8.6 % of one core over its life (2 min 14 s of CPU in 25 min 56 s), and it stayed so during the run (`results/run2.ps-before.txt`, `run2.ps-after.txt`). The load average was 0.39 at the start and 1.51 at the end; the run took 68 s of wall time.

## Layout

One mapping: a header page, an index, a ring. The same code runs on an anonymous mapping (stage 1), a private file (stage 2) and a shared file (stage 3).

| Part | Holds |
| --- | --- |
| Header, 4 KiB | magic `ZOECACH\x01`; size of the ring; number of slots; a checksum of these three; the boot id of the kernel; the clean flag; `applied`, the tid up to which the cache applied the invalidations; `head`, the logical write position of the ring; a checksum of clean, `applied` and `head`, which a clean close writes |
| Index | slots of 16 bytes `(oid + 1, position)`, a power of two, ring ÷ 256 slots; open addressing, linear probes of at most 32 slots. A slot points at the newest entry of its oid. A slot is never emptied: a slot whose entry is gone is reused by the next insert that passes it |
| Ring | entries of 72 bytes of head plus the data, aligned to 8 bytes; an entry never crosses the end of the ring |

An entry holds its own logical position, the oid, the start tid, the end tid (0 for a current entry), the position of the previous entry of the same oid, the stored length, the raw length, the codec, the checksum of the stored bytes (xxh3) and the checksum of the 64 bytes of head before it. ZEO's block has 43 bytes of overhead and no checksum.

**Logical positions.** `head` counts bytes from the start and never wraps; byte `p` lives at `p mod ring`. An entry is intact while `head ≤ p + ring`. A writer moves `head` before it writes, so a reader that copied an entry and then finds `head > p + ring` knows that a writer reached it. A position in a slot or in an entry that the ring overwrote fails the check of the position stored in the entry, so the index needs no cleaning.

**Lookup of (oid, before).** Load `applied`, find the slot of the oid, then follow the chain of entries of the oid, newest insert first, at most 8. Serve the first entry that is valid: `start < before`, and either `end ≥ before` (a non-current entry), or `end = 0` and `before ≤ applied + 1` (a current entry). Copy the data, check both checksums, check that the ring did not reach the entry during the copy. Anything else is a miss. Each entry is a true statement about its oid, so any valid entry gives the right revision: the order of the chain does not matter for correctness.

**Eviction under `cache-size`.** The ring is FIFO, as in ZEO: a new entry overwrites the oldest bytes, and the entries there leave the cache. A hit of a current entry in the oldest fifth of the ring copies it to the head, as the `rearrange` of ZEO does, but only when the writer lock is free, so a hit never waits. An entry larger than half the ring is not cached (ZEO: larger than the file). `cache-size` is the size of the ring; the header and the index add about 6 %.

**Compression.** The codec is per entry: raw, or zstd. On the pickles of zodbshootout (2,000 pickles of 2,840 bytes on average from "1 KB" objects, 200 pickles of 103,545 bytes from 100 KB objects), with a reused zstd context, in µs per record, median of 21 rounds (range in brackets):

| Pickles | Codec | Stored ÷ raw | Compress | Decompress |
| --- | --- | --- | --- | --- |
| 2.8 KB | copy, no codec | 1 | | 0.09 |
| 2.8 KB | xxh3 of the data | | | 0.14 |
| 2.8 KB | zstd −5 | 0.865 | 4.3 | 1.25 (1.24–1.92) |
| 2.8 KB | zstd −1 | 0.727 | 7.3 | 1.88 (1.87–1.97) |
| 2.8 KB | zstd 1 | 0.536 | 10.6 | 3.87 (3.85–4.19) |
| 2.8 KB | zstd 3 | 0.531 | 15.9 | 4.52 (4.48–4.74) |
| 100 KB | copy, no codec | 1 | | 3.2 |
| 100 KB | xxh3 of the data | | | 6.5 |
| 100 KB | zstd −5 | 0.546 | 225 | 56 (55–60) |
| 100 KB | zstd −1 | 0.444 | 287 | 69 (69–71) |
| 100 KB | zstd 1 | 0.351 | 277 | 70 (69–74) |
| 100 KB | zstd 3 | 0.321 | 348 | 76 (75–80) |

A whole hit of the prototype in Rust, without Python (`micro lookup`: lookup, copy, both checksums, ring check), takes 207 ns (202–272) for a raw 2.8 KB entry and 4,040 ns (3,981–4,425) for the same entry in zstd 1: decompression is 95 % of a compressed hit. For 100 KB: 6.3 µs raw, 70.5 µs in zstd 1. zstd at level 1 halves a small entry and divides a large one by three, so the same `cache-size` holds two or three times more entries. But each hit of a compressed entry costs one decompression, and a hit is the path that N4 measures. So the prototype stores an entry raw unless `compress_min` is set, and the hit table measures zstd on every entry as the other bound. LZ4, which decompresses faster than zstd at a lower ratio, was not measured: it is not a dependency of the workspace.

**The three stages.** Stage 1 is an anonymous mapping, with the writer lock a mutex of the process. Stage 2 adds the file, the boot rule and the clean close. Stage 3 adds the shared `flock`, the writer lock across processes and the per-process view of question 3. The open in every stage reads one header page: it never scans the file. ZEO reads every block at its open to build its index.

## Hit cost

`hit.py`, run 2. µs per hit; the median of the rounds, the range in brackets. "Through ZODB": `cacheMinimize`, then one attribute read per object, through `Connection.setstate`, the MVCC adapter and `ClientStorage.loadBefore` of the stock client. "Python calls the cache": `cache.loadBefore` in a loop, without ZODB. In each round, the kinds take turns.

**One thread, 1,000 objects, pickles of 2.5 to 3.0 KB, 30 rounds.**

| Cache | Through ZODB | Python calls the cache |
| --- | --- | --- |
| `ZEO.cache` hit, temporary file (the harness's `<zeoclient>`) | 9.63 (9.45–11.12) | 4.05 (3.94–4.47) |
| `ZEO.cache` hit, named file | 9.44 (9.28–9.86) | 4.06 (3.94–4.73) |
| prototype hit, memory | 5.51 (5.41–6.04) | 0.43 (0.41–0.61) |
| prototype hit, private file | 5.57 (5.42–5.81) | 0.45 (0.41–0.79) |
| prototype hit, shared file | 5.56 (5.43–6.03) | 0.46 (0.43–0.82) |
| prototype hit, shared file, zstd 1 on every entry | 10.38 (10.19–11.49) | 5.00 (4.96–5.52) |
| ZODB on `MappingStorage`, no ZEO (a floor) | 6.85 (6.67–7.13) | |

**16 threads, each with its own connection and its own 100 objects, 30 rounds.** "Wall" is the time from the first start to the last end ÷ the hits of all threads. "One thread" is the time of one thread ÷ its hits, the mean of the threads: zodbshootout counts this one, `ops/s = 16 × 100 ÷ mean`.

| Cache | Wall | One thread |
| --- | --- | --- |
| `ZEO.cache` hit, temporary file | 39.6 (24.0–44.2) | 585 (376–661) |
| `ZEO.cache` hit, named file | 37.0 (32.4–42.9) | 561 (483–634) |
| prototype hit, memory | 14.8 (10.9–17.0) | 13.5 (10.1–15.7) |
| prototype hit, private file | 15.0 (11.0–17.0) | 13.6 (10.4–15.7) |
| prototype hit, shared file | 15.4 (9.7–16.5) | 13.9 (8.9–16.2) |
| prototype hit, shared file, zstd 1 | 22.1 (16.0–27.1) | 21.1 (15.5–33.6) |
| ZODB on `MappingStorage` | 18.6 (14.7–35.0) | 17.5 (14.0–502) |

**One thread, 200 objects of 100 KB, a 256 MB cache, 15 rounds.**

| Cache | Through ZODB | Python calls the cache |
| --- | --- | --- |
| `ZEO.cache` hit | 34.9 (33.2–36.4) | 19.9 (19.2–20.8) |
| prototype hit, shared file | 25.7 (24.7–27.5) | 11.7 (11.3–12.2) |
| prototype hit, shared file, zstd 1 | 102.8 (99.6–106.1) | 90.1 (89.1–91.8) |

Each prototype kind counted at most 4 misses in its whole run, against more than 30,000 hits: the timed rounds were hits.

What the numbers say:

- **The cache is no longer the cost.** Called from Python, a hit of the prototype costs 0.46 µs, a ninth of a `ZEO.cache` hit; 0.2 µs of it is the work in Rust, the rest is the call and the two `bytes` objects. Through ZODB, a hit costs 5.6 µs against 9.6 µs, 1.7 times less: about 5 µs of each hit is ZODB (`setstate`, the MVCC adapter, `ClientStorage.loadBefore`, the unpickling), whatever the cache. `MappingStorage`, a dict behind a lock in Python, costs more than the prototype.
- **Private, shared or in memory costs the same on a hit.** The three prototype kinds are within their ranges of each other: a shared file costs nothing on the read path.
- **zstd doubles a small hit and quadruples a large one** through ZODB (10.4 against 5.6 µs; 103 against 26 µs). That is the reason for raw entries by default.
- **With threads, `ZEO.cache` waits.** Each read of `ZEO.cache` is a `seek` and a `read` of the file, and each `read` releases the GIL, so the 16 threads interleave: one thread takes 585 µs per hit, though the hits of all threads pass at one per 40 µs of wall time. A prototype hit never releases the GIL, so one thread runs its 100 hits in one stretch: 13.9 µs per hit. zodbshootout counts the time of one thread, so in the threads cells of `read_hot` this is 42 times fewer µs per hit, which is far more than the 3× of N4. These numbers agree with the planner's: 16 × 100 ÷ (100 × 585 µs) is 27,000 ops/s for ZEO, against 29,768 and 33,602 from zodbshootout.
- **For N4, the processes cells are the hard ones.** In a process of its own, a hit has no other thread to wait for, so the single-thread numbers apply: 5.6 against 9.6 µs, about 1.7×, below the 3× of N4. Even a cache that cost nothing would leave the 5 µs of ZODB: at most about 1.9×. This is an estimate from one thread per process; zodbshootout's processes mode was not run. Reaching 3× in the processes cells of `read_hot` needs less work in ZODB's load path, outside the cache (a question for the human below).

## Crash safety

Three rules. Each entry has two checksums (head and data); a doubt is a miss, never a hit. A process that dies leaves the page cache as it was, so only its torn entry is bad. A machine that restarts loses the page cache in an unknown order, so an open file is not trusted after it.

- **A writer killed while it writes an entry.** The writer moves `head`, writes the entry, and only then points the slot at it. A torn entry is reachable from no slot, or fails a checksum. In three runs of `sim run`, 90 kills landed at random points of the workers; no hit was stale or wrong, and no open failed.
- **A writer killed in an apply.** The ends of a transaction go in before `applied` moves. An end at T on an entry is true whatever happens next, so a partial apply leaves only true entries, and the next process applies T again: an end that is there already is not written twice.
- **The end of an entry** is the only field that changes in place. It is under the head checksum: the writer stores the end, then the new checksum. A reader that sees one without the other misses.
- **The writer lock** is an open-file-description lock, which the kernel releases when its process dies. A killed holder blocks nobody.
- **A machine restart, a torn page.** At the open, a file whose boot id differs from the kernel's and that was not closed cleanly drops every entry. A clean close (the last process of the file) syncs the mapping, sets the clean flag and the state checksum, and syncs the header: that file survives a restart. In `sim damage`, 200 files whose pages mix two images of an open file, opened under another boot id, were all dropped. The canary, 200 such files without the boot rule, served 24,059 stale answers out of 1,124,941 hits: a page with an older entry and a header page with a newer `applied` make a current entry lie. The boot rule is necessary.
- **Bit flips.** 300 files with 1 to 8 random bit flips anywhere: 2,245,873 hits, none bad. 200 files with one bit flip in the first 88 bytes of the header: 134 dropped by a header or state checksum, the rest flipped bytes that change no answer (the boot id of a clean file); none bad. A flip in an index slot gives a miss: the entry it reaches has another oid or another position.
- **A file cut short.** The length no longer matches the header: the open drops every entry. 200 of 200 cut files dropped.
- **A drop never truncates a file that another process maps.** Only a process that holds the file alone (an exclusive `flock` that succeeds) may recreate it. A process that joins a shared file with a bad header does not open it and runs without the shared cache.

The cache is never the only copy of anything: it holds what the server sent, and the data of a commit only after the server confirmed the commit (`setLastTid` of `tpc_finish_co`). A drop loses no data.

## Several processes

**The writers.** Every process writes: the one that missed stores the reply of its load, and the one that reads a transaction first applies it. Writers exclude each other with an open-file-description lock (`F_OFD_SETLKW`) on byte 0 of the file, and inside a process with a mutex: an OFD lock does not exclude two threads of one process. A lock and an unlock of the OFD lock cost 467 ns, against 9 ns for the mutex alone (`micro lock`). The apply of a transaction of 100 oids that the cache holds, lock included, takes 7.1 µs (6.1–13.9). The lock is taken on a miss, an apply and a commit, never on a hit. A robust mutex in the mapping would be faster, but its recovery after the death of its owner is more code to get right for a lock that is taken only on a write.

**The tid of the node and the tid of a process.** The header holds `applied`: the node applied every transaction up to it. Each process keeps its own last tid: the tid up to which it read its stream. That last tid is its `lastTransaction`, so its DB never takes a snapshot that its own object caches have not seen the invalidations of. `applied` is never below the last tid of a process that is up to date, because a process applies a transaction before it moves its own tid.

**A current entry is valid for S when S ≤ `applied`.** Invariant: when an entry `(oid, s, 0)` is in the file, the oid has no revision in `(s, applied]`. Two rules keep it:

1. **Apply.** On `setLastTid(T)` (after the `invalidate` calls of T, as ZEO calls them), a process takes the writer lock. If `applied ≥ T`, another process applied T: it skips the ends and gives T to its own DB only. Otherwise it ends the current entries of the oids of T at T, stores the data of its own commit if T is its commit, then moves `applied` to T. If `applied` is below the previous tid of the process, the cache missed transactions that no process holds: it drops every entry and moves `applied` to T. An empty cache is true at any tid.
2. **Store.** A load reply that says "current" is current up to the stream of the process that got it, because the server sends every invalidation before the reply that follows it. A process stores a current entry only when `applied` is not above its own last tid. A non-current entry `(s, e)` is true for ever and always goes in.

**A commit.** `tpc_finish_co` calls `invalidate(oid, T)`, then `store(oid, T, None, data)`, then `setLastTid(T)`. The prototype keeps the stores of T with the invalidations and applies them together. When another process applied T first (it read the invalidation of T before this process read its own `tpc_finish` reply), the data still goes in, current, when `applied` is still T.

**Each invalidation once per node, and once per DB.** In each run of `sim run`, the 20,000 tids were applied once each (19,996 to 19,999 applies counted; a process killed between the apply and the count leaves the rest) and skipped 80,406 to 80,467 times by the other processes, which still gave them to their own model DB.

**A process that lags.** It holds no lock while it lags, so no other process waits for it. Its snapshot is older than `applied`. A current entry newer than its snapshot fails `start < before`, a current entry older than its snapshot is still true for it, and an ended entry serves it as for any reader. Its loads are current only up to its own stream, below `applied`: the store rule refuses them as current, so it gets more misses and stores nothing stale. In `sim run`, worker 0 ended 19,518 to 19,571 tids behind; the store rule refused 129,474 to 130,245 current stores (counted in the workers that ended cleanly). Without the rule (the canary), the run served 408,416 stale answers to the workers, and 381 to the final check.

| `sim run`, 6 workers, 20,000 tids | seed 1 | seed 2 | seed 3 | canary: no store rule |
| --- | --- | --- | --- | --- |
| reads | 11,278,531 | 12,994,183 | 11,904,575 | 12,382,632 |
| hits | 10,867,304 | 12,588,421 | 11,471,781 | 12,034,918 |
| of them current | 9,795,998 | 11,340,287 | 10,332,386 | 10,870,658 |
| SIGKILLs, opens | 30, 36 | 30, 36 | 30, 36 | 30, 36 |
| clears | 45 | 44 | 53 | 44 |
| largest lag, in tids | 19,518 | 19,571 | 19,536 | 19,521 |
| stale or wrong answers | 0 | 0 | 0 | 408,416 |
| final check by the parent: hits, bad | 14,985, 0 | 10,359, 0 | 14,947, 0 | 14,997, 381 |

Each run lasts 6.3 s. The final check reads every oid at three snapshots.

**ZEO's lock (question 6).** `ZEO.cache` takes `zc.lockfile` (an exclusive `flock` and a pid) on `<file>.lock`, so a second process on the same file fails with `LockError`. The private file of stage 2 keeps that behaviour: an exclusive `flock`, and a second process fails at the open. The native client gives its file another name than ZEO's, `{client}-{storage}.zoec`, so it never takes or rewrites the `.zec` file of a stock client of the same `client` name. The shared file of F38 departs from it on purpose: each process holds a shared `flock`, and a configuration key (`shared-cache true`) chooses it. A private and a shared open of the same file exclude each other, so a process that expects to be alone never finds another writer.

## Verification

**One process (ZOE-162)** keeps `verify` of `ZEO/asyncio/client.py` as it is, with `getLastTid` as the tid of the cache. `verify.py` ran it through the stock `ZEO.ClientStorage` on the prototype as a private file, against a ZEO 6.2 server with an invalidation queue of 5, and reopened the client after each step:

| Step | `verify_result` of ZEO | Reads of the 50 objects | Newest values |
| --- | --- | --- | --- |
| first open, new file | `empty cache` | | |
| reopen, no commit in between | `Cache up to date` | 52 hits, 0 misses | yes |
| reopen after 1 commit of 3 objects by another client | `quick verification` | 49 hits, 6 misses | yes |
| reopen after 20 commits of 2 objects | `cache too old, clearing` | 2 hits, 100 misses | yes |
| the same file against a new, empty server | `Cache newer than server` | | |

The misses count twice per load, because `ClientStorage.loadBefore` and `load_before_co` both ask the cache. A quick verification drops the current entry of each oid that `getInvalidations` names and keeps the ended ones, as `invalidate(oid, None)` of ZEO does. A second private open of the file and a shared open of it both failed with "the cache file is locked".

**Several processes (F38).** At its open, a process takes the tid of its `register`, R, and reads `applied`, A, which the other processes move while it verifies.

- A = R: nothing to do; the last tid of the process is R.
- A < R: `getInvalidations(A)`. With an answer `(R', oids)`, the process drops the current entries of the oids and applies R' with the apply rule: when another process moved `applied` to R' or beyond in the meantime, the drop is skipped, which is right, because that process applied the same transactions. With `None`, it clears the cache and applies R. The call to the server holds no lock.
- A > R: normal with several processes, because the others read commits that came after the `register` of this one. The process starts at R and skips the applies up to A. A cache file of another history has the same shape, so the process calls `lastTransaction` once its stream reaches past R: a server tid below A means another history, and the process clears the file and applies its own tid. ZEO's `AssertionError` stays for a private file only. An extension that gives the identity of the store at `register` would make this exact; it is a wire-protocol change, left to the human below and out of the scope of the card.

In `sim run` every new worker verified this way against a file that the others kept moving: 36 opens per run, none failed, no stale answer after them. The model has one history, so the check of another history did not run.

## Clear

`ZEO.cache.clear` truncates the file and keeps the tid of the cache. The prototype's `clear` takes the writer lock and moves `head` two rings forward: every entry fails its position check at once, the index slots become reusable, and `applied` stays. It writes 8 bytes instead of the file. With a shared file, `clear` empties the cache of every process of the node, and the readers of the other processes miss from then on; their snapshots stay correct, because an empty cache is true at any tid.

zodbshootout calls `db.storage._cache.clear()` in `read_cold` and in `read_warm`. In the threads mode only worker 0 calls it, between two barriers. In the processes mode every worker calls it, without a barrier. On a shared file, the clear of one worker then empties the cache of the others too, at a moment that the others do not know. Each worker reads its own objects, and `read_cold` wants misses, so this adds misses only to cold reads that miss anyway. A named file also stays warm from one `ClientStorage` to the next: `read_cold` opens a new DB each loop and stays cold only because it clears first. With ZEO, a client without `client` (the harness's `<zeoclient>`) gets a new temporary file each time, so it starts cold. The native client keeps that: without `client` it uses stage 1, and the shared file is never the default. In `sim run`, 44 to 53 clears per run came at random times from random workers; no stale answer followed.

## Risks and limits

- The prototype copies the bytes of an entry with plain reads while a writer may overwrite them, then checks the head of the ring and the checksums: the pattern of a seqlock. On x86-64 and Linux it is sound; in the Rust memory model it is a data race. The build card must use atomic or volatile copies, and run the kill loop under ThreadSanitizer as the TSan gate of the phase does.
- The index never grows. With 2.8 KB entries it is far from full; with many tiny entries a full probe window means the entry is not indexed, so a miss. The build card must count this.
- The chain of an oid is followed for 8 entries at most. An older revision behind 8 newer inserts of the same oid is a miss.
- The writer lock is a system call pair per write; a cold start of 16 processes serializes their stores on it.
- The hit numbers are from one workstation with another builder on it; the free-threaded build (3.14t) and the reference machine were not measured.
- `sim run` models the server; it does not run `zoe serve`. The stock client ran on the prototype only in one process.
- In `sim run`, no process found `applied` below its previous tid, so the path that drops the cache for a gap never ran.
- zodbshootout itself did not run on the prototype; `hit.py` does what `read_hot` does, and its ZEO numbers agree with zodbshootout's.

## Decisions for the human

1. **The layout** of the ADR draft 0034: one mapping, a shared index, a FIFO ring with checksums, for the three stages.
2. **Compression.** F28 says "compressed". On these pickles, zstd per entry doubles the cost of a hit through ZODB for 2.8 KB pickles and quadruples it for 100 KB pickles, against two to three times more entries in the same `cache-size`. The proposal: raw entries by default, and zstd per entry as a setting for the sites that need the capacity more than the hit. Then the row F28 changes from "compressed" to "compression as a setting". The alternative is to keep compression on and accept the cost, or to measure LZ4 first.
3. **The name of the file** of the native client: `{client}-{storage}.zoec`, not ZEO's `.zec`.
4. **The configuration key** of F38: `shared-cache true` on a client with `client`; never the default.
5. **The check of another history** with a shared file: `lastTransaction` after the stream passes R, or a new extension that names the store at `register` (a wire-protocol change).
6. **N4 in the processes cells of `read_hot`.** The cache brings a hit through ZODB from 9.6 to 5.6 µs in one thread; the 3× of N4 in a process needs the ZODB part of a load to shrink as well (a native `loadBefore` path in `zoe.zeo.ClientStorage` without the Python frames of the MVCC adapter and of `ClientStorage`, or work in ZODB itself). Decide whether a card measures that path, or whether the gate of the processes cells of `read_hot` changes.

## Card for the shared cache

```markdown
# ZOE-NNN — the shared client cache: one file per node for the native client
Phase 3 · crate zoe-client (cache), zoe-py, python/zoedb · red zone: no

Requirement: F38 ("one shared mmap'd cache per node for the native client"; "two workers on one node share one cache and apply each invalidation once"), with F28 (the persistent cache) and F9 (cache verification).
Goal: the client processes of a node that name the same cache file with `shared-cache true` map one file. Each invalidation is applied to the file once per node, and each process still gives it to its own DB. No process serves a stale entry, also after a SIGKILL of any process at any point.
Acceptance tests (do not edit): crates/zoe-client/tests/shared_cache.rs (the kill loop of the spike: 6 processes, one that lags, a SIGKILL every 200 ms, against a model of the history; zero stale answers, each tid applied once, and the same run without the rule for current stores finds stale answers); python/zoedb/tests/test_shared_cache.py (two processes with `zoe.zeo.ClientStorage` on one file against `zoe serve`: a commit in one is a hit in the other at the next snapshot; `applied` moves once per transaction; `clear` in one empties the other; a process that joins verifies against `getInvalidations`; a private open of a shared file fails, and a shared open of a private one).
Scope fence: crates/zoe-client/src/cache/**, crates/zoe-client/tests/shared_cache.rs, python/zoedb/src/zoe/zeo/** (the key `shared-cache`), python/zoedb/tests/test_shared_cache.py, docs/spec/technical-specification.md (the row F38 and the subsection of the client cache)
Out of scope: the cache in memory (ZOE-156) and the private file (ZOE-162), which this card extends; `blob_url` and blob dedup (the rest of F38); an extension that names the store at `register` (a wire-protocol change: stop and ask)
Invariants: the semantics of `ZEO.cache.ClientCache` are the reference; a stale entry is never served, also after a crash of any process; a hit takes no lock and writes nothing; `applied` moves only after the ends of its transaction; a current entry from a load goes in only when `applied` is not above the stream of the process; each process keeps its own `lastTransaction`; a drop never truncates a file that another process maps; the file is never the only copy of anything
Spike or build: build
Budget: one session; a spent budget means stop and ask, not try harder
Done when: acceptance green; the kill loop runs in the pull-request tier; the hit cost of a shared file within 10 % of a private file in `zoe-bench`; reviewer approved
```

It depends on ZOE-162, so it is Ready only after ZOE-162 merges.
