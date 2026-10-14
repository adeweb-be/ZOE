# ZOE-150 POC: the budget of each cell of the phase 3 gate

**This is a spike. Do not merge it.** The branch holds the scripts and the raw results of ZOE-150 (#298). The finding is `docs/spikes/phase3-budget.md` on the card branch.

The branch starts from `poc/ZOE-077-server-performance` and merges `main`. So `zoe serve` of this branch is `main` with the toggles of ZOE-077 (see `../ZOE-077/README.md`), and `../ZOE-077/rustclient` is the POC client. One change to that client: its pool opens a connection only when a transaction finds no free one, up to `pool`. A process with one thread then keeps one connection, as a real pool does.

## What runs

| Configuration | Server | Client |
| --- | --- | --- |
| `zeo` | ZEO 6 on a FileStorage, on CPython 3.13 | `ZEO.ClientStorage`, as the harness runs it |
| `poc-main` | `zoe serve` as on `main` | the POC client on `zoe-client`, a pool of 16 connections, no cache |
| `poc-proto` | `zoe serve` of this branch with `ZOE_POC_STORECOMPRESS=1` and `ZOE_POC_TIDONLY=1` | the same |
| `floor` | none | zodbshootout in the process, on a DemoStorage: its changes go to a MappingStorage, its base is a read-only FileStorage that the driver fills first |

Each run is one value of one scenario of one cell, as the harness runs it since ZOE-135: a fresh store, one pyperf worker, one warmup, one value, one loop per sample, 100 objects per transaction. The store is on the disk of the work directory, never on tmpfs, and the driver removes it after the run.

## Run

Inside `devenv shell`, from the root of the worktree:

```sh
ZOE_MAIN=/path/to/main/target/release/zoe bench/poc/ZOE-150/setup.sh   # or MAIN_REV=<commit>
target/bench/venv-3.13/bin/python bench/poc/ZOE-150/run.py              # the full run
target/bench/venv-3.13/bin/python bench/poc/ZOE-150/summarize.py bench/poc/ZOE-150/results/full.jsonl
```

`run.py` appends one JSON line per run to `results/full.jsonl` and skips the runs that the file already holds, so a stopped run starts again where it stopped. Its phases, in order: `machine`, `fsync`, `gil`, `rtt`, `pyspy`, `floor`, `main`, `spans`. `--phases`, `--cells` (for example `threads:4:1`), `--pythons` and `--rounds` select a part.

| Script | Does |
| --- | --- |
| `setup.sh` | builds `zoe-main` and `zoe-proto` into `target/zoe150`, the environments `target/bench/venv-3.13` and `venv-3.14t` from the lock of `bench/shootout` (without zoedb and RelStorage), the POC client for each interpreter, and py-spy |
| `driver.py` | one run: the server pinned to `--server-cpus`, zodbshootout pinned to `--client-cpus`, one JSON line with ops/s, the samples, the CPU of the server and of the client, the CPU of everything else (`foreign_cores`), the governor and the frequency of each CPU before and after, the `/metrics` deltas of `zoe serve`, the size of the store, and the modules that turned the GIL back on; with `--spans`, the OTLP spans of `zoe serve` summed by name; with `--profile`, the counters of the prototypes |
| `run.py` | the full run; a run with more than `--foreign-limit` cores of other load runs again, and both lines stay |
| `rtt.py` | the round trip of one call: loopback TCP between two Python processes, `new_oids(1)` and `ping` through `ZEO.ClientStorage`, `new_oids(1)` through the POC client |
| `gil.py` | imports the stacks of the runs and says whether the GIL is on |
| `wrap-pyspy.sh` | a prefix of zodbshootout: py-spy samples the client |
| `summarize.py` | the tables of the finding |

## Defaults

- Rounds: 3, interleaved per scenario of each cell: `zeo`, `poc-main`, `poc-proto`, then again.
- Cores: the server on CPUs 0-5 (three performance cores and their siblings), the clients on CPUs 6-19 (three performance cores and eight efficiency cores). CPUs 20 and 21, the low-power cores, run the rest of the machine.
- The ZEO server runs on CPython 3.13 for both client interpreters: its fastest interpreter, as in ZOE-077.
- The floor skips `add` and `update` at concurrency 64 with 100 KB objects: the DemoStorage keeps every write in memory, about 40 GB.
