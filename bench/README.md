# Benchmarks

This directory holds the reference baseline: a harness that runs [zodbshootout](https://pypi.org/project/zodbshootout/) against FileStorage, ZEO 6 and RelStorage on PostgreSQL, and the JSON schema of its results. The same harness measures ZOE in-process, the storage `zoe`, through the `<zoe>` stanza, and ZOE server, the storage `zoe-server`: a stock ZEO client, through the `<zeoclient>` section, of `zoe serve`. `zoe-bench` (contention and server saturation) and the criterion microbenchmarks live in `crates/`.

## Run it

`just bench` is the entry point. Run it inside `devenv shell`.

| Command | What it does |
| --- | --- |
| `just bench` | the full set against every storage; writes to `target/bench/results` |
| `just bench run --quick` | the quick set, the regression gate of a pull request |
| `just bench run --storage zeo --quick` | one storage only |
| `just bench test` | the tests of the harness (the acceptance test of the baseline) |
| `just bench index DIR` | rewrites `index.json` of a results tree |
| `just bench gate DIR` | the parity gate: `zoe-server` against `zeo` in each cell of a results tree but `read_hot` (see [The gate](#the-gate)) |

The bench project takes `python/zoedb` as an editable path source, as the uv workspace of the Python tests does. So the harness runs the Python code of the tree. uv builds the extension with maturin in the release profile, and the cache keys in `python/zoedb/pyproject.toml` tell uv to build it again when the Rust code changes.

RelStorage runs when `ZOE_BENCH_PG_DSN` (or `--dsn`) names a PostgreSQL database that the harness may empty. Without it, the harness skips RelStorage and says so. The Dagger function `bench` starts PostgreSQL 17 as a service and runs the harness tests, then the baseline, and returns the results tree:

```sh
dagger call --mod ci bench --quick export --path target/bench/ci-results
```

Other settings:

- `ZOE_BENCH_MACHINE` or `--machine`: the machine name in the results. The default is the CPU model, for example `intel-core-ultra-7-155h`.
- `ZOE_BENCH_COMMIT`: the commit to record. The default is `git rev-parse HEAD`.
- `UV_PYTHON`: the interpreter, `3.13` by default. Each interpreter gets its own environment under `target/bench/`.
- `--workdir`: where the storages write their files, `target/bench/work` by default. Keep it on the disk under test, not on a tmpfs.
- `ZOE_BINARY`: the `zoe` binary of the storage `zoe-server`. Without it, the harness takes `target/release/zoe` of the repository (under `CARGO_TARGET_DIR` when it is set), and cargo builds it when it is missing. `just bench run` and `just bench test` build it in release first, so it is never stale.

## What it measures

zodbshootout runs five scenarios: `add`, `update`, `read_cold`, `read_warm` (zodbshootout's write/read: write the objects, then read them back in the next transaction; the time includes both) and `read_hot`. Each runs in a set of cells. A cell is a mode, a concurrency and an object size. Every transaction holds 100 objects.

| Set | Cells | Samples per scenario |
| --- | --- | --- |
| quick | threads, concurrency 1 and 16, objects of 1 KB | 5 |
| full | threads at concurrency 1, 4, 16 and 64; processes at 4, 16 and 64; objects of 1 KB and 100 KB | 20 (2 processes × 10) |

- In the threads mode, one process runs the workers as threads that share one `DB`, as in one Zope process. In the processes mode, each worker is a process with its own `DB`, as in several Zope processes.
- FileStorage and `zoe` run only in the threads mode: only one process can open a FileStorage file or a ZOE store. ZEO, RelStorage and `zoe-server` run in every cell.
- A sample is the time for the slowest worker to do one transaction. A result reports the mean, the median and the 99th percentile of these times. `ops_per_second` is objects per second for all workers together: concurrency × 100 ÷ mean.
- Every storage runs with its default settings. The `<zoe>` stanza sets only `path`. RelStorage keeps history, like FileStorage and ZEO. The ZEO server is a separate process on the same machine.
- `zoe-server` runs `zoe serve --path <cell>/store --listen 127.0.0.1:0` in the work directory of each cell, a separate process on the same machine, as the ZEO server. No `ZOE_*` variable of the environment reaches it, so it runs with its default settings. The client is the `<zeoclient>` section of ZEO with its default settings, as for `zeo`. The result records the version of the server as `zoe` in `software`.

The corpus is synthetic: zodbshootout generates its own objects. Results are numbers, never data.

## Results

Each run of one storage gives one JSON document that validates against [`schema/results.schema.json`](schema/results.schema.json). The document names the storage, the machine (CPU, logical CPUs, memory, kernel, the disk that holds the storage files), the Python build (version, free-threaded or not), the versions of every package and server, the commit, and one entry per scenario and cell. It holds no host name, user name or path.

## The gate

The phase 2 gate of N4 is parity: `zoe-server` at least 1× `zeo` in each zodbshootout cell that reaches the server, both through `ZEO.ClientStorage`. `python -m shootout gate`, or `just bench gate DIR`, reads the results tree DIR in the layout below and turns the gate into a pass or a fail per cell:

```sh
just bench gate target/bench/results
```

- For each machine, Python build and set (quick or full), it keeps the last result of each storage, by the start of the run. So it compares one machine, one Python build and the same cells, and never a quick run with a full run.
- For each scenario and cell of the baseline, it prints `ops_per_second` of both storages and their ratio.
- It leaves `read_hot` out. zodbshootout's `read_hot` empties the object cache of the connection, then reads the objects from the cache of `ZEO.ClientStorage`, so no read reaches the server. The report prints each `read_hot` cell with the numbers that exist and the verdict `left out`, and says why. A `read_hot` cell counts neither as below nor as missing. `read_hot` still runs and stays in the results.
- It exits with 0 when the ratio of each cell that counts is at least `--ratio`. It exits with 1 when a ratio is below, when a cell of the baseline has no counterpart in the storage, or when the tree holds no result of the baseline.
- `--storage zoe-server --baseline zeo --ratio 1` are the defaults. `--ratio` takes any positive value. The phase 2 gate counts only the full set of the reference machine.

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
