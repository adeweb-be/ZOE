# Spike: read_hot at 100 KB, and what the client can still win

Card ZOE-187 (#385), requirements N4 and F28. The prototypes are on the branch `poc/ZOE-187-read-hot`, not for merge: the scripts under `bench/poc/ZOE-187/`, their raw results under `bench/poc/ZOE-187/results/`, and a memo of hits in `crates/zoe-py/src/hit_memo.rs`.

**Verdict.**

- The native hit is not broken. In the processes cells every timed read is a hit of the native cache. The server counts 3,329 loads per value, and that is the populate and the priming.
- 3× ZEO is not reachable through the client in `read_hot` at 100 KB at threads 1 or processes 16. ZODB alone, through a diagnostic client that copies nothing, takes 17 µs per object at threads 1, and the target is 10.5 µs. At processes 16 it takes 82 to 92 µs, and the target is 42 µs. A client that costs nothing gives at most 1.85× and 1.4 to 1.5× ZEO on this machine.
- The threads cell at 16 does not measure hits. Its timed reads all miss: the one shared cache of 20 MB is smaller than the 160 MB of the 16 workers. With a cache of 256 MB for both clients, native is 7.5× ZEO in that cell.
- The largest client term of a 100 KB hit is the xxh3 of the data (about 4.6 of 10 µs). Then come the atomic loads of the words (2.6 µs, the cost of a memcpy) and the write into the `bytes` (1.7 µs). The lookup costs 16 ns.
- The prototype (a bounded memo of the checked `bytes` of recent hits, keyed by the logical position of the entry) gives 1.56× at threads 1 (25.7 → 16.8 µs, the ZODB floor) and 1.32× at processes 16. It changes no decision of the cache. It costs up to its bound in memory (+10 MiB per worker here). It loses 11 to 13 % when the memo is smaller than the working set.
- Recommendation: revise, and do not merge as is. The gain is real and bounded, but it does not change the verdict of the gate. A memo is a new feature: a second cache with its own memory bound. It needs a decision of the human and an ADR, and the thrash case needs a guard.

## Machine, build and method

- Intel Core Ultra 7 155H laptop: 6 P-cores with SMT (CPUs 0 to 11) and 10 E-cores (CPUs 12 to 21), L3 24 MiB, 31 GB of RAM, turbo on, `intel_pstate` with `powersave` and the `balanced` profile. Disk: a 1 TB NVMe SSD, ext4 on LVM, 98 % full. Linux 7.0.0-38. Not the reference machine. A single value in a processes cell spreads up to ±50 % here.
- CPython 3.14.7 free-threaded (`Py_GIL_DISABLED=1`). The GIL is off in every worker (the GIL record of the harness). ZEO 6.2, ZODB 6.3, zodbshootout 0.8.0, persistent 6.8+zoe.1, zodbpickle 4.5+zoe.1. ZEO on CPython 3.13.15 for the build label. rustc 1.99.0. `zoe serve` and `zoedb` in release.
- Base: `main` at 0f03dfb (#384), which holds every fix up to ZOE-172 and ZOE-186. The prototype is on top of it. With `ZOE_POC_HIT_MEMO` unset, the prototype runs the same hit path as `main`, plus one `Option` check.
- `perf` is not available: `kernel.perf_event_paranoid` is 4, and the session has no root. The traces use `getrusage(RUSAGE_THREAD)` (CPU time, context switches, page faults), `sched_getcpu` and `scaling_cur_freq` instead.
- `bench/poc/ZOE-187/drive.py` runs each value as `_run_value` of the harness does: a fresh store and server, then zodbshootout with the options of the harness (`--processes 1 --values 1 --warmups 1 --loops 1 --object-counts 100`, `--threads shared` in the threads mode). It changes nothing of the harness. It only adds a `PYTHONPATH` entry, `startup/sitecustomize.py`, and the variables that pyperf passes on. It counts `zoe_loads_total` over each value of `zoe-native`.
- The timing runs have no hooks. The profiling runs (`--trace`, `--barrier`, `--terms`) are separate. `--trace` replaces `bench_hot_read` with a copy that records each loop outside the clock. `--barrier` makes the workers start their timed loops together. `--diag same|copy` serves `loadBefore` from a dict of the process, after the prime: the same bytes and serials that the real `loadBefore` gave, for a snapshot after their start. The run checks each record against the real `loadBefore` at the end, and checks that no commit came. `same` gives the same `bytes` object, and `copy` gives a new copy.
- The A/B runs interleave per cell: each round runs zeo, native without the memo and native with it, in an order that rotates. A first A/B that ran all cells of one variant in a row (`results/ab-biased.jsonl`) gave a false 0.83× at threads 1 and 1 KB. In it, the cell with the memo always came after the heaviest cell of the run before. The tables below use the interleaved runs.
- Time per object: the time of the slowest worker for 100 objects, divided by 100. This is what zodbshootout and the harness count (`ops_per_second` = concurrency × 100 ÷ mean).

## 1. The gap

**Uninstrumented, 5 values per storage, alternating** (`results/base-main.jsonl`; µs per object, median; the spread is (max − min) ÷ median):

| cell | ZEO | native | native / ZEO | spread ZEO, native | server loads per value |
| --- | --- | --- | --- | --- | --- |
| read_hot threads 1, 100 KB | 31.5 | 24.5 | 1.28 | 9 %, 13 % | 209 |
| read_hot threads 16, 100 KB | 2,592 | 808 | 3.21 | 16 %, 47 % | 102,724 |
| read_hot processes 16, 100 KB | 125.4 | 104.2 | 1.20 | 58 %, 47 % | 3,329 |
| read_hot threads 1, 1 KB | 10.2 | 8.1 | 1.25 | 19 %, 22 % | 209 |
| read_hot processes 16, 1 KB | 27.8 | 22.9 | 1.22 | 72 %, 53 % | 3,329 |

- Paired by round, native beats ZEO in each of the 5 rounds at processes 16 and 100 KB (1.03 to 1.34). The 0.88× of #382 on AWS (3 values) is inside the spread of this cell. It is not a defect of the hit.
- **Hits are hits.** Threads 1: 209 loads per value, which is the populate, the prime, and nothing timed. Processes 16: 3,329 loads per value, the same at every value, with 16 × 30 × 100 timed reads per call. In traced runs of the prototype build with the memo off (32 workers at processes 16, 2 at threads 1), no counter of the native cache moves during the timed loops: `stored`, `refused`, `copied` (the copies to the head of the ring), `unindexed`, `head`, `applied` and the last tid. So there is no miss, no eviction, no refresh and no invalidation, and the 10 MB of a worker sit in a ring of 20 MB. On ZEO's side, `adds` does not move, and `accesses` moves by one per read and by a few per loop for the mappings.
- **Threads 16 at 100 KB measures misses.** 102,724 loads per value: every timed read reaches the server, for both clients. The 16 threads share one client with a cache of 20 MB, and their working set is 16 × 100 × 100 KB. The 3.21× (2.85× on AWS) compares the miss paths.
- **Steady state against start-up.** In processes mode zodbshootout has no barrier before the timed loops (`ForkedSpeedTestWorker.sync_before_timing_loop` does nothing). Each worker sums 30 loops, and the sample is the slowest worker. In the traces, the median loop overlaps the open or the prime of no other worker. During a timed loop, a worker has 0 context switches and 0 page faults, and its CPU time is 0.96 to 0.97 of its wall time. So nothing waits during the reads: not the tokio threads of the client (they are parked), not the network, not the page faults of a new mapping.

**The cores decide the processes cell** (`results/cores-p16.txt`; profiling run, µs per object of the loops by the CPU that ran them):

| run | P-core loops | E-core loops | slowest worker |
| --- | --- | --- | --- |
| ZEO, no barrier | 59.9 | 148.4 | 130.5 |
| native, no barrier | 58.2 | 125.0 | 111.0 |
| ZEO, barrier | 79.6 | 178.4 | 146.4 |
| native, barrier | 74.8 | 155.7 | 138.6 |

On a P-core, the two clients are almost equal at 16 processes. The sample is the slowest worker, which is a worker on an E-core. When the workers start their loops together (barrier), each loop gets slower. So the loops share a resource of the machine.

**Concurrency against the ZODB floor** (`results/sweep-cores.txt`; barrier, P-core loops, µs per object; "floor" is `--diag same` on the native client):

| processes | ZEO | native | floor | native − floor |
| --- | --- | --- | --- | --- |
| 1 | 30.4 | 25.2 | 14.8 | 10.4 |
| 2 | 37.2 | 28.6 | 19.5 | 9.1 |
| 4 | 46.6 | 37.3 | 28.3 | 9.0 |
| 6 | 51.5 | 43.7 | 32.4 | 11.3 |
| 8 | 54.7 | 47.9 | 34.1 | 13.8 |
| 12 | 56.2 | 51.5 | 39.2 | 12.3 |
| 16 | 65.8 | 62.5 | 47.3 | 15.2 |

- ZODB alone gets 3.2× slower per object from 1 to 16 processes, and already 1.75× slower at 4 processes, where each worker can have a physical P-core of its own. So it is not SMT alone, and not the client.
- The frequency of the CPU of each loop stays at 4.0 to 4.4 GHz on the P-cores at 1, 4 and 16 processes, and at 2.8 GHz on the E-cores (`results/mhz.txt`). So it is not a loss of turbo either.
- What is left is the memory path that the workers share: L3 and DRAM. A hit of 100 KB moves about 400 KB: the client reads 100 KB and writes 100 KB, and the unpickle of ZODB reads 100 KB and writes 100 KB. 16 workers hold 160 MB of hot data, against 24 MiB of L3.
- At 16 processes, on the E-cores, native (144.4 µs) is already at the floor (143.3 µs).
- The client's share (native − floor) stays at 9 to 15 µs per object at every concurrency.

**Terms of a hit** (`results/terms.txt` and `results/poc_cost.txt`). The Python terms are tight loops over the 100 objects in each worker, with a barrier, so they are indicative only (µs per object):

| term | 1 process, 100 KB | 16 processes, 100 KB | 1 process, 1 KB |
| --- | --- | --- | --- |
| native hit, `_native.load_before` | 10.1 | 29.0 | 0.52 |
| `ClientStorage.loadBefore` of `zoe.zeo` (Python wrapper on top) | 10.9 | 29.0 | 0.86 |
| ZEO hit, `_cache.loadBefore` / `ClientStorage.loadBefore` | 12.1 / 13.4 | 45.8 / 41.1 | 2.67 / 2.76 |
| ZODB unpickle, `ObjectReader.getState` | 8.6 to 9.4 | 16.1 to 18.6 | 3.6 to 4.0 |
| alloc and memcpy of the `bytes`, for scale | 3.2 to 3.5 | 7.6 to 8.1 | 0.24 |

Rust, one process, 100 entries in a cache of 20 MB (ns per entry, median of 200 passes):

| term | 100 KB | 16 KB | 1 KB |
| --- | --- | --- | --- |
| lookup: slot, chain, head and checks | 16 | 15 | 32 |
| atomic loads of the words only | 2,612 | 402 | 41 |
| xxh3, streamed in 4 KiB blocks | 4,565 | 715 | 41 |
| copy and hash, no destination | 6,927 | 894 | 271 |
| copy, hash and write into a `Vec` | 8,770 | 1,206 | 112 |
| memcpy, for scale | 2,511 | 190 | 11 |

- No intermediate `Vec` exists on the hit path: `PyBytes::new_with_writer` takes the blocks.
- The atomic loads cost what a memcpy costs. A plain memcpy of memory that writers change would not be faster, and it would be wrong.
- The hash is the largest term. xxh3 here runs at about 22 GB/s, without AVX2: the wheel targets the x86-64 baseline. A path with AVX2, chosen at run time, would give the same hashes, so the format does not change. That is a hypothesis for another card, not measured here.
- The binding adds about 1.3 µs over the Rust term (the `bytes`, the tuple, the serials). The Python wrapper adds 0.3 to 0.8 µs.

## 2. The budget of each failing cell

Target = ZEO ÷ 3, from the ZEO of the same run. The ZODB floor is `--diag same` (`results/diag.jsonl`, 5 values; the range is the floor on the ZEO client and on the native client). Client share = native − floor. µs per object, this machine:

| cell | ZEO | target | native now | ZODB floor | client share | improvement needed | best with a free client |
| --- | --- | --- | --- | --- | --- | --- | --- |
| read_hot threads 1, 100 KB | 31.5 | 10.5 | 24.5 | 17.0 to 17.1 | 7.5 (31 %) | 2.33× | 1.85× ZEO |
| read_hot processes 16, 100 KB | 125.4 | 41.8 | 104.2 | 82.1 to 91.6 | 12.6 to 22.1 (12 to 21 %) | 2.49× | 1.37 to 1.53× ZEO |
| read_hot threads 16, 100 KB (misses) | 2,592 | 864 | 808 | 131 to 194 with hits | the miss path | met here (3.21×); 1.054× on AWS | 7.5× ZEO with a cache of 256 MB |

- One copy from the heap (`--diag copy`) gives 18.4 to 18.8 µs at threads 1 and 108 to 110 µs at processes 16. At 16 processes, a plain copy of the record costs as much as the whole native hit.
- **The AWS cells of #382 (`aws-c6id-2xlarge`, 3.14t):**
  - Threads 1: ZEO 46.7 µs, native 34.0 µs, target 15.6 µs.
  - Processes 16: ZEO 127.9 µs, native 144.7 µs, target 42.6 µs.
  - The floor was not measured there. The floor of the budget spike (DemoStorage over FileStorage, 44 µs at threads 1) is not a bound: native is below it.
  - If the shares of this machine hold, the best with a free client is about 1.85× at threads 1 and 1.4 to 1.5× at processes 16.
- **Why the client cannot reach 3×.** The unchanged ZODB part alone is above the target in both hit cells. It is the unpickle, which builds a new `bytes` of 100 KB for the attribute `data` (a copy that the pickle format forces), then the activation of the object and the loop of zodbshootout. A 3× needs a change below the client: a load that hands ZODB a state without a copy of the data (pickle protocol 5 with buffers out of band, a `memoryview` attribute). That changes what an application object holds, so it is a change of ZODB and of the object model, not of ZOE. Or it needs a change of the gate, which is the human's decision. This spike changes neither.
- **The gate and the build labels.** `gate.py` compares ZEO and zoe-native of the same Python build (`--python cp314t`). The posted tables of #313 and #382 divide by ZEO's best build of 3.14t and 3.13. For the three `read_hot` cells at 100 KB, ZEO is faster on 3.14t, so both comparisons give the same verdict:
  - on AWS at 91921cd: 125,137 against 62,111 ops/s, 21,422 against 15,548, and 4,385 against 3,999;
  - here: 125.4 against 204.6 µs at processes 16, 31.5 against 49.7 at threads 1, and 2,592 against 3,283 at threads 16 (`results/zeo313.jsonl`).
  - The two comparisons differ in most of the write, `read_cold` and `read_warm` cells, where ZEO on 3.13 is faster. There, the posted ratios are stricter than `gate.py`.

## 3. The prototype: a memo of the bytes of hits

**What it does.**

- `Cache::load_before_at_with` and `load_at_with` give the closure of a hit the logical position of the entry. `load_before_with` and `load_with` call them and do not change.
- `zoe-py` keeps, per client, a map from oid to (position, `bytes`), in 16 shards with a mutex each, and one byte budget for all of them.
- On a hit, the cache runs its lookup as before: `applied`, the skip rule, the slot, the chain, the head checksum, the intact check. If the memo holds a copy of that oid at that position, the hit returns that `bytes` object. It copies nothing and hashes nothing. Else the hit copies and checks as before, and the memo keeps the copy.
- The bound is strict: a copy reserves its bytes with a compare-exchange before it goes in. A copy that finds no room drops the oldest copies of its shard, or is refused. Dropped objects are freed after the lock.
- `cache_clear` and `close` empty the memo.
- The prototype reads `ZOE_POC_HIT_MEMO=BOUND[:MIN_LEN]` at the open of a client. Unset, no memo.

**Why it cannot serve stale data.**

- `head` only moves forward: `append` writes at or past `head`, and `clear` jumps two rings. Logical positions never repeat in a `Cache`.
- An entry's bytes are written once, after the writer moved `head` past them. A writer that overwrites them moves `head` past `pos + ring` first.
- A lookup that finds the entry at `pos` intact has seen `head ≤ pos + ring`. So no overwrite of `pos` has started, and the bytes at `pos` are the bytes that an earlier checked copy of `pos` read.
- The memo changes no decision of the cache. It serves only the entry that the lookup chose. The start and end tids come from the head of that entry, not from the memo. So the snapshot intervals, `end` in place, the ordering of invalidations and of `applied`, the skip after a lost listener, `verify`, `clear`, `disable` and `close` keep their meaning.
- The file layout, the checksums and the rule for stores do not change.
- A memo belongs to one `NativeClient`, so to one `Cache`, which a reconnect keeps.

**Correctness tests.**

| test | result |
| --- | --- |
| `poc_positions_name_one_entry` (Rust): the same entry keeps its position; a new revision, a drop, a clear and a wrap of the ring give another position or a miss | pass |
| `poc_memo_readers_never_get_a_stale_or_torn_hit` (Rust): `readers_never_get_a_stale_or_torn_hit`, with 4 readers on one shared memo, while a writer commits, stores, wraps the ring and clears; every hit checked against the history | pass, 24,067 hits, 19,300 from the memo |
| both Rust tests under `just tsan zoe-client poc_` | no report; the canary race reported |
| `python/zoedb/tests/client/test_poc_hit_memo.py`: one object only with the memo, new data after a commit of another client and old data at the old snapshot, the bound, `clear`, a wrapping ring, `MIN_LEN` | 7 pass |
| `python/zoedb/tests/client/` with the memo on for every size (`ZOE_POC_HIT_MEMO=67108864:0`); among them 16 threads that load while another client commits, and the reconnect and verification tests | 130 pass; 51 clients in the cache, concurrent, persistent-cache and reconnect modules served 12,617 memo hits |
| `test_zeo_suites_native.py` (ZEO's client tests, the phase 3 acceptance test) and the native race, storage, connection and conflict suites, memo on (`just conformance 3.14t`) | 10 pass; 9,046 clients, 59,465 memo hits |
| `cargo test -p zoe-client`, `just check` | pass |

Not tested:

- ThreadSanitizer on the memo of `zoe-py` itself. It is a cdylib inside CPython, and the recipe covers only the Rust model of it.
- CPython 3.12, 3.13 and 3.14 with the GIL, and the 3.13 build of the extension.
- A shared cache file (stage 3, not built).
- The cloud machine.

**A/B, interleaved, 6 rounds, memo of 20 MiB** (`results/ab.jsonl`; on ÷ off paired per round, then the ratio to the ZEO of the same rounds):

| cell | on ÷ off, median (min to max) | native ÷ ZEO, off → on | µs per object, off → on |
| --- | --- | --- | --- |
| read_hot threads 1, 100 KB | 1.56 (1.37 to 1.64) | 1.18 → 1.80 | 25.7 → 16.8 |
| read_hot processes 16, 100 KB | 1.32 (1.11 to 1.44) | 1.18 → 1.61 | 151.4 → 110.5 |
| read_hot threads 16, 100 KB | 1.02 (0.97 to 2.01) | 2.05 → 2.78 | misses; one round of 6 went to 2.01 |
| read_hot threads 1, 1 KB | 1.05 (0.96 to 1.09) | 1.29 → 1.32 | 8.3 → 8.2 |
| read_hot processes 16, 1 KB | 0.92 (0.84 to 1.08) | 1.26 → 1.22 | inside the spread of the cell |
| read_hot threads 16, 1 KB | 1.04 (0.74 to 1.17) | | |

The machine ran slower in this session than in section 1: ZEO took 178 µs at processes 16. So the ratios matter, not the absolute numbers. At threads 1 the memo brings the native client to the floor of `--diag same`.

**Regressions, interleaved, 4 rounds** (`results/ab2.jsonl`, on ÷ off):

| scenario | threads cell | processes 16 |
| --- | --- | --- |
| `read_cold` at 100 KB, the population of an empty cache | 1.01 (0.94 to 1.05), threads 1 | 1.04 (0.99 to 1.06) |
| `read_warm` at 100 KB, commits with invalidations | 0.98 (0.90 to 1.09), threads 16 | 1.01 (0.89 to 1.07) |
| read_hot, ring of 6 MB under 10 MB of hot data | 1.00 (0.96 to 1.02), threads 1 | 0.99 (0.95 to 1.08) |
| **read_hot, memo of 4 MiB under 10 MB of hot data** | **0.89 (0.82 to 1.00)**, threads 1 | **0.87 (0.79 to 1.11)** |
| read_hot threads 16, cache of 256 MB for both, memo of 256 MiB | 1.03 (0.98 to 1.05); native 7.5× ZEO without the memo, 7.9× with it | |

- **Memory.** At processes 16 and 100 KB, the peak RSS of a worker goes from 65.2 to 75.4 MiB: the 100 hot entries, held a second time. The memo doubles the memory of hot entries up to its bound. A `bytes` that a reader still holds after a drop stays alive with the reader and is outside the bound. In ZODB, the unpickle releases it at once.
- **The thrash case.** When the memo is smaller than the hot set that the cache holds, each hit pays the copy, the put and a drop, and gains nothing.

## 4. Demonstrated, and still hypotheses

Demonstrated on this machine:

- The timed reads of the processes cells are hits.
- The threads cell at 16 measures misses.
- The share of ZODB is above the 3× target in the hit cells.
- At 16 processes, the slowest worker on an E-core and the shared memory path set the sample, for every client and for the floor.
- At 100 KB the xxh3 is the largest term of a native hit, and the copy-free hit reaches the floor at threads 1.

Hypotheses, not measured:

- That the AWS machine has the same shares. Its floor was not measured.
- That an xxh3 with AVX2, chosen at run time, takes about half of the hash term.
- That a guard against the thrash (keep a copy only after a second hit of the same position, or stop the puts while the memo serves few hits) removes the regression of the small memo.
- The cost of DRAM traffic per hit, without `perf`.

Side findings:

- zodbshootout catches an exception of a benchmark function, logs it, and reports a bogus time (6.66 s in a smoke run here). The harness then counts it as a value. `drive.py` fails a value whose log holds `ERROR`. The harness could do the same.
- An A/B that runs one variant after the other in a fixed order of cells gives false results here (0.83× at threads 1, 1 KB). Compare variants interleaved per cell.

## Recommendation

Revise the prototype. Do not merge it as is.

- **Keep the finding.** No client change brings `read_hot` at 100 KB to 3× ZEO at threads 1 or processes 16. The human decides whether the gate counts these cells against a floor, as decision 1 of `docs/spikes/phase3-budget.md` did for 1 KB, or leaves them out. This spike does not change the gate.
- **The threads cell at 16 and 100 KB is a cache-size question, not a hit question.** The harness runs both clients with 20 MB. A cache that holds the 16 workers gives 7.5× ZEO. A larger default `cache-size` of the native client alone would change the scenario rather than the client. That is for the human.
- **If the human wants the memo**, as its own card with an ADR:
  - a key of `<zoeclient>` instead of the variable;
  - a default bound tied to `cache-size`;
  - a `MIN_LEN` of about 16 KiB (at 1 KB it gains 0.3 µs per hit, and the doubled memory buys nothing);
  - a guard against the thrash;
  - TSan or a stress test of the PyO3 side under 3.14t.
  - It brings threads 1 to about 1.8× ZEO and processes 16 to about 1.4 to 1.6× ZEO here. That moves the cells, but no cell passes.
- **A cheaper next step with no new feature:** an xxh3 with AVX2 chosen at run time, behind the same checksums. It would also speed up the stores. Measure it first.
