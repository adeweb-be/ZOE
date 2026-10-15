# Benchmarks

This directory holds the reference baseline: a harness that runs [zodbshootout](https://pypi.org/project/zodbshootout/) against FileStorage, ZEO 6 and RelStorage on PostgreSQL, with history (`relstorage`) and without (`relstorage-hf`), and the JSON schema of its results. The same harness measures ZOE in-process, the storage `zoe`, through the `<zoe>` stanza, and ZOE server twice: the storage `zoe-server`, a stock ZEO client of `zoe serve` through the `<zeoclient>` section, and the storage `zoe-native`, the native client of `zoe serve` through the `<zoeclient>` section with `native true`. The phase 3 gate does not run zodbshootout: it has a driver of its own, the native gate (see [The gate](#the-gate)). `zoe-bench` (contention and server saturation) and the criterion microbenchmarks live in `crates/`.

## Run it

`just bench` is the entry point. Run it inside `devenv shell`.

| Command | What it does |
| --- | --- |
| `just bench` | the full set against every storage; writes to `target/bench/results` |
| `just bench run --quick` | the quick set, the regression gate of a pull request |
| `just bench run --storage zeo --quick` | one storage only |
| `just bench native` | the phase 3 gate (ADR 0039): `zoe-native` against `zeo` in the 30 cells of the gate set, in about a minute; writes `target/bench/native/<started>.json` and prints the report (see [The gate](#the-gate)) |
| `just bench native --cell threads:1:1024 --scenario read_warm` | a part of the phase 3 gate |
| `just bench native report FILE` | the rows of a result of the phase 3 gate and its verdict |
| `just bench test` | the tests of the harness (the acceptance test of the baseline) |
| `just bench test FILE...` | the tests of those files only |
| `just bench client` | the client benchmark: `ZEO.ClientStorage` against the native client (see [The client benchmark](#the-client-benchmark)) |
| `just bench client report FILE` | the rows of a result of the client benchmark and its verdict |
| `just bench index DIR` | rewrites `index.json` of a results tree |
| `just bench gate DIR` | the parity gate of phase 2: `zoe-server` against `zeo` in each cell of a results tree but `read_hot` (see [The gate](#the-gate)) |
| `just bench plone COMMAND` | the Plone end-to-end benchmark: Locust against a Plone site on ZEO and on ZOE (see [The Plone end-to-end benchmark](#the-plone-end-to-end-benchmark)) |

The bench project takes `python/zoedb` as an editable path source, as the uv workspace of the Python tests does. So the harness runs the Python code of the tree. uv builds the extension with maturin in the release profile, and the cache keys in `python/zoedb/pyproject.toml` tell uv to build it again when the Rust code changes.

RelStorage runs when `ZOE_BENCH_PG_DSN` (or `--dsn`) names a PostgreSQL database that the harness may empty. `relstorage` and `relstorage-hf` use the same database. The harness drops every table, sequence, view and function of RelStorage there, so never give it a database that something else uses. Without it, the harness skips both and says so. The Dagger function `bench` starts PostgreSQL 17 as a service and runs the harness tests, then the baseline, and returns the results tree:

```sh
dagger call --mod ci bench --quick export --path target/bench/ci-results
```

Other settings:

- `ZOE_BENCH_MACHINE` or `--machine`: the machine name in the results. The default is the CPU model, for example `intel-core-ultra-7-155h`.
- `ZOE_BENCH_COMMIT`: the commit to record. The default is `git rev-parse HEAD`.
- `UV_PYTHON`: the interpreter, `3.13` by default. Each interpreter gets its own environment under `target/bench/`.
- `--workdir`: where the storages write their files, `target/bench/work` by default. Keep it on the disk under test, not on a tmpfs. Its disk needs the free space of one value (see [The disk](#the-disk)).
- `ZOE_BINARY`: the `zoe` binary of the storages `zoe-server` and `zoe-native`. Without it, the harness takes `target/release/zoe` of the repository (under `CARGO_TARGET_DIR` when it is set), and cargo builds it when it is missing. `just bench run` and `just bench test` build it in release first, so it is never stale.

## What it measures

zodbshootout runs five scenarios: `add`, `update`, `read_cold`, `read_warm` (zodbshootout's write/read: write the objects, then read them back in the next transaction; the time includes both) and `read_hot`. Each runs in a set of cells. A cell is a mode, a concurrency and an object size. Every transaction holds 100 objects.

| Set | Cells | Values per scenario and cell |
| --- | --- | --- |
| quick | threads, concurrency 1 and 16, objects of 1 KB | 5 |
| full | threads at concurrency 1, 4, 16 and 64; processes at 4, 16 and 64; objects of 1 KB and 100 KB | 20 |

- Each value runs in its own zodbshootout run, on a fresh store: one scenario, one pyperf worker process, one warmup, then the value (`--processes 1 --warmups 1 --values 1 --loops 1`). The warmup runs in the same process and on the same store as the value, so the costs of a first call stay out of the value. The other options are the same for every value.
- Only `add` gets `--zap force`: zodbshootout zaps the store before each call of `add`, and it refuses a zap without `add`. For the other scenarios, zodbshootout populates the fresh store before the calls: the tree with 100 objects per worker and the shared mapping of conflicts.
- For each value, the harness makes a new directory under the work directory, starts the ZEO server or `zoe serve` on a new store in it, runs zodbshootout, stops the server and removes the directory: the store, the logs and the pyperf output. For `relstorage` and `relstorage-hf`, it empties the database with RelStorage's `zap_all` before and after each value. RelStorage refuses to open a database that has the schema of the other history mode, so before the first value of each of the two storages, the harness drops the tables, the sequences, the views and the functions of RelStorage in both modes. A value that fails keeps its directory and its logs, and the run stops.
- In the threads mode, one process runs the workers as threads that share one `DB`, as in one Zope process. In the processes mode, each worker is a process with its own `DB`, as in several Zope processes.
- FileStorage and `zoe` run only in the threads mode: only one process can open a FileStorage file or a ZOE store. ZEO, both RelStorage storages, `zoe-server` and `zoe-native` run in every cell.
- A sample is the time for the slowest worker to do one transaction. A result reports the mean, the median and the 99th percentile of the values of a scenario and cell. `ops_per_second` is objects per second for all workers together: concurrency × 100 ÷ mean.
- Every storage runs with its default settings. The `<zoe>` stanza sets only `path`. `relstorage` keeps history, like FileStorage and ZEO. `relstorage-hf` is the same `<relstorage>` section with `keep-history false` and nothing else: many sites run RelStorage without history, and it is the faster mode on writes. The ZEO server is a separate process on the same machine.
- `zoe-server` runs `zoe serve --path <value>/store --listen 127.0.0.1:0` in the directory of each value, a separate process on the same machine, as the ZEO server. No `ZOE_*` variable of the environment reaches it, so it runs with its default settings. The client is the `<zeoclient>` section of ZEO with its default settings, as for `zeo`. The result records the version of the server as `zoe` in `software`.
- `zoe-native` runs `zoe serve` as `zoe-server` does, with `--admin-listen 127.0.0.1:0` in addition. The client is the `<zoeclient>` section of `zoe.zeo.ClientStorage`, after `%import zoe`, with `server` and `native true` and no other key: the native inside, with its default settings. The result records the version of the server as `zoe` and the version of `zoedb` in `software`.
- zodbshootout logs the exception of a benchmark and reports a time for it all the same. So when the log of a zodbshootout run holds a record of level `ERROR` or `CRITICAL`, the value fails with that record, and the run stops.
- zodbshootout empties the cache of the client with `_cache.clear()` before each cold read. A cache that this call does not empty would turn `read_cold` into a cache read. So for each `read_cold` value of `zoe-native`, the harness reads `zoe_loads_total` on the admin port before and after the zodbshootout run, and compares the difference with the objects that the run read (the warmup and the value). When the server counted fewer loads than objects read, the run fails with a message that names `read_cold` and the cache.
- Each scenario entry records the GIL of its worker processes, in the list `workers`: one item per Python process of its zodbshootout runs that imported ZODB (the zodbshootout process, its pyperf worker and, in the processes mode, the process of each worker). `gil_enabled` is `sys._is_gil_enabled()` when the process ended, and `gil_enabled_by` lists the modules that turned the GIL back on, from the warnings of CPython. On 3.14t a number means little without it: RelStorage's driver `psycopg2._psycopg` turns the GIL back on, for example. The record comes from `shootout/startup/sitecustomize.py`, which the harness puts first on `PYTHONPATH` of each zodbshootout run, and which pyperf passes to its worker with `--inherit-environ`. It changes no setting of the run: it shows each warning as Python does, removes its directory from `sys.path`, and imports the `sitecustomize` module that it hides, if there is one.

The corpus is synthetic: zodbshootout generates its own objects. Results are numbers, never data.

## The disk

The work directory holds one value at a time. The largest value of the full set, `update` at concurrency 64 with objects of 100 KB, takes about 40 GB on `filestorage` and `zeo`, and about 26 GB on `zoe` and `zoe-server`. A value of the quick set takes at most 0.3 GB.

Before each cell, the harness compares `disk_needed(storage, cell)` with the free space of the disk of the work directory. When the free space is smaller, the cell fails before it starts. The message names the storage, the cell, the bytes needed and the bytes free. The cells before it left nothing, and the harness writes no result for that storage.

`disk_needed` is an upper bound of the bytes of one value: the populate and two calls of `update` (the warmup and the value), at the object size and 8 KiB per record, and 64 MiB for an empty store, the index files and the logs. For `relstorage` and `relstorage-hf` it is 64 MiB, because the data goes to PostgreSQL, whose disk the harness does not see.

| Objects | c=1 | c=4 | c=16 | c=64 |
| --- | --- | --- | --- | --- |
| 1 KB | 0.26 GB | 0.82 GB | 0.98 GB | 3.7 GB |
| 100 KB | 2.3 GB | 9.1 GB | 11.0 GB | 43.7 GB |

So the full set needs about 44 GB of free space, and the quick set about 1 GB.

The populate of a value is one transaction: about 0.67 GB at concurrency 64 with objects of 100 KB. Through `ZEO.ClientStorage`, for `zeo`, `zoe-server` and `zoe-native`, the transaction buffer of the client holds a copy of it in a temporary file under `TMPDIR`, and for `zeo` the commit log of the ZEO server holds a second copy there. These files are outside the work directory and outside the check, so `TMPDIR` needs room for them.

## Results

Each run of one storage gives one JSON document that validates against [`schema/results.schema.json`](schema/results.schema.json). The document names the storage, the machine (CPU, logical CPUs, memory, kernel, the disk that holds the storage files), the Python build (version, free-threaded or not), the versions of every package and server, the commit, and one entry per scenario and cell, with the GIL record of its workers. It holds no host name, user name or path.

## The gate

N4 sets two gates against ZEO 6 through `ZEO.ClientStorage`.

### Phase 2: parity

The phase 2 gate is parity: `zoe-server` at least 1× `zeo` in each zodbshootout cell that reaches the server. `python -m shootout gate`, or `just bench gate DIR`, reads the results tree DIR in the layout below and turns the gate into a pass or a fail per cell:

```sh
just bench gate target/bench/results
```

- For each machine, Python build and set (quick or full), it keeps the last result of each storage, by the start of the run. So it compares one machine, one Python build and the same cells, and never a quick run with a full run.
- For each scenario and cell of the baseline, it prints `ops_per_second` of both storages and their ratio.
- It checks parity of `ops_per_second` by default and leaves `read_hot` out: zodbshootout's `read_hot` empties the object cache of the connection, then reads the objects from the cache of `ZEO.ClientStorage`, so no read reaches the server. The report prints each cell that it leaves out with the numbers that exist and the verdict `left out`, and says why in a `left out:` line, such as `left out: read_hot at 1 KB, because ...`. Such a cell counts neither as below nor as missing. The summary line names the cells that it leaves out: `each cell but read_hot`. `read_hot` still runs and stays in the results.
- It exits with 0 when the ratio of each cell that counts is at least the ratio of the gate. It exits with 1 when a ratio is below, when a cell of the baseline has no counterpart in the storage, or when the tree holds no result of the baseline.
- `--storage zoe-server --baseline zeo` are the defaults. `--ratio` takes any positive value in place of parity.
- `--python cp314t` counts only the results of that build. The report prints the groups of the other builds after them, marked `does not count`, with their verdicts in lower case. Without `--python`, every build counts.
- The phase 2 gate counts only the full set of the reference machine. The RelStorage row of N4 counts `relstorage` and `relstorage-hf`.

### Phase 3: the native gate

The phase 3 gate (ADR 0039) asks `zoe-native`, `zoe serve` through the native client, to be faster than `zeo` in every cell of the gate set, and it runs in about a minute. It does not run zodbshootout: `shootout/native.py` drives both storages, and `shootout/nativeworker.py` runs the workers.

```sh
just bench native
just bench native --cell processes:16:102400 --scenario add update
just bench native report target/bench/native/<started>.json
```

- **Cells.** The 5 scenarios of zodbshootout (`add`, `update`, `read_cold`, `read_warm`, `read_hot`) in 3 shapes (threads 1, threads 16, processes 16), at 1 KB and 100 KB: 30 cells. A transaction holds 100 objects at 1 KB and 10 objects at 100 KB.
- **The same work as zodbshootout.** The workers use zodbshootout's `SpeedTestData`: its `PObject`, its data and its mapping per worker. `add` puts new objects in the mapping and commits. `update` changes each object and commits. `read_cold` opens a connection with no cache and reads each object. `read_warm` opens a connection with no cache, changes each object, commits, reads each object and commits. `read_hot` empties the cache of a primed connection and reads each object from the cache of the storage. Before a batch of `read_cold` or `read_warm`, each process empties the cache of its connections and of its storage, and that work is not timed.
- **Start once.** For each cell, each storage gets one server on a fresh store (a ZEO 6 server on a FileStorage, and `zoe serve` with its default settings, which store records raw, as FileStorage does), one populate and one set of worker processes, started with `spawn`. In the threads mode, one process runs the workers as threads on one shared `DB`. In the processes mode, each worker is a process with its own `DB`. Both servers run for the whole cell. The cell removes its stores when it ends.
- **Pairs.** A batch runs the same number of transactions in a row in each worker of one storage, as the inner loops of zodbshootout do: the warmup pair sets it, so that a batch lasts about 20 ms, with at most 20 transactions. Its time is the time of the slowest worker for one transaction. A pair is one batch of `zeo` and one of `zoe-native`, and the storage that goes first changes from one pair to the next. Only one storage works at a time. One pair before the first one warms the scenario up and does not count.
- **The rule.** The ratio of a pair is `t_zeo / t_zoe`. After at least 10 pairs, a scenario passes when the 99 % confidence interval (Student's t) of the mean of the log of the ratios is above 0, and fails when it is below 0. It stops at its first verdict, at 100 pairs, or after 10 s of pairs. A scenario without a verdict is `UNDECIDED`, and it fails the gate: a tie is not faster.
- **Checks.** The run of `read_cold` of `zoe-native` fails when `zoe serve` counted fewer loads (`zoe_loads_total` on its admin port) than the objects that the batches read. Each worker checks that it read every object, and that a cold read loaded them.
- **The report.** One row per cell that the result holds: the median times per object of the slowest worker of both storages in µs, the geometric mean of the ratios, its interval, the pairs and the verdict. The summary counts the cells that are slower, undecided and missing. `just bench native` exits with 0 when every cell of the 30 passes, else with 1; `report` exits with 2 when the file is not a result of the gate.
- **The result.** One JSON document that validates against [`schema/native.schema.json`](schema/native.schema.json): the machine, the Python build, the versions, the rule, and for each cell the GIL record of the worker processes of each storage and the pairs of each scenario. The report applies the rule to the pairs again.
- **Python.** The phase 3 gate counts a run on CPython 3.14t: `UV_PYTHON=3.14t just bench native`. A run on another build is a report, not the gate.

## The client benchmark

F28 asks the native client to beat `ZEO.ClientStorage` on the client benchmark, cold and warm cache, and the phase 3 gate asks for it too. zodbshootout mixes the client with ZODB and the server. The client benchmark isolates the client: it calls the storage API of each client without ZODB, `loadBefore` for the loads and `new_oid`, `tpc_begin`, `store`, `tpc_vote` and `tpc_finish` for the commits. The clients are `ZEO.ClientStorage` and the native client, `zoe.zeo.ClientStorage` with `native=True`, both with their default settings but the cache. `python -m shootout client`, in `shootout/client.py` and `shootout/clientworker.py`, is the benchmark.

```sh
just bench client                                # both servers, 1000 objects per worker
just bench client --server zoe-serve --objects 200
just bench client report target/bench/client/<started>.json
```

- **Shapes.** One process with 1 thread, one process with 16 threads on one client, and 16 processes with one client each.
- **Record sizes.** 1 KB and 100 KB of random bytes.
- **Workloads.** `cold`: each worker loads `--objects` distinct oids (1000 by default), with no cache. Each load is `loadBefore(oid, tid)`, where `tid` follows the last transaction of the client, as the loads of a ZODB connection are. `warm`: the same loads from a client that opens the cache file that its `cold` round wrote; the open of the client and the verification of its cache count in the time and the CPU. `commit`: each worker commits `--commits` transactions of 100 new records (`max(1, objects // 50)` by default, so 20 with 1000 objects). When the spread of a commit row is large, give more commits.
- **Servers.** `zoe serve` (`zoe-serve`) and a ZEO 6 server on a FileStorage (`zeo`), each client against each. Each server has its default settings, but the ZEO server speaks the msgpack protocol M5 (`ZEO_MSGPACK`): the native client speaks only M5, as `zoe serve` does. So both clients speak M5 to both servers. `--server` takes one or both.
- **The same server state.** For each server and record size, the harness fills one store with the records that the workers load, then no round changes it. After the fill, a new client of ZEO with an empty cache loads each record once, and no round measures it. So the first `cold` round, which is a round of `ZEO.ClientStorage` in two rounds of three, finds the server as warm as the next ones. Each commit round runs on a fresh store. Both clients run the same calls in the same order.
- **Rounds.** Each row runs in at least three rounds (`--rounds`). The rounds of both clients interleave, and the client that goes first changes from one round to the next. Each round runs in a fresh process. The native client imports ZEO too, so the processes of both clients load ZEO and ZODB. Each process imports its client and the modules of its calls before the clock starts. So the clock of a `warm` round covers the open of the client and the verification of its cache, and no import. In the processes shape, that process starts the 16 worker processes (`spawn`), and they start their calls together.
- **The cache.** In the `cold` and `warm` rounds, each client has a cache file: `client` and `var` give `w<worker>-1.zec` to `ZEO.ClientStorage` and `w<worker>-1.zoec` to the native client. The `cold` round starts with no file and writes it. The `warm` round of the same client reopens it, and fails when it is not there. Both clients get the same `cache_size`, large enough to hold every record of the round. Against `zoe serve`, the harness reads `zoe_loads_total` on the admin port before and after each round: a `cold` round fails when the server counted fewer loads than the round made, and a `warm` round fails when the server counted any load. The ZEO server counts no loads, so nothing checks that the cache served the `warm` rows against it. These rows do not count in the verdict.
- **Numbers.** For each row: calls per second (the calls of all workers of a round, divided by the time of the round, the median of the rounds), p50 and p99 of one call (over the calls of all rounds), client CPU per object (the user and system CPU of the client processes of a round, their threads included, divided by the objects loaded or stored, the median of the rounds) and the spread of the rounds (the highest rate minus the lowest, divided by the median). A call is one `loadBefore`, or one commit of 100 records.
- **The result.** One JSON document that validates against [`schema/client.schema.json`](schema/client.schema.json): the machine, the Python build, `gil_enabled_by` (the modules that turned the GIL back on in a process of the run), the versions of both clients (`ZEO` and `zoedb`) and of `zoe serve`, the commit, the objects and one row per client, server, shape, workload and record size. Each row records the GIL of its processes in `workers`, as the results of zodbshootout do. `--output` names the file; the default is `target/bench/client/<started>.json`. A run that fails keeps its directory under the work directory, with the logs of its servers.

**The verdict.** `just bench client report FILE` prints one row per shape, workload, record size and server, with the rates of both clients, their ratio, their client CPU per object, their p50 and p99, and the spread. It first checks the file against the schema, and exits with 2 when the file does not match. In a row against `zoe serve`, the native client passes when it has a higher rate and a lower client CPU per object than `ZEO.ClientStorage`, and the row says `FAIL` otherwise. A row with the numbers of one client only says `missing`. A row against the ZEO server shows the same comparison in lower case and does not count. The report exits with 0 when every row against `zoe serve` passes and no row is missing, and with 1 otherwise.

**The disk.** For each server and record size, the work directory holds the load store, the cache files of both clients for one shape, and one commit store: `disk_needed` in `shootout/client.py` bounds it, and the harness checks the free space before it starts. With 1000 objects and records of 100 KB, and 20 commits per worker, that is about 10.1 GB; with records of 1 KB, about 0.7 GB. The native client allocates its whole cache file at the open.

The full run belongs on the reference machine, on CPython 3.14t with the 3.13 run beside it, as for the gate.

## The Plone end-to-end benchmark

B5 of the specification: Locust against a Plone 6.1 site, ZEO against ZOE, same Plone, same hardware. zodbshootout and the client benchmark measure the storage; this benchmark measures what a Plone user gets: completed pages and saves per second, their latency, and what conflicts cost. The harness is the uv project `bench/plone` (package `plonebench`). The findings of the last campaign are in [`docs/findings/plone-end-to-end.md`](../docs/findings/plone-end-to-end.md).

```sh
just plone zeo build && just plone zoeclient build && just plone zeoclient build
just bench plone prepare                       # the small fixture, once
just bench plone run --stack zoe-native --scenario edit --steps 1:60,4:60,16:60
just bench plone campaign --stacks zeo,zoe-native --scenarios browse,edit --reps 3
just bench plone report target/bench/plone/runs/* --out report.md --json summary.json
just bench plone serve --stack zeo             # a stack on a copy of the fixture, for a look
just bench plone test                          # the tests of the harness
```

**The stacks.** Every setting but the storage section is the same: the runner writes `zope.conf` and `wsgi.ini` of each instance from one template (`plonebench/stack.py`), with the eggs of the buildout scenarios of `plone/`.

| Stack | Server | Client | What differs from `zeo` |
| --- | --- | --- | --- |
| `zeo` | ZEO 6 (`runzeo`, FileStorage, pickle protocol Z5) | `<zeoclient>`: `ZEO.ClientStorage` | the baseline; the ZEO server resolves conflicts with ZODB's resolver |
| `zoe-native` | `zoe serve` | `<zoeclient>`: `zoe.zeo.ClientStorage`, native client; `zoedb[plone]` | the production combination: the native client, the `sequences` extension and `zoe.plone`, which gives `portal_uidgenerator` its ids from a store sequence |
| `zoe-asyncio` | `zoe serve` | `<zoeclient>` with `native false`; `zoedb[plone]` | ZEO's asyncio client with the ZOE extensions: the sequence without the native client |
| `zoe-stock` | `zoe serve` | `<zeoclient>`: stock `ZEO.ClientStorage` over M5, no zoedb | the server alone, with no client extension |

On `zoe serve` every client resolves conflicts through the vote (tier 3). The structural and field-level merges, shadow mode and early write-intent locks of phase 4 are not built, so no stack has them, and the harness has no switch for them. `zoe serve` always compresses records with zstd at level 3; it has no option to turn compression off, so the benchmark has no compression experiment.

**The constants** (`stack.Settings`, recorded in each result): 2 Zope instances with 2 request threads each, a ZODB cache of 30,000 objects per connection and a pool of 7 connections, a storage client cache of 128 MB in memory for each instance (no persistent cache file, for either client), a blob cache per instance, on Linux the server on CPU 0, the instances on CPUs 1 and 2, Locust on CPU 3 (`taskset`). Both servers make a commit durable before `tpc_finish` returns: FileStorage calls `fsync` in `_finish_finish`, `zoe serve` syncs its group commit. The page cache of the store disk is shared and stays warm between runs: the container cannot drop it.

**On macOS.** The harness runs there too, with three differences that each result records under `environment.machine`. macOS has no CPU affinity, so no process is pinned, and the scheduler puts each thread on a performance or an efficiency core (`cpu_pinning`, `core_kinds`). On macOS `fsync(2)` leaves the data in the cache of the drive, and only `F_FULLFSYNC` writes it to the medium. The Rust std of `zoe serve` uses `F_FULLFSYNC`, and Python's `os.fsync` does not. So by default `probe/zeoserver/launch.py` makes `os.fsync` of the ZEO server use `F_FULLFSYNC`, and both servers pay for the same durability. `--zeo-fsync plain` keeps stock ZEO (`durability`). The sampler reads `proc_pid_rusage` and psutil instead of `/proc`. The runnable time minus the CPU time stands for the run delay, and steal time, disk flushes and the split of context switches are absent. The store copies are APFS clones (`/bin/cp -c`).

**The fixture.** `prepare` builds a deterministic synthetic site (`plonebench/corpus.py`; presets `small`, `medium`, `large`, or `--spec FILE` with any field of `SiteSpec`): sections of nested folders, Documents and News Items with generated text and subjects, Files of 16 KB to 1 MB, collections, users with local roles through groups, three workflow states, and the folders `bench/inbox`, `bench/hot`, `bench/shared` and `bench/import` for the contention scenarios. It builds the site on a ZEO server, converts the `Data.fs` with `zoe convert`, then reads a logical digest of the site through each store (`inzope/digest.py`: every object with its fields, state, UID, local roles, View roles, dates, versions and workflow history; the users and groups; the catalog and fixed queries) and refuses the fixture when they differ. The text is made of generated words, never of a natural language. The fixture lives under `target/bench/plone/fixtures/<name>`, with the password of the accounts in a file only the user reads. A run never opens it: it copies the store first.

**The scenarios** (`plonebench/workload.py`). Each simulated user has its own session and account: anonymous visitors, members, editors who hold local roles on one section, an importer and a maintainer. The UI scenarios go through the rendered pages and the forms of Plone, with the CSRF token of plone.protect; the REST scenarios go through plone.restapi with a JWT, and the report names the interface of each scenario.

| Scenario | Users do |
| --- | --- |
| `browse` | folders, Documents, News Items, File views and downloads, up the breadcrumbs; anonymous and logged in |
| `catalog` | `@@search` with text, filters, sorting and a second page; collections; REST `@search` with metadata |
| `edit` | the edit form on Documents of their own section, a user per Document |
| `create-spread` / `create-shared` | the add form in folders of their own section / all in `bench/inbox` |
| `workflow` | publish and retract from the workflow menu, then an anonymous request that checks the visibility |
| `mixed` | a weighted mix of the above, weights in the result |
| `mixed-background` | `mixed`, beside one maintainer that reindexes `SearchableText` in a loop and one importer that posts Documents through REST |
| `contention-folder` | the edit form, a Document each, all in one folder |
| `contention-fields` | REST PATCH, two users on each Document: one writes the title, the other the description |
| `contention-same` | REST PATCH, every user writes the title of one of four hot Documents |

A run steps the users of a closed workload (`--steps users:seconds,...`, no think time by default): each user waits for its response before its next operation, so a slower server lowers the offered rate. The first seconds of each step stay out (`--trim`). Locust gets no retry: a write that fails is a failed operation, unless `--client-retries N` gives a bounded retry, whose latency runs from the first request to the final outcome.

**The ledger and the verification.** Locust writes one record for each logical operation (`plonebench/ledger.py`): what it wrote and what the site acknowledged, with the outcome `ok` (the response showed the effect), `failed` (an HTTP error: Zope aborted the transaction), `ambiguous` (no complete answer: the write may have committed) or `rejected`. A write leaves a pending record before its request, so a run that ends during a request leaves an ambiguous one. After the run, the instances stop, and `inzope/observe.py` reads the site through a client of its own, read-only, before any repair. `plonebench/verification.py` then checks: each acknowledged creation exists, no object exists without its operation, a refused write did not commit, UIDs and history ids are unique, each edited field holds the last acknowledged value (with several writers, a value that no later acknowledged write superseded), each acknowledged edit added one CMFEditions version and each transition one workflow history entry, anonymous may View exactly the published objects, each object has one catalog entry with its title, state, date and security, and a search for the token of the current title finds the object while a superseded token does not. The verdict is in each result.

**The instrumentation.** A WSGI filter in each Zope process (`probe/plonebench_probe.py`, first in the pipeline of `wsgi.ini`) writes one JSON line per request: the op id and attempt that Locust sends in headers (never a URL or a body), the attempts of the publisher, each `ConflictError` with the class of its object and whether the publisher retried, the commit time, the storage calls of the request (`loadBefore`, `store`, `tpc_vote`, `tpc_finish`, `tpc_abort`) with their time and bytes, the conflicts that ZODB's resolver resolved in the process, and the CPU time of the thread. The same filter runs on every stack. The ZEO server resolves conflicts in its own process, so a probe there (`probe/zeoserver`) counts them; on `zoe serve` the clients resolve, and the probe of the instances counts them. The runner reads `server_status` of both servers, and the Prometheus metrics of `zoe serve` (lock waits, vote and finish latency, fsync batch sizes), before and after the measured window, and samples the processes once a second (`/proc` on Linux, `proc_pid_rusage` and psutil on macOS): CPU, resident memory, I/O bytes, context switches and run-queue delay of each process, the CPUs (steal included) and the disk of the store (flushes included). A counter that a stack does not keep is absent from its result, never 0. `--no-probe` runs without the filter, to measure its cost.

**The results.** Each run writes `target/bench/plone/runs/<started>-<scenario>-<stack>-r<n>/`: `result.json` (the machine, the software, the commit, the stack, the settings, the scenario and its mix, the steps, the fixture and its equivalence, the numbers of each step, the server counters, the resources and the verdict), the ledger, the probe files, the samples, the CSV files of Locust and the logs. The store copy goes at the end. `report` reads run directories and prints one table per scenario: per stack and step, the median rate of successful operations over the repetitions with its range, p50, p95 and p99 pooled over the repetitions (a p99 of fewer than 200 operations is marked `~`), failures, first-attempt successes, server attempts per operation, conflicts per 1,000 operations, exhausted retries, CPU per operation of Zope and of the server, bytes the server wrote per operation, the CPU of Locust and the verdicts. A campaign alternates the order of the stacks from one repetition to the next.

## The results repository

The results go to the [`adeweb-be/zoe-bench-results`](https://github.com/adeweb-be/zoe-bench-results) repository, which keeps only full runs:

```text
index.json
results/<machine>/<python>/<storage>/<started>-<commit>.json
```

- `<python>` is a short name for the build: `cp313`, or `cp314t` for a free-threaded 3.14.
- `<started>` is the UTC start of the run, `YYYYMMDDTHHMMSSZ`. `<commit>` is the first 12 characters of the ZOE commit.
- `index.json` lists every result with its path, machine, Python, storage, start and commit, oldest first. `just bench index` rewrites it from the tree.

The harness writes the same layout under its output directory. A quick run goes under `quick/` instead of `results/` and is never published.

To publish a full run, copy the `results/` tree of the run into a checkout of the results repository, run `just bench index <checkout>`, and commit both.

## The dashboard

The dashboard is the GitHub Pages site of the results repository, <https://adeweb-be.github.io/zoe-bench-results/>: one static page, `index.html`, with no build step. It reads `index.json`, then the result documents it lists. For each machine and Python build, it plots `ops_per_second` and `p99_seconds` of each scenario and cell against the start of the run, with one line per storage. A point links to its commit. The quick cells are a subset of the full cells, so a regression gate can compare a quick run with the last ten full runs on the same machine.

The page lives in the results repository, not in this directory.

## The reference machine

The numbers mean something only when they come from one fixed machine. For now the reference machine is a developer workstation; a dedicated runner can replace it later. Every result names its machine, so results from another machine never mix with the baseline on the dashboard.
