# Benchmarks

This directory holds the reference baseline: a harness that runs [zodbshootout](https://pypi.org/project/zodbshootout/) against FileStorage, ZEO 6 and RelStorage on PostgreSQL, with history (`relstorage`) and without (`relstorage-hf`), and the JSON schema of its results. The same harness measures ZOE in-process, the storage `zoe`, through the `<zoe>` stanza, and ZOE server twice: the storage `zoe-server`, a stock ZEO client of `zoe serve` through the `<zeoclient>` section, and the storage `zoe-native`, the native client of `zoe serve` through the `<zoeclient>` section with `native true`. `zoe-bench` (contention and server saturation) and the criterion microbenchmarks live in `crates/`.

## Run it

`just bench` is the entry point. Run it inside `devenv shell`.

| Command | What it does |
| --- | --- |
| `just bench` | the full set against every storage; writes to `target/bench/results` |
| `just bench run --quick` | the quick set, the regression gate of a pull request |
| `just bench run --gate --storage zeo zoe-native` | the gate set, the head-to-head that the phase gates count; results go under `gate/` |
| `just bench run --storage zeo --quick` | one storage only |
| `just bench test` | the tests of the harness (the acceptance test of the baseline) |
| `just bench test FILE...` | the tests of those files only |
| `just bench client` | the client benchmark: `ZEO.ClientStorage` against the native client (see [The client benchmark](#the-client-benchmark)) |
| `just bench client report FILE` | the rows of a result of the client benchmark and its verdict |
| `just bench index DIR` | rewrites `index.json` of a results tree |
| `just bench gate DIR` | the parity gate of phase 2: `zoe-server` against `zeo` in each cell of a results tree but `read_hot` (see [The gate](#the-gate)) |
| `just bench gate --storage zoe-native --python cp314t DIR` | the phase 3 gate: `zoe-native` at least 3× `zeo` in each cell of the 3.14t results but the `read_hot` cells that hit the native cache (1 KB; 100 KB with threads at concurrency 1 and with processes), with the other builds printed beside them |

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
| gate | threads, concurrency 1 and 16; processes at 16; objects of 1 KB and 100 KB | 3 |
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

N4 sets two gates against ZEO 6 through `ZEO.ClientStorage`. The phase 2 gate is parity: `zoe-server` at least 1× `zeo` in each zodbshootout cell that reaches the server. The phase 3 gate is `zoe-native`, `zoe serve` through the native client, at least 3× `zeo` in every cell but the `read_hot` cells whose reads hit the cache of the native client: at 1 KB, and at 100 KB with threads at concurrency 1 and with processes. `python -m shootout gate`, or `just bench gate DIR`, reads the results tree DIR in the layout below and turns a gate into a pass or a fail per cell:

```sh
just bench gate target/bench/results
just bench gate --storage zoe-native --python cp314t target/bench/results
```

- For each machine, Python build and set (quick or full), it keeps the last result of each storage, by the start of the run. So it compares one machine, one Python build and the same cells, and never a quick run with a full run.
- For each scenario and cell of the baseline, it prints `ops_per_second` of both storages and their ratio.
- The rules of each storage are constants of `shootout/gate.py`, not options. `--storage zoe-native` checks a ratio of 3 by default and counts every cell but the `read_hot` cells whose reads hit the native cache. The native client has a cache of its own, so its `read_hot` is a number of ZOE. But at 1 KB ZODB itself takes most of the time of a hit. At 100 KB with threads at concurrency 1 and with processes, each read hits the cache, and ZODB alone takes more than a third of the time of ZEO, so no client reaches 3× (the finding of ZOE-187, #385). Its `read_hot` at 100 KB with threads at a higher concurrency counts: the threads share one cache of the default size, which does not hold their objects, so the reads reach the server. Every other storage, `zoe-server` first, checks parity by default and leaves `read_hot` out: zodbshootout's `read_hot` empties the object cache of the connection, then reads the objects from the cache of `ZEO.ClientStorage`, so no read reaches the server. The report prints each cell that it leaves out with the numbers that exist and the verdict `left out`, and says why in a `left out:` line, such as `left out: read_hot at 1 KB, because ...`. Such a cell counts neither as below nor as missing. The summary line names the cells that it leaves out: `each cell but read_hot at 1 KB, read_hot at 100 KB with threads at concurrency 1, read_hot at 100 KB with processes` for `zoe-native`, `each cell but read_hot` for the others. `read_hot` still runs and stays in the results.
- It exits with 0 when the ratio of each cell that counts is at least the ratio of the gate. It exits with 1 when a ratio is below, when a cell of the baseline has no counterpart in the storage, or when the tree holds no result of the baseline.
- `--storage zoe-server --baseline zeo` are the defaults. `--ratio` takes any positive value in place of the default ratio of the storage.
- `--python cp314t` counts only the results of that build. The report prints the groups of the other builds after them, marked `does not count`, with their verdicts in lower case. Without `--python`, every build counts.
- The phase 2 gate counts only the full set of the reference machine. The phase 3 gate counts the gate set (`--gate`), a head-to-head of `zeo` and `zoe-native` on 6 cells; the full set is the comprehensive benchmark of a later phase. It counts the results on CPython 3.14t, each storage as its stack runs there, with the report of 3.13 printed beside it: on 3.13 the threads cells measure one client process under the GIL, and on 3.14t they measure the storage when it commits in parallel. The GIL record of each result says which workers ran with the GIL. The RelStorage row of N4 counts `relstorage` and `relstorage-hf`.

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
