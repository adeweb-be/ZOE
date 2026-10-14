# ZOE-150 POC: the budget of each cell of the phase 3 gate

**This is a spike. Do not merge it.** The branch holds the scripts and the raw results of ZOE-150 (#298). The finding is `docs/spikes/phase3-budget.md` on the card branch.

The branch starts from `poc/ZOE-077-server-performance` and merges `main`. So `zoe serve` of this branch is `main` with the toggles of ZOE-077 (see `../ZOE-077/README.md`), and `../ZOE-077/rustclient` is the POC client. One change to that client: its pool opens a connection only when a transaction finds no free one, up to `pool`. A process with one thread then keeps one connection, as a real pool does.

## What runs

| Configuration | Server | Client |
| --- | --- | --- |
| `zeo` | ZEO 6 on a FileStorage, on the interpreter of the client, as the harness runs it | `ZEO.ClientStorage` |
| `poc-proto` | `zoe serve` of this branch with `ZOE_POC_STORECOMPRESS=1` and `ZOE_POC_TIDONLY=1` | the POC client on `zoe-client`, a pool of 16 connections, no cache |
| `poc-main` | `zoe serve` as on `main` | the same |
| `floor` | none | zodbshootout in the process, on a DemoStorage: its changes go to a MappingStorage, its base is a read-only FileStorage that the driver fills first |

## The reduced run

The run takes the 14 cells of the full set (threads at concurrency 1, 4, 16 and 64, processes at 4, 16 and 64, objects of 1 KB and 100 KB), the five scenarios, CPython 3.13 and 3.14t: 140 rows. It is short:

- One run is one cell of one configuration: a fresh store, and the five scenarios in one process. `shoot.py` runs zodbshootout with `--worker`, so pyperf starts no worker process, and there is no warmup.
- A transaction holds 100 objects, as in the harness. Each worker runs `max(1, 16 // concurrency)` transactions per scenario, not 100 or 30. One run at concurrency 64 with 100 KB objects writes about 2 GB.
- Each scenario of a run gives one sample. Three interleaved rounds of `zeo` and `poc-proto` give three samples per row.
- After the rounds, while the run is under its budget (840 s): the spans of `zoe serve` for each cell (3.13, `poc-proto`), then one round of `poc-main`.

## Run

Inside `devenv shell`, from the root of the worktree:

```sh
MAIN_REV=<commit of main> bench/poc/ZOE-150/setup.sh   # or ZOE_MAIN=/path/to/main/release/zoe
bench/poc/ZOE-150/full.sh                              # results/run.jsonl, results/run.log
target/bench/venv-3.13/bin/python bench/poc/ZOE-150/summarize.py bench/poc/ZOE-150/results/run.jsonl
```

`run.py` skips the runs that its output already holds, so a stopped run starts again where it stopped. `--phases`, `--cells` (for example `threads:4:1`), `--pythons`, `--rounds`, `--main-rounds` and `--budget` select a part.

| Script | Does |
| --- | --- |
| `setup.sh` | builds `zoe-main` and `zoe-proto` into `target/zoe150`, the environments `target/bench/venv-3.13` and `venv-3.14t` from the lock of `bench/shootout` (without zoedb and RelStorage), the POC client for each interpreter, and py-spy |
| `full.sh` | starts `run.py` from the root of the worktree; the server gets the first quarter of the CPUs, the clients the rest |
| `run.py` | the phases `machine`, `fsync`, `gil`, `rtt`, `pyspy`, `floor`, `main`, then the optional `spans` and `ref`; a run with more than 1.5 cores of other load runs again, and both lines stay; a cell that does not fit the disk is skipped |
| `driver.py` | one run: the server pinned to `--server-cpus`, the client to `--client-cpus`; one JSON line with ops/s and the mean of each scenario, the CPU of the server and of the client, the CPU of the other processes (`foreign_cores`) and of the kernel threads, the governor and the frequency of each CPU, the `/metrics` deltas of `zoe serve`, the size of the store, the modules that turned the GIL back on; with `--spans`, the OTLP spans of `zoe serve` summed by name; with `--profile`, the counters of the prototypes |
| `shoot.py` | zodbshootout in one process with fewer transactions per worker |
| `rtt.py` | the round trip of one call: loopback TCP between two Python processes, `new_oids(1)` and `ping` through `ZEO.ClientStorage`, `new_oids(1)` through the POC client |
| `fsyncx.py` | the cost of `fdatasync` and `fsync` after 4 KiB to 800 KiB of writes |
| `gil.py` | imports the stacks of the runs and says whether the GIL is on |
| `wrap-pyspy.sh` | a prefix of zodbshootout: py-spy samples the client (3.13 only: py-spy does not read a free-threaded 3.14) |
| `summarize.py` | the tables of the finding |

## On EC2: `aws/`

`aws/launch.sh` runs the reduced run on a spot instance with local NVMe (`c6id.4xlarge` by default) in the region of the aws configuration (`eu-west-3` by default), copies `results/` back to `results/aws/`, and terminates the instance. It falls back to on-demand only when the spot launch fails. `aws/user-data.sh` mounts the local NVMe, installs nix 2.35.2 and devenv 2.4.0 as the CI does, clones the public repository at the commit, builds with `setup.sh` and runs `full.sh`.

```sh
bench/poc/ZOE-150/aws/launch.sh --dry-run --hours 1         # the cost, no launch
bench/poc/ZOE-150/aws/launch.sh --ssh-cidr <address>/32     # the run
```

Guard rails:

- `--instance-initiated-shutdown-behavior terminate`, and `shutdown -h +90` first in the user data: the instance ends after 90 minutes, also without the launcher.
- The tag `Project=zoe-spike` on the instance, its volume, the key pair and the security group.
- An exit trap terminates the instance and deletes the key pair and the security group, also on an error or Ctrl-C.
- The security group allows SSH only from `--ssh-cidr`. The key pair is new for each launch, and its private key stays in a temporary directory on the launching machine.
- `--dry-run` prints the spot price × `--hours` and the on-demand price, and launches nothing.
- No credential goes to the repository, the user data or the logs. The instance needs none.
