# Spike: the budget of each cell of the phase 3 gate

Card ZOE-150, requirements N4 and F28.

**Verdict.** Through a client without a cache and with a pool of connections, `zoe serve` with the prototypes of ZOE-077 reaches 3× ZEO in 26 of the 140 rows of the reduced run, and in none of the `add` rows of 100 KB or the `read_hot` rows. 3× is out of reach for any storage in 41 of the 114 rows under 3×:

- 24 rows: ZODB and zodbshootout alone, on an in-process storage, reach less than 3× ZEO.
- 17 `read_hot` rows: ZODB alone takes the time of a hit.

So the gate "at least 3× in every scenario" cannot hold for these rows, whatever the native client and the server do. In the other 73 rows, four terms keep the cell under 3×:

- 28 rows: the commit of 100 KB records, with compression on the path of the transaction and a slow redb finish.
- 18 rows: the commit of 1 KB records, the finish of redb.
- 16 rows: the load path: the round trip at 1 KB, the copy and the decompression at 100 KB.
- 11 rows: the client cache, which the POC client does not have.

The run is not the full set of the harness. It is a reduced run on an EC2 instance (see Method), so the numbers give the shape and the terms, not the gate.

## Question

For each cell of the full set and each Python build, is 3× ZEO 6 through `ZEO.ClientStorage` reachable for `zoe serve` through a native client, which term of the time of a transaction keeps a cell under 3×, and which card moves it?

## Method

**What runs.** zodbshootout 0.8.0, the five scenarios, the 14 cells of the full set: threads at concurrency 1, 4, 16 and 64, processes at 4, 16 and 64, objects of 1 KB and 100 KB. CPython 3.13.15 and 3.14.7t, so 140 rows.

- **zeo**: the ZEO 6 server on a FileStorage, and `ZEO.ClientStorage`. The ZEO server runs on the Python build of the client, as the harness runs it: 3.14t for the 3.14t rows. On 3.14t the one thread of the ZEO server is slower, so the 3.14t rows of processes favour the POC client.
- **poc**: the POC client of ZOE-077 (`zoeclientx`, `zoe-client` through PyO3), with a pool of up to 16 connections and no cache. The pool opens a connection only when a transaction finds no free one. The server is `zoe serve` of the POC branch: `main` at 2ad287e with the prototypes of ZOE-077 on, compression at each `storea` (`ZOE_POC_STORECOMPRESS=1`) and a vote that reads only tids (`ZOE_POC_TIDONLY=1`).
- **floor**: no server. zodbshootout on a DemoStorage in the process: the changes go to a MappingStorage, and the base is a read-only FileStorage that the driver fills first. This is the time of ZODB and zodbshootout alone.

**The reduced run.** The human capped the run at 10 to 15 minutes, so the run is not comparable with the full set of the harness:

- One process runs the five scenarios of a cell on a fresh store, with pyperf's `--worker`: no pyperf worker process, and **no warmup**.
- A transaction holds 100 objects, as in the harness, but each worker runs `max(1, 16 // concurrency)` transactions per scenario, **not 100 or 30**. A run at concurrency 64 with 100 KB objects writes about 2 GB.
- Each scenario of a run gives **one sample**, not 20.
- The rounds are **interleaved**: zeo, poc, zeo, poc, zeo, poc, for each cell and Python build. So each row has three samples per side. The tables give the median.

**Machine.** An EC2 `c6id.4xlarge` spot instance in `eu-west-3`, which ran nothing else: Intel Xeon Platinum 8375C at 2.90 GHz, 16 vCPUs (8 cores, 2 threads each), 30.8 GB of memory, Linux 7.0 for AWS. The stores were on the local NVMe of the instance (`Amazon EC2 NVMe Instance Storage`, ext4, `noatime`), never on tmpfs. The guest has no cpufreq driver, so it shows no governor and no frequency: the hypervisor sets them. The load of the machine before the run was 0.0 cores.

**Cores.** The server ran on CPUs 0-3 and the clients on CPUs 4-15, with `taskset`. Each run records the CPU of the server, of the client, of the kernel threads and of the other processes.

**Rounds and their spread.** The spread of a row is (max − min) ÷ median of its three rounds. It is 6 % for zeo and 5 % for poc in the median row. In 9 rows of 10 it is under 45 % for zeo and under 14 % for poc. The widest rows are all in the processes mode, for zeo, where the client cache of each process and the one thread of the server make the rounds differ:

| row | zeo spread | poc spread |
| --- | --- | --- |
| `read_hot`, processes, 64, 100 KB, 3.14t | 320 % | 2 % |
| `update`, processes, 64, 100 KB, 3.14t | 114 % | 5 % |
| `read_hot`, processes, 4, 1 KB, 3.13 | 104 % | 16 % |
| `read_warm`, processes, 64, 100 KB, 3.14t | 103 % | 4 % |

The `## Cells under 3×` table gives the spread of each row under 3×.

**Disturbed rounds.** The driver marks a run disturbed when other processes use more than 1.5 cores, and runs it again. Both runs stay in the raw results. The marks fell on the processes runs of 3.14t: 64 of the 76 runs were marked, which made 34 reruns. The tables use the last run of each round. The load came from the run itself, not from outside:

- The machine was idle (0.0 cores before the run), and no run on 3.13 or in the threads mode exceeded 0.83 cores of other load.
- On 3.14t the default start method of `multiprocessing` is `forkserver`. The worker processes are children of the fork server, so the driver does not count their CPU as client CPU, and they show as other load (median 3.0 cores, at most 10.2). On 3.13, with `fork`, the same cell records 42 s of client CPU; on 3.14t it records 0.8 s.

So these rounds are valid. Only the client CPU of the 3.14t processes rows is missing, and this finding uses no client CPU from those rows.

**GIL.** On 3.14t no module turned the GIL back on: neither the import check of the stacks nor any run recorded one. The stacks are persistent, BTrees and zodbpickle from the forks of the lock, ZODB, ZEO, msgpack, zodbshootout, pyperf and the POC client.

**Time.** The run took 1,414 s, about 23.5 minutes. That is over the cap of 15 minutes:

| phase | time |
| --- | --- |
| machine, fsync, GIL, round trips | 8 s |
| py-spy profile | 13 s |
| floors | 104 s |
| interleaved rounds | 1,288 s |
| spans of the server, and `zoe serve` as on `main` | 0 s: skipped |

- The reruns of the 3.14t processes runs took 300 s.
- The 35 runs at concurrency 64 with 100 KB objects, reruns and floors included, took 657 s of the 1,311 s of runs. A ZEO run of these cells took 17 to 41 s.
- The run was over its budget of 840 s when the rounds ended, so it skipped its two optional phases: the OTLP spans of `zoe serve` for each cell, and one round of `zoe serve` as on `main`. This finding therefore has no span split of the server and no row for `main`. Its server terms come from `/metrics` (`zoe_vote_latency_seconds`, `zoe_finish_latency_seconds`, `zoe_fsync_batch_size`) of the timed runs, and from the floors.
- The first launch stopped before the run, because the user data had no `HOME`. Its log is in the raw results.

**Raw results and scripts.** The branch `poc/ZOE-150-phase3-budget`, not for merge:

- the scripts at 49113ee: `bench/poc/ZOE-150/`, with the launcher in `aws/`;
- the raw results at e6cf4aa: `bench/poc/ZOE-150/results/aws/run.jsonl`, 236 runs, with the log, the machine, the fsync, the GIL checks and the py-spy samples;
- the table at 36ba912: `terms.py`.

## Results

The ratio is poc ops/s ÷ zeo ops/s. The term is the term that keeps a row under 3× (see `## Cells under 3×`). A row at 3× or above has `-`.

| scenario | mode | concurrency | object size | python | zeo ops/s | poc ops/s | ratio | term |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| add | threads | 1 | 1 KB | 3.13 | 11,049 | 20,121 | 1.82 | commit latency (finish 1.3 ms) |
| add | threads | 4 | 1 KB | 3.13 | 10,036 | 30,275 | 3.02 | - |
| add | threads | 16 | 1 KB | 3.13 | 9,950 | 27,054 | 2.72 | ZODB floor (2.8x) |
| add | threads | 64 | 1 KB | 3.13 | 9,707 | 25,668 | 2.64 | ZODB floor (2.5x) |
| add | threads | 1 | 100 KB | 3.13 | 2,695 | 1,601 | 0.59 | 100 KB commit (finish 9 ms) |
| add | threads | 4 | 100 KB | 3.13 | 2,598 | 4,315 | 1.66 | 100 KB commit (finish 15 ms) |
| add | threads | 16 | 100 KB | 3.13 | 2,495 | 4,500 | 1.80 | 100 KB commit (finish 56 ms) |
| add | threads | 64 | 100 KB | 3.13 | 2,458 | 5,423 | 2.21 | 100 KB commit (finish 89 ms) |
| add | processes | 4 | 1 KB | 3.13 | 23,867 | 35,774 | 1.50 | ZODB floor (2.7x) |
| add | processes | 16 | 1 KB | 3.13 | 21,631 | 31,395 | 1.45 | ZODB floor (2.3x) |
| add | processes | 64 | 1 KB | 3.13 | 21,073 | 29,753 | 1.41 | ZODB floor (2.2x) |
| add | processes | 4 | 100 KB | 3.13 | 1,905 | 2,215 | 1.16 | ZODB floor (1.8x) |
| add | processes | 16 | 100 KB | 3.13 | 1,296 | 1,452 | 1.12 | ZODB floor (1.3x) |
| add | processes | 64 | 100 KB | 3.13 | 921 | 1,314 | 1.43 | ZODB floor (1.9x) |
| update | threads | 1 | 1 KB | 3.13 | 8,685 | 17,268 | 1.99 | commit latency (finish 1.3 ms) |
| update | threads | 4 | 1 KB | 3.13 | 7,159 | 20,842 | 2.91 | commit (finish 2.0 ms) |
| update | threads | 16 | 1 KB | 3.13 | 4,221 | 12,119 | 2.87 | ZODB floor (2.1x) |
| update | threads | 64 | 1 KB | 3.13 | 4,181 | 12,128 | 2.90 | ZODB floor (2.2x) |
| update | threads | 1 | 100 KB | 3.13 | 2,314 | 1,566 | 0.68 | 100 KB commit (finish 9 ms) |
| update | threads | 4 | 100 KB | 3.13 | 2,118 | 4,472 | 2.11 | 100 KB commit (finish 15 ms) |
| update | threads | 16 | 100 KB | 3.13 | 1,522 | 3,257 | 2.14 | ZODB floor (2.8x) |
| update | threads | 64 | 100 KB | 3.13 | 1,512 | 3,869 | 2.56 | ZODB floor (2.7x) |
| update | processes | 4 | 1 KB | 3.13 | 16,230 | 28,460 | 1.75 | commit (finish 2.5 ms) |
| update | processes | 16 | 1 KB | 3.13 | 10,724 | 27,560 | 2.57 | commit (finish 5.9 ms) |
| update | processes | 64 | 1 KB | 3.13 | 13,036 | 34,420 | 2.64 | commit (finish 19.5 ms) |
| update | processes | 4 | 100 KB | 3.13 | 2,659 | 4,154 | 1.56 | 100 KB commit (finish 12 ms) |
| update | processes | 16 | 100 KB | 3.13 | 1,807 | 3,358 | 1.86 | 100 KB commit (finish 46 ms) |
| update | processes | 64 | 100 KB | 3.13 | 1,826 | 3,711 | 2.03 | 100 KB commit (finish 245 ms) |
| read_cold | threads | 1 | 1 KB | 3.13 | 5,296 | 9,739 | 1.84 | load round trip |
| read_cold | threads | 4 | 1 KB | 3.13 | 6,913 | 24,600 | 3.56 | - |
| read_cold | threads | 16 | 1 KB | 3.13 | 7,852 | 25,132 | 3.20 | - |
| read_cold | threads | 64 | 1 KB | 3.13 | 8,381 | 29,381 | 3.51 | - |
| read_cold | threads | 1 | 100 KB | 3.13 | 3,226 | 3,456 | 1.07 | 100 KB load |
| read_cold | threads | 4 | 100 KB | 3.13 | 4,245 | 9,491 | 2.24 | 100 KB load |
| read_cold | threads | 16 | 100 KB | 3.13 | 3,970 | 11,269 | 2.84 | ZODB floor (2.1x) |
| read_cold | threads | 64 | 100 KB | 3.13 | 4,324 | 11,398 | 2.64 | ZODB floor (1.7x) |
| read_cold | processes | 4 | 1 KB | 3.13 | 17,114 | 27,515 | 1.61 | load round trip |
| read_cold | processes | 16 | 1 KB | 3.13 | 19,965 | 54,201 | 2.71 | load round trip |
| read_cold | processes | 64 | 1 KB | 3.13 | 32,457 | 85,881 | 2.65 | load round trip |
| read_cold | processes | 4 | 100 KB | 3.13 | 9,144 | 9,454 | 1.03 | 100 KB load |
| read_cold | processes | 16 | 100 KB | 3.13 | 8,576 | 14,885 | 1.74 | 100 KB load |
| read_cold | processes | 64 | 100 KB | 3.13 | 4,163 | 14,310 | 3.44 | - |
| read_warm | threads | 1 | 1 KB | 3.13 | 3,430 | 6,343 | 1.85 | commit latency (finish 1.3 ms) |
| read_warm | threads | 4 | 1 KB | 3.13 | 3,964 | 11,690 | 2.95 | commit (finish 2.0 ms) |
| read_warm | threads | 16 | 1 KB | 3.13 | 4,332 | 11,337 | 2.62 | ZODB floor (2.2x) |
| read_warm | threads | 64 | 1 KB | 3.13 | 4,182 | 12,481 | 2.98 | ZODB floor (2.0x) |
| read_warm | threads | 1 | 100 KB | 3.13 | 1,392 | 1,076 | 0.77 | 100 KB commit (finish 9 ms) |
| read_warm | threads | 4 | 100 KB | 3.13 | 1,508 | 3,100 | 2.06 | 100 KB commit (finish 15 ms) |
| read_warm | threads | 16 | 100 KB | 3.13 | 1,705 | 3,497 | 2.05 | ZODB floor (2.6x) |
| read_warm | threads | 64 | 100 KB | 3.13 | 1,478 | 3,861 | 2.61 | ZODB floor (2.7x) |
| read_warm | processes | 4 | 1 KB | 3.13 | 7,496 | 15,739 | 2.10 | commit (finish 2.5 ms) |
| read_warm | processes | 16 | 1 KB | 3.13 | 10,420 | 23,621 | 2.27 | commit (finish 5.9 ms) |
| read_warm | processes | 64 | 1 KB | 3.13 | 12,056 | 28,284 | 2.35 | commit (finish 19.5 ms) |
| read_warm | processes | 4 | 100 KB | 3.13 | 1,917 | 2,970 | 1.55 | 100 KB commit (finish 12 ms) |
| read_warm | processes | 16 | 100 KB | 3.13 | 1,742 | 3,463 | 1.99 | 100 KB commit (finish 46 ms) |
| read_warm | processes | 64 | 100 KB | 3.13 | 1,049 | 3,700 | 3.53 | - |
| read_hot | threads | 1 | 1 KB | 3.13 | 62,777 | 9,761 | 0.155 | ZODB per hit (1.0x) |
| read_hot | threads | 4 | 1 KB | 3.13 | 27,885 | 25,973 | 0.93 | ZODB per hit (2.3x) |
| read_hot | threads | 16 | 1 KB | 3.13 | 25,936 | 27,268 | 1.05 | ZODB per hit (2.4x) |
| read_hot | threads | 64 | 1 KB | 3.13 | 13,798 | 24,634 | 1.79 | client cache |
| read_hot | threads | 1 | 100 KB | 3.13 | 16,180 | 3,604 | 0.223 | ZODB per hit (1.0x) |
| read_hot | threads | 4 | 100 KB | 3.13 | 4,434 | 9,410 | 2.12 | client cache |
| read_hot | threads | 16 | 100 KB | 3.13 | 5,095 | 10,765 | 2.11 | client cache |
| read_hot | threads | 64 | 100 KB | 3.13 | 4,498 | 10,552 | 2.35 | client cache |
| read_hot | processes | 4 | 1 KB | 3.13 | 121,369 | 29,344 | 0.242 | ZODB per hit (2.1x) |
| read_hot | processes | 16 | 1 KB | 3.13 | 427,639 | 53,423 | 0.125 | ZODB per hit (1.8x) |
| read_hot | processes | 64 | 1 KB | 3.13 | 456,378 | 56,818 | 0.124 | ZODB per hit (1.7x) |
| read_hot | processes | 4 | 100 KB | 3.13 | 33,080 | 9,697 | 0.293 | ZODB per hit (2.0x) |
| read_hot | processes | 16 | 100 KB | 3.13 | 129,618 | 14,256 | 0.110 | ZODB per hit (1.5x) |
| read_hot | processes | 64 | 100 KB | 3.13 | 122,078 | 12,829 | 0.105 | ZODB per hit (1.6x) |
| add | threads | 1 | 1 KB | 3.14t | 15,692 | 19,858 | 1.27 | commit latency (finish 1.3 ms) |
| add | threads | 4 | 1 KB | 3.14t | 14,354 | 48,596 | 3.39 | - |
| add | threads | 16 | 1 KB | 3.14t | 13,949 | 39,365 | 2.82 | commit (finish 10.1 ms) |
| add | threads | 64 | 1 KB | 3.14t | 13,124 | 50,770 | 3.87 | - |
| add | threads | 1 | 100 KB | 3.14t | 2,395 | 1,581 | 0.66 | 100 KB commit (finish 9 ms) |
| add | threads | 4 | 100 KB | 3.14t | 2,230 | 4,342 | 1.95 | 100 KB commit (finish 14 ms) |
| add | threads | 16 | 100 KB | 3.14t | 2,291 | 3,832 | 1.67 | 100 KB commit (finish 47 ms) |
| add | threads | 64 | 100 KB | 3.14t | 2,359 | 5,340 | 2.26 | 100 KB commit (finish 102 ms) |
| add | processes | 4 | 1 KB | 3.14t | 21,929 | 34,289 | 1.56 | ZODB floor (2.7x) |
| add | processes | 16 | 1 KB | 3.14t | 21,430 | 29,844 | 1.39 | ZODB floor (2.1x) |
| add | processes | 64 | 1 KB | 3.14t | 18,018 | 32,316 | 1.79 | ZODB floor (2.9x) |
| add | processes | 4 | 100 KB | 3.14t | 1,871 | 2,031 | 1.09 | ZODB floor (1.8x) |
| add | processes | 16 | 100 KB | 3.14t | 1,277 | 1,410 | 1.10 | ZODB floor (1.2x) |
| add | processes | 64 | 100 KB | 3.14t | 1,127 | 1,245 | 1.10 | ZODB floor (1.4x) |
| update | threads | 1 | 1 KB | 3.14t | 9,680 | 16,359 | 1.69 | commit latency (finish 1.3 ms) |
| update | threads | 4 | 1 KB | 3.14t | 7,913 | 25,470 | 3.22 | - |
| update | threads | 16 | 1 KB | 3.14t | 4,346 | 20,250 | 4.66 | - |
| update | threads | 64 | 1 KB | 3.14t | 4,428 | 19,899 | 4.49 | - |
| update | threads | 1 | 100 KB | 3.14t | 1,939 | 1,562 | 0.81 | 100 KB commit (finish 9 ms) |
| update | threads | 4 | 100 KB | 3.14t | 1,811 | 3,992 | 2.20 | 100 KB commit (finish 14 ms) |
| update | threads | 16 | 100 KB | 3.14t | 1,427 | 3,550 | 2.49 | 100 KB commit (finish 47 ms) |
| update | threads | 64 | 100 KB | 3.14t | 1,418 | 3,719 | 2.62 | 100 KB commit (finish 102 ms) |
| update | processes | 4 | 1 KB | 3.14t | 13,917 | 29,700 | 2.13 | commit (finish 2.1 ms) |
| update | processes | 16 | 1 KB | 3.14t | 9,710 | 30,206 | 3.11 | - |
| update | processes | 64 | 1 KB | 3.14t | 7,536 | 49,183 | 6.53 | - |
| update | processes | 4 | 100 KB | 3.14t | 2,459 | 4,305 | 1.75 | 100 KB commit (finish 12 ms) |
| update | processes | 16 | 100 KB | 3.14t | 1,196 | 3,425 | 2.86 | 100 KB commit (finish 51 ms) |
| update | processes | 64 | 100 KB | 3.14t | 880 | 3,854 | 4.38 | - |
| read_cold | threads | 1 | 1 KB | 3.14t | 5,192 | 9,439 | 1.82 | load round trip |
| read_cold | threads | 4 | 1 KB | 3.14t | 7,529 | 25,744 | 3.42 | - |
| read_cold | threads | 16 | 1 KB | 3.14t | 7,057 | 37,229 | 5.28 | - |
| read_cold | threads | 64 | 1 KB | 3.14t | 8,764 | 38,142 | 4.35 | - |
| read_cold | threads | 1 | 100 KB | 3.14t | 2,823 | 3,406 | 1.21 | 100 KB load |
| read_cold | threads | 4 | 100 KB | 3.14t | 4,131 | 9,044 | 2.19 | 100 KB load |
| read_cold | threads | 16 | 100 KB | 3.14t | 4,462 | 11,515 | 2.58 | 100 KB load |
| read_cold | threads | 64 | 100 KB | 3.14t | 4,714 | 11,330 | 2.40 | 100 KB load |
| read_cold | processes | 4 | 1 KB | 3.14t | 13,562 | 26,630 | 1.96 | load round trip |
| read_cold | processes | 16 | 1 KB | 3.14t | 15,290 | 55,661 | 3.64 | - |
| read_cold | processes | 64 | 1 KB | 3.14t | 14,001 | 154,142 | 11.01 | - |
| read_cold | processes | 4 | 100 KB | 3.14t | 8,187 | 8,974 | 1.10 | 100 KB load |
| read_cold | processes | 16 | 100 KB | 3.14t | 6,537 | 14,950 | 2.29 | 100 KB load |
| read_cold | processes | 64 | 100 KB | 3.14t | 2,765 | 14,832 | 5.36 | - |
| read_warm | threads | 1 | 1 KB | 3.14t | 3,463 | 6,268 | 1.81 | commit latency (finish 1.3 ms) |
| read_warm | threads | 4 | 1 KB | 3.14t | 4,132 | 15,053 | 3.64 | - |
| read_warm | threads | 16 | 1 KB | 3.14t | 4,335 | 17,717 | 4.09 | - |
| read_warm | threads | 64 | 1 KB | 3.14t | 4,365 | 17,981 | 4.12 | - |
| read_warm | threads | 1 | 100 KB | 3.14t | 1,107 | 1,051 | 0.95 | 100 KB commit (finish 9 ms) |
| read_warm | threads | 4 | 100 KB | 3.14t | 1,370 | 2,371 | 1.73 | 100 KB commit (finish 14 ms) |
| read_warm | threads | 16 | 100 KB | 3.14t | 1,395 | 3,622 | 2.60 | 100 KB commit (finish 47 ms) |
| read_warm | threads | 64 | 100 KB | 3.14t | 1,460 | 3,687 | 2.52 | 100 KB commit (finish 102 ms) |
| read_warm | processes | 4 | 1 KB | 3.14t | 6,866 | 16,712 | 2.43 | commit (finish 2.1 ms) |
| read_warm | processes | 16 | 1 KB | 3.14t | 9,600 | 25,963 | 2.70 | commit (finish 4.3 ms) |
| read_warm | processes | 64 | 1 KB | 3.14t | 8,620 | 38,242 | 4.44 | - |
| read_warm | processes | 4 | 100 KB | 3.14t | 1,850 | 2,919 | 1.58 | 100 KB commit (finish 12 ms) |
| read_warm | processes | 16 | 100 KB | 3.14t | 981 | 3,557 | 3.63 | - |
| read_warm | processes | 64 | 100 KB | 3.14t | 956 | 3,750 | 3.92 | - |
| read_hot | threads | 1 | 1 KB | 3.14t | 60,436 | 9,849 | 0.163 | ZODB per hit (1.0x) |
| read_hot | threads | 4 | 1 KB | 3.14t | 80,108 | 25,684 | 0.321 | client cache |
| read_hot | threads | 16 | 1 KB | 3.14t | 34,290 | 38,078 | 1.11 | client cache |
| read_hot | threads | 64 | 1 KB | 3.14t | 14,372 | 38,406 | 2.67 | client cache |
| read_hot | threads | 1 | 100 KB | 3.14t | 27,152 | 3,572 | 0.132 | ZODB per hit (1.0x) |
| read_hot | threads | 4 | 100 KB | 3.14t | 4,716 | 9,977 | 2.12 | client cache |
| read_hot | threads | 16 | 100 KB | 3.14t | 4,651 | 11,683 | 2.51 | client cache |
| read_hot | threads | 64 | 100 KB | 3.14t | 4,626 | 11,453 | 2.48 | client cache |
| read_hot | processes | 4 | 1 KB | 3.14t | 128,522 | 28,447 | 0.221 | ZODB per hit (1.9x) |
| read_hot | processes | 16 | 1 KB | 3.14t | 395,100 | 52,906 | 0.134 | ZODB per hit (1.8x) |
| read_hot | processes | 64 | 1 KB | 3.14t | 875,325 | 74,489 | 0.085 | ZODB per hit (0.8x) |
| read_hot | processes | 4 | 100 KB | 3.14t | 77,745 | 9,399 | 0.121 | ZODB per hit (1.4x) |
| read_hot | processes | 16 | 100 KB | 3.14t | 154,899 | 14,282 | 0.092 | ZODB per hit (2.1x) |
| read_hot | processes | 64 | 100 KB | 3.14t | 5,692 | 12,443 | 2.19 | client cache |

## Floors

The floors are the terms that no change of the client or the server removes. All come from the same instance.

**fsync of the NVMe of the instance** (`fsyncx.py`, the median of 40 calls):

| bytes written before the sync | `fdatasync` | `fsync` |
| --- | --- | --- |
| 4 KiB, sequential | 0.13 ms | 0.14 ms |
| 64 KiB, sequential | 0.18 ms | 0.20 ms |
| 288 KiB, sequential | 0.32 ms | 0.37 ms |
| 288 KiB, scattered pages | 1.12 ms | 1.08 ms |
| 800 KiB, sequential | 0.71 ms | 0.83 ms |
| 800 KiB, scattered pages | 2.64 ms | 2.35 ms |

A sync is cheap on this disk: a commit of 100 records of 1 KB pays about 0.3 ms of sync, and `tpc_finish` of `zoe serve` takes 1.3 ms at concurrency 1 (see `## Server work`). On the workstation disk of the first plan, a sync of 4 KiB took 9 ms.

**Round trip** (`rtt.py`, the median of 10 batches of 100 calls; wall time per call, then the client CPU per call):

| call | 3.13 | 3.14t |
| --- | --- | --- |
| loopback TCP between two Python processes | 20 µs, 9 µs | 20 µs, 9 µs |
| `ZEO.ClientStorage`, `new_oids(1)` on the ZEO server | 135 µs, 81 µs | 143 µs, 96 µs |
| `ZEO.ClientStorage`, `ping` on the ZEO server | 132 µs, 79 µs | 139 µs, 95 µs |
| POC client, `new_oids(1)` on `zoe serve` | 70 µs, 32 µs | 71 µs, 33 µs |

So one call through the POC client costs half the time of a call through `ZEO.ClientStorage`, and 50 µs more than the loopback round trip.

**Client CPU per object of each client.** This is the client CPU of a run minus the client CPU of the floor run of the same cell, divided by the objects of the run. It is the mean of the five scenarios, so it mixes stores and loads. It comes from the threads cells: in the processes cells the floor opens its FileStorage again in each process, and on 3.14t the CPU of the worker processes is not counted (see Method).

| client | 1 KB, 3.13 | 1 KB, 3.14t | 100 KB, 3.13 | 100 KB, 3.14t |
| --- | --- | --- | --- | --- |
| `ZEO.ClientStorage` | 57 to 66 µs per object | 63 to 86 µs per object | 202 to 227 µs per object | 227 to 316 µs per object |
| POC client | 14 to 28 µs per object | -15 to 4 µs per object, within the noise | 29 to 52 µs per object | 45 to 67 µs per object |

**ZODB per object** (the floor, threads, concurrency 1: the time of one transaction ÷ 100):

| scenario | 1 KB, 3.13 | 1 KB, 3.14t | 100 KB, 3.13 | 100 KB, 3.14t |
| --- | --- | --- | --- | --- |
| `add` | 29 µs | 35 µs | 86 µs | 93 µs |
| `update` | 23 µs | 32 µs | 48 µs | 64 µs |
| `read_cold` | 21 µs | 28 µs | 39 µs | 50 µs |
| `read_hot` | 20 µs | 26 µs | 42 µs | 44 µs |

**What the floors mean.** At concurrency 1 a ZEO transaction of `add` with 1 KB objects takes 9.1 ms on 3.13, so 3× needs 3.0 ms. ZODB and zodbshootout alone take 2.9 ms of it, and this leaves 0.1 ms for the client storage, the wire and the server. In 24 rows the floor is above the time that 3× allows: the in-process storage reaches only 1.1 to 2.9× ZEO. These are:

- `add` in every processes cell, on both builds;
- `add`, `update` and `read_warm` in the threads cells at concurrency 16 and 64 on 3.13, where one GIL runs ZODB for every thread;
- `read_cold` at 100 KB in the threads cells at concurrency 16 and 64 on 3.13.

On 3.14t the floor of the threads cells does not bound the POC client: the DemoStorage commits one transaction at a time, and the pool of the POC client commits in parallel. So the rule leaves those rows out.

## The client cache

**The share of `ZEO.cache` in a `read_hot` hit.** py-spy sampled zodbshootout with `ZEO.ClientStorage` on 3.13, threads, 1 KB objects, during the `read_hot` loop:

- **Concurrency 16:** 96 % of the 1,177 samples of the loop are in `ZEO/cache.py`, under `ClientStorage.loadBefore`. With 16 threads, the lock of the cache is where the threads wait.
- **Concurrency 1:** 0.5 % of the 212 samples are in `ClientStorage.loadBefore`. ZODB and persistent take 94 %.

So one hit at concurrency 1 is ZODB work: 15.9 µs on 3.13 and 16.5 µs on 3.14t at 1 KB, 62 µs and 37 µs at 100 KB. py-spy does not read a free-threaded 3.14 process, so the profile exists for 3.13 only.

**The cost of a hit that 3× allows.** A cell at 3× serves three times the hits per second of ZEO. With the client cores of the cell, this gives the time of one hit on one core. The client has one core under the GIL of 3.13 in the threads mode, and else the concurrency, at most the 12 client vCPUs. That time minus the ZODB time of a hit is what the cache may cost:

| mode | concurrency | object size | python | zeo hits/s | 3× hits/s | client cores | ZODB per hit | time per hit at 3×, per core | left for the cache |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| threads | 4 | 1 KB | 3.13 | 27,885 | 83,656 | 1 | 16 µs | 12 µs | none |
| threads | 16 | 1 KB | 3.13 | 25,936 | 77,810 | 1 | 16 µs | 13 µs | none |
| threads | 64 | 1 KB | 3.13 | 13,798 | 41,394 | 1 | 16 µs | 24 µs | 8 µs |
| threads | 4 | 100 KB | 3.13 | 4,434 | 13,300 | 1 | 62 µs | 75 µs | 13 µs |
| threads | 16 | 100 KB | 3.13 | 5,095 | 15,286 | 1 | 62 µs | 65 µs | 4 µs |
| threads | 64 | 100 KB | 3.13 | 4,498 | 13,496 | 1 | 62 µs | 74 µs | 12 µs |
| processes | 4 | 1 KB | 3.13 | 121,369 | 364,107 | 4 | 16 µs | 11 µs | none |
| processes | 16 | 1 KB | 3.13 | 427,639 | 1,282,918 | 12 | 16 µs | 9 µs | none |
| processes | 64 | 1 KB | 3.13 | 456,378 | 1,369,134 | 12 | 16 µs | 9 µs | none |
| processes | 4 | 100 KB | 3.13 | 33,080 | 99,240 | 4 | 62 µs | 40 µs | none |
| processes | 16 | 100 KB | 3.13 | 129,618 | 388,855 | 12 | 62 µs | 31 µs | none |
| processes | 64 | 100 KB | 3.13 | 122,078 | 366,233 | 12 | 62 µs | 33 µs | none |
| threads | 4 | 1 KB | 3.14t | 80,108 | 240,326 | 4 | 17 µs | 17 µs | about 0 |
| threads | 16 | 1 KB | 3.14t | 34,290 | 102,869 | 12 | 17 µs | 117 µs | 100 µs |
| threads | 64 | 1 KB | 3.14t | 14,372 | 43,114 | 12 | 17 µs | 278 µs | 262 µs |
| threads | 4 | 100 KB | 3.14t | 4,716 | 14,147 | 4 | 37 µs | 283 µs | 246 µs |
| threads | 16 | 100 KB | 3.14t | 4,651 | 13,952 | 12 | 37 µs | 860 µs | 823 µs |
| threads | 64 | 100 KB | 3.14t | 4,626 | 13,880 | 12 | 37 µs | 865 µs | 828 µs |
| processes | 4 | 1 KB | 3.14t | 128,522 | 385,566 | 4 | 17 µs | 10 µs | none |
| processes | 16 | 1 KB | 3.14t | 395,100 | 1,185,299 | 12 | 17 µs | 10 µs | none |
| processes | 64 | 1 KB | 3.14t | 875,325 | 2,625,976 | 12 | 17 µs | 5 µs | none |
| processes | 4 | 100 KB | 3.14t | 77,745 | 233,235 | 4 | 37 µs | 17 µs | none |
| processes | 16 | 100 KB | 3.14t | 154,899 | 464,697 | 12 | 37 µs | 26 µs | none |
| processes | 64 | 100 KB | 3.14t | 5,692 | 17,074 | 12 | 37 µs | 703 µs | 666 µs |

- **No cache can give 3× in 13 of these rows:** every processes cell but one, and the threads cells of 1 KB at concurrency 4 and 16 on 3.13. There the time of a hit at 3× is below the ZODB time of a hit, because ZEO.cache is already cheap or because the GIL holds ZODB to one core. With the 4 rows at concurrency 1, this makes the 17 `read_hot` rows that no card can move.
- **In the 11 other rows, a hit may cost 4 to 13 µs on 3.13** (threads, 100 KB and concurrency 64 at 1 KB) and **0 to 830 µs on 3.14t** (threads, and processes at concurrency 64 with 100 KB). In these rows ZEO.cache misses or waits on its lock. The `processes, 64, 100 KB, 3.14t` row has a zeo spread of 320 %: a ZEO.cache of 20 MB holds the 100 objects of 100 KB of a worker on some rounds only.

ZOE-156 (the cache of one process) and ZOE-162 (the persistent cache) move these 11 rows. For the 17 bounded rows they can only bring parity: about 1× ZEO, against 0.09 to 0.3× now.

## Cells under 3×

114 of the 140 rows are under 3×. Each row has one term, from these rules, in this order (`terms.py`):

1. **`read_hot`.** The POC client has no cache, so each hit goes to the server. If the client cores cannot run 3× the hits of ZEO at the ZODB time of a hit, the term is *ZODB per hit* and no card can move the row. Else the term is the *client cache*.
2. **ZODB floor.** If the in-process storage reaches less than 3× ZEO, no card can move the row. This rule does not apply to the threads rows of 3.14t (see `## Floors`).
3. **Commits** (`add`, `update`, `read_warm`). At 100 KB the term is the *100 KB commit*. At 1 KB it is the *commit latency* at concurrency 1 and the *commit* above. Each gives the median `tpc_finish` of the server.
4. **Loads** (`read_cold`). At 100 KB the term is the *100 KB load*. At 1 KB it is the *load round trip*.

| term | rows | card |
| --- | --- | --- |
| ZODB floor | 24 (18 on 3.13, 6 on 3.14t) | no card can: ZODB and zodbshootout alone reach less than 3× ZEO |
| ZODB per hit | 17 (10, 7) | no card can: ZODB alone takes the time of a hit |
| 100 KB commit | 28 (13, 15) | new card: compression off the commit path, on a shared pool of threads |
| commit, commit latency | 18 (11, 7) | new card: a cheaper redb finish; at concurrency above 1 also the compression card |
| 100 KB load | 10 (4, 6) | new card: the read path of the server |
| load round trip | 6 (4, 2) | new card: the read path of the server; ZOE-153 for the client side |
| client cache | 11 (4, 7) | ZOE-156; in the processes mode also ZOE-162 |

The table gives every row under 3×. "floor / zeo" is the ops/s of the in-process storage ÷ the ops/s of ZEO. The spread is that of the three rounds of each side.

| scenario | mode | concurrency | object size | python | ratio | floor / zeo | spread zeo / poc | term | card |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| add | threads | 1 | 1 KB | 3.13 | 1.82 | 3.1 | 2 % / 2 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| add | threads | 16 | 1 KB | 3.13 | 2.72 | 2.8 | 11 % / 2 % | ZODB floor (2.8x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | threads | 64 | 1 KB | 3.13 | 2.64 | 2.5 | 1 % / 1 % | ZODB floor (2.5x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | threads | 1 | 100 KB | 3.13 | 0.59 | 4.3 | 4 % / 3 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| add | threads | 4 | 100 KB | 3.13 | 1.66 | 4.5 | 14 % / 32 % | 100 KB commit (finish 15 ms) | new card: compression off the commit path |
| add | threads | 16 | 100 KB | 3.13 | 1.80 | 4.3 | 8 % / 4 % | 100 KB commit (finish 56 ms) | new card: compression off the commit path |
| add | threads | 64 | 100 KB | 3.13 | 2.21 | 4.3 | 4 % / 3 % | 100 KB commit (finish 89 ms) | new card: compression off the commit path |
| add | processes | 4 | 1 KB | 3.13 | 1.50 | 2.7 | 4 % / 11 % | ZODB floor (2.7x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 16 | 1 KB | 3.13 | 1.45 | 2.3 | 39 % / 2 % | ZODB floor (2.3x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 64 | 1 KB | 3.13 | 1.41 | 2.2 | 15 % / 2 % | ZODB floor (2.2x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 4 | 100 KB | 3.13 | 1.16 | 1.8 | 9 % / 3 % | ZODB floor (1.8x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 16 | 100 KB | 3.13 | 1.12 | 1.3 | 7 % / 3 % | ZODB floor (1.3x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 64 | 100 KB | 3.13 | 1.43 | 1.9 | 5 % / 1 % | ZODB floor (1.9x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | threads | 1 | 1 KB | 3.13 | 1.99 | 5.0 | 1 % / 2 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| update | threads | 4 | 1 KB | 3.13 | 2.91 | 3.5 | 1 % / 7 % | commit (finish 2.0 ms) | new card: compression off the commit path, a cheaper redb finish |
| update | threads | 16 | 1 KB | 3.13 | 2.87 | 2.1 | 4 % / 9 % | ZODB floor (2.1x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | threads | 64 | 1 KB | 3.13 | 2.90 | 2.2 | 4 % / 8 % | ZODB floor (2.2x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | threads | 1 | 100 KB | 3.13 | 0.68 | 9.0 | 4 % / 7 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| update | threads | 4 | 100 KB | 3.13 | 2.11 | 5.4 | 2 % / 0 % | 100 KB commit (finish 15 ms) | new card: compression off the commit path |
| update | threads | 16 | 100 KB | 3.13 | 2.14 | 2.8 | 2 % / 5 % | ZODB floor (2.8x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | threads | 64 | 100 KB | 3.13 | 2.56 | 2.7 | 5 % / 6 % | ZODB floor (2.7x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | processes | 4 | 1 KB | 3.13 | 1.75 | 7.8 | 23 % / 7 % | commit (finish 2.5 ms) | new card: compression off the commit path, a cheaper redb finish |
| update | processes | 16 | 1 KB | 3.13 | 2.57 | 11.7 | 8 % / 5 % | commit (finish 5.9 ms) | new card: compression off the commit path, a cheaper redb finish |
| update | processes | 64 | 1 KB | 3.13 | 2.64 | 11.7 | 21 % / 6 % | commit (finish 19.5 ms) | new card: compression off the commit path, a cheaper redb finish |
| update | processes | 4 | 100 KB | 3.13 | 1.56 | 13.9 | 17 % / 15 % | 100 KB commit (finish 12 ms) | new card: compression off the commit path |
| update | processes | 16 | 100 KB | 3.13 | 1.86 | 22.8 | 23 % / 9 % | 100 KB commit (finish 46 ms) | new card: compression off the commit path |
| update | processes | 64 | 100 KB | 3.13 | 2.03 | 21.7 | 43 % / 1 % | 100 KB commit (finish 245 ms) | new card: compression off the commit path |
| read_cold | threads | 1 | 1 KB | 3.13 | 1.84 | 9.2 | 1 % / 4 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | threads | 1 | 100 KB | 3.13 | 1.07 | 7.9 | 2 % / 12 % | 100 KB load | new card: the read path of the server |
| read_cold | threads | 4 | 100 KB | 3.13 | 2.24 | 3.2 | 3 % / 2 % | 100 KB load | new card: the read path of the server |
| read_cold | threads | 16 | 100 KB | 3.13 | 2.84 | 2.1 | 25 % / 11 % | ZODB floor (2.1x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_cold | threads | 64 | 100 KB | 3.13 | 2.64 | 1.7 | 6 % / 6 % | ZODB floor (1.7x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_cold | processes | 4 | 1 KB | 3.13 | 1.61 | 5.6 | 13 % / 6 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | processes | 16 | 1 KB | 3.13 | 2.71 | 14.1 | 8 % / 5 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | processes | 64 | 1 KB | 3.13 | 2.65 | 10.1 | 34 % / 16 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | processes | 4 | 100 KB | 3.13 | 1.03 | 3.8 | 3 % / 6 % | 100 KB load | new card: the read path of the server |
| read_cold | processes | 16 | 100 KB | 3.13 | 1.74 | 8.6 | 75 % / 5 % | 100 KB load | new card: the read path of the server |
| read_warm | threads | 1 | 1 KB | 3.13 | 1.85 | 5.7 | 4 % / 3 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| read_warm | threads | 4 | 1 KB | 3.13 | 2.95 | 4.8 | 4 % / 15 % | commit (finish 2.0 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | threads | 16 | 1 KB | 3.13 | 2.62 | 2.2 | 2 % / 1 % | ZODB floor (2.2x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_warm | threads | 64 | 1 KB | 3.13 | 2.98 | 2.0 | 4 % / 12 % | ZODB floor (2.0x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_warm | threads | 1 | 100 KB | 3.13 | 0.77 | 6.3 | 9 % / 8 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| read_warm | threads | 4 | 100 KB | 3.13 | 2.06 | 3.8 | 2 % / 5 % | 100 KB commit (finish 15 ms) | new card: compression off the commit path |
| read_warm | threads | 16 | 100 KB | 3.13 | 2.05 | 2.6 | 6 % / 10 % | ZODB floor (2.6x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_warm | threads | 64 | 100 KB | 3.13 | 2.61 | 2.7 | 5 % / 3 % | ZODB floor (2.7x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| read_warm | processes | 4 | 1 KB | 3.13 | 2.10 | 8.6 | 17 % / 15 % | commit (finish 2.5 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | processes | 16 | 1 KB | 3.13 | 2.27 | 9.8 | 8 % / 4 % | commit (finish 5.9 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | processes | 64 | 1 KB | 3.13 | 2.35 | 9.7 | 9 % / 9 % | commit (finish 19.5 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | processes | 4 | 100 KB | 3.13 | 1.55 | 10.5 | 12 % / 4 % | 100 KB commit (finish 12 ms) | new card: compression off the commit path |
| read_warm | processes | 16 | 100 KB | 3.13 | 1.99 | 21.5 | 16 % / 5 % | 100 KB commit (finish 46 ms) | new card: compression off the commit path |
| read_hot | threads | 1 | 1 KB | 3.13 | 0.155 | 0.8 | 3 % / 6 % | ZODB per hit (1.0x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 4 | 1 KB | 3.13 | 0.93 | 0.9 | 3 % / 10 % | ZODB per hit (2.3x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 16 | 1 KB | 3.13 | 1.05 | 0.6 | 5 % / 12 % | ZODB per hit (2.4x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 64 | 1 KB | 3.13 | 1.79 | 1.0 | 2 % / 7 % | client cache | ZOE-156 |
| read_hot | threads | 1 | 100 KB | 3.13 | 0.223 | 1.5 | 2 % / 10 % | ZODB per hit (1.0x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 4 | 100 KB | 3.13 | 2.12 | 2.7 | 7 % / 2 % | client cache | ZOE-156 |
| read_hot | threads | 16 | 100 KB | 3.13 | 2.11 | 2.1 | 4 % / 4 % | client cache | ZOE-156 |
| read_hot | threads | 64 | 100 KB | 3.13 | 2.35 | 2.1 | 4 % / 3 % | client cache | ZOE-156 |
| read_hot | processes | 4 | 1 KB | 3.13 | 0.242 | 1.6 | 104 % / 16 % | ZODB per hit (2.1x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 16 | 1 KB | 3.13 | 0.125 | 0.8 | 17 % / 1 % | ZODB per hit (1.8x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 64 | 1 KB | 3.13 | 0.124 | 0.6 | 14 % / 1 % | ZODB per hit (1.7x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 4 | 100 KB | 3.13 | 0.293 | 1.5 | 58 % / 4 % | ZODB per hit (2.0x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 16 | 100 KB | 3.13 | 0.110 | 0.7 | 25 % / 1 % | ZODB per hit (1.5x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 64 | 100 KB | 3.13 | 0.105 | 0.8 | 53 % / 0 % | ZODB per hit (1.6x) | no card can: ZODB alone takes the time of a hit |
| add | threads | 1 | 1 KB | 3.14t | 1.27 | 1.8 | 7 % / 1 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| add | threads | 16 | 1 KB | 3.14t | 2.82 | 1.8 | 6 % / 9 % | commit (finish 10.1 ms) | new card: compression off the commit path, a cheaper redb finish |
| add | threads | 1 | 100 KB | 3.14t | 0.66 | 4.5 | 2 % / 1 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| add | threads | 4 | 100 KB | 3.14t | 1.95 | 4.8 | 5 % / 8 % | 100 KB commit (finish 14 ms) | new card: compression off the commit path |
| add | threads | 16 | 100 KB | 3.14t | 1.67 | 4.3 | 1 % / 6 % | 100 KB commit (finish 47 ms) | new card: compression off the commit path |
| add | threads | 64 | 100 KB | 3.14t | 2.26 | 4.1 | 4 % / 1 % | 100 KB commit (finish 102 ms) | new card: compression off the commit path |
| add | processes | 4 | 1 KB | 3.14t | 1.56 | 2.7 | 1 % / 5 % | ZODB floor (2.7x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 16 | 1 KB | 3.14t | 1.39 | 2.1 | 34 % / 5 % | ZODB floor (2.1x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 64 | 1 KB | 3.14t | 1.79 | 2.9 | 21 % / 54 % | ZODB floor (2.9x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 4 | 100 KB | 3.14t | 1.09 | 1.8 | 5 % / 10 % | ZODB floor (1.8x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 16 | 100 KB | 3.14t | 1.10 | 1.2 | 3 % / 3 % | ZODB floor (1.2x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| add | processes | 64 | 100 KB | 3.14t | 1.10 | 1.4 | 25 % / 2 % | ZODB floor (1.4x) | no card can: ZODB and zodbshootout alone reach less than 3x ZEO |
| update | threads | 1 | 1 KB | 3.14t | 1.69 | 3.3 | 0 % / 1 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| update | threads | 1 | 100 KB | 3.14t | 0.81 | 8.1 | 2 % / 0 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| update | threads | 4 | 100 KB | 3.14t | 2.20 | 7.3 | 1 % / 17 % | 100 KB commit (finish 14 ms) | new card: compression off the commit path |
| update | threads | 16 | 100 KB | 3.14t | 2.49 | 5.4 | 2 % / 1 % | 100 KB commit (finish 47 ms) | new card: compression off the commit path |
| update | threads | 64 | 100 KB | 3.14t | 2.62 | 5.0 | 1 % / 1 % | 100 KB commit (finish 102 ms) | new card: compression off the commit path |
| update | processes | 4 | 1 KB | 3.14t | 2.13 | 7.4 | 1 % / 4 % | commit (finish 2.1 ms) | new card: compression off the commit path, a cheaper redb finish |
| update | processes | 4 | 100 KB | 3.14t | 1.75 | 13.8 | 13 % / 4 % | 100 KB commit (finish 12 ms) | new card: compression off the commit path |
| update | processes | 16 | 100 KB | 3.14t | 2.86 | 28.2 | 45 % / 7 % | 100 KB commit (finish 51 ms) | new card: compression off the commit path |
| read_cold | threads | 1 | 1 KB | 3.14t | 1.82 | 6.9 | 1 % / 2 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | threads | 1 | 100 KB | 3.14t | 1.21 | 7.0 | 1 % / 11 % | 100 KB load | new card: the read path of the server |
| read_cold | threads | 4 | 100 KB | 3.14t | 2.19 | 11.9 | 4 % / 6 % | 100 KB load | new card: the read path of the server |
| read_cold | threads | 16 | 100 KB | 3.14t | 2.58 | 13.5 | 4 % / 3 % | 100 KB load | new card: the read path of the server |
| read_cold | threads | 64 | 100 KB | 3.14t | 2.40 | 16.1 | 3 % / 1 % | 100 KB load | new card: the read path of the server |
| read_cold | processes | 4 | 1 KB | 3.14t | 1.96 | 9.2 | 3 % / 4 % | load round trip | new card: the read path of the server; ZOE-153 for the client |
| read_cold | processes | 4 | 100 KB | 3.14t | 1.10 | 6.8 | 6 % / 1 % | 100 KB load | new card: the read path of the server |
| read_cold | processes | 16 | 100 KB | 3.14t | 2.29 | 11.1 | 24 % / 2 % | 100 KB load | new card: the read path of the server |
| read_warm | threads | 1 | 1 KB | 3.14t | 1.81 | 4.3 | 1 % / 2 % | commit latency (finish 1.3 ms) | new card: a cheaper redb finish |
| read_warm | threads | 1 | 100 KB | 3.14t | 0.95 | 5.0 | 2 % / 11 % | 100 KB commit (finish 9 ms) | new card: compression off the commit path |
| read_warm | threads | 4 | 100 KB | 3.14t | 1.73 | 5.0 | 5 % / 33 % | 100 KB commit (finish 14 ms) | new card: compression off the commit path |
| read_warm | threads | 16 | 100 KB | 3.14t | 2.60 | 6.2 | 2 % / 3 % | 100 KB commit (finish 47 ms) | new card: compression off the commit path |
| read_warm | threads | 64 | 100 KB | 3.14t | 2.52 | 5.0 | 1 % / 1 % | 100 KB commit (finish 102 ms) | new card: compression off the commit path |
| read_warm | processes | 4 | 1 KB | 3.14t | 2.43 | 7.3 | 32 % / 6 % | commit (finish 2.1 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | processes | 16 | 1 KB | 3.14t | 2.70 | 9.7 | 24 % / 25 % | commit (finish 4.3 ms) | new card: compression off the commit path, a cheaper redb finish |
| read_warm | processes | 4 | 100 KB | 3.14t | 1.58 | 9.7 | 9 % / 6 % | 100 KB commit (finish 12 ms) | new card: compression off the commit path |
| read_hot | threads | 1 | 1 KB | 3.14t | 0.163 | 0.6 | 3 % / 2 % | ZODB per hit (1.0x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 4 | 1 KB | 3.14t | 0.321 | 1.3 | 2 % / 8 % | client cache | ZOE-156 |
| read_hot | threads | 16 | 1 KB | 3.14t | 1.11 | 3.6 | 5 % / 4 % | client cache | ZOE-156 |
| read_hot | threads | 64 | 1 KB | 3.14t | 2.67 | 7.2 | 4 % / 3 % | client cache | ZOE-156 |
| read_hot | threads | 1 | 100 KB | 3.14t | 0.132 | 0.8 | 10 % / 12 % | ZODB per hit (1.0x) | no card can: ZODB alone takes the time of a hit |
| read_hot | threads | 4 | 100 KB | 3.14t | 2.12 | 13.4 | 8 % / 8 % | client cache | ZOE-156 |
| read_hot | threads | 16 | 100 KB | 3.14t | 2.51 | 16.4 | 3 % / 1 % | client cache | ZOE-156 |
| read_hot | threads | 64 | 100 KB | 3.14t | 2.48 | 13.5 | 2 % / 1 % | client cache | ZOE-156 |
| read_hot | processes | 4 | 1 KB | 3.14t | 0.221 | 1.1 | 62 % / 7 % | ZODB per hit (1.9x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 16 | 1 KB | 3.14t | 0.134 | 0.7 | 1 % / 3 % | ZODB per hit (1.8x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 64 | 1 KB | 3.14t | 0.085 | 0.6 | 58 % / 15 % | ZODB per hit (0.8x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 4 | 100 KB | 3.14t | 0.121 | 0.9 | 54 % / 4 % | ZODB per hit (1.4x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 16 | 100 KB | 3.14t | 0.092 | 0.9 | 70 % / 2 % | ZODB per hit (2.1x) | no card can: ZODB alone takes the time of a hit |
| read_hot | processes | 64 | 100 KB | 3.14t | 2.19 | 55.7 | 320 % / 2 % | client cache | ZOE-156, ZOE-162 |

## Server work

The prototypes of ZOE-077 are on in every poc row: compression at each `storea`, and a vote that reads only tids. The `/metrics` of the timed runs give the vote and the finish of each cell. Phase 3 needs four more changes on the server. Each becomes a new card:

1. **Compression off the path of the transaction, on a shared pool of threads.** The prototype compresses each record on the thread of the connection, in the `storea` call, so a transaction waits for its 100 compressions in a row.
   - At 100 KB this is the largest term. At concurrency 1, a transaction of `add` takes 62.5 ms through the POC client against 37.1 ms through ZEO. The floor takes 8.6 ms, the vote 0.1 ms and the finish 8.6 ms, so the 100 `storea` calls take about 45 ms.
   - The server uses 445 to 734 µs of CPU per 100 KB object, against 162 to 354 µs for ZEO.
   - ZOE-077 (part 2) proposed this design: one pool of compression threads for all transactions, sized to the cores, and a vote that waits for the compressions of its transaction. A lower zstd level or a size threshold for large records would cut the CPU too.
   - **Moves:** the 28 rows of the 100 KB commit: `add`, `update` and `read_warm` at 100 KB in the threads cells and the processes cells. Six of these rows are under 1× today: `add`, `update` and `read_warm` at concurrency 1 on both builds.
2. **A cheaper finish.** At concurrency 1 with 1 KB objects, `tpc_finish` takes 1.3 ms. The sync of a commit of that size takes about 0.3 ms on this disk, so about 1 ms is the redb insert and commit, against 0.16 ms for the append of FileStorage measured in ZOE-077.
   - At 100 KB the finish grows to 9 ms at concurrency 1, and to 89 to 245 ms per batch of about 3 commits at concurrency 64.
   - Fewer bytes per commit is a change of the record layout: the red zone of the format (ZOE-077, rank 4).
   - **Moves:** the 18 commit rows at 1 KB: the 6 rows at concurrency 1, and 12 rows above it, mostly `update` and `read_warm`. It also moves part of the 100 KB commit rows.
3. **The read path of a 100 KB record.** At concurrency 1, a cold load of a 100 KB object takes 289 µs through the POC client, 39 µs of which is ZODB, against 310 µs through ZEO. So the load itself is no cheaper than ZEO's: the decompression, the copies and the 100 KB on the wire take about 250 µs.
   - **Moves:** the 10 `read_cold` rows at 100 KB.
4. **The read path at 1 KB.** At concurrency 1, a cold load takes 103 µs through the POC client, against 21 µs for ZODB alone. A call of the POC client takes 70 µs, against 20 µs for loopback TCP. ZOE-077 measured that a load of `zoe serve` uses 3.3× the CPU of a ZEO load. Fewer handoffs between the threads of the server cut the server part. Prefetch in the native client (ZOE-153) cuts the client part.
   - **Moves:** the 6 `read_cold` rows at 1 KB: concurrency 1, and the processes cells on 3.13 and at concurrency 4 on 3.14t.

No server change moves the 24 rows of the ZODB floor, the 17 rows of ZODB per hit or the 11 rows of the client cache. The client cache is client work: ZOE-156 and ZOE-162.

## Limits

- **Not the full set of the harness.** There is no warmup, each worker runs 1 to 16 transactions per scenario instead of 100 or 30, and there are three samples per row instead of 20. A cell with few transactions per worker weighs the start of a run more: the first commit, the first loads.
- **Over the time cap.** The run took 23.5 minutes, so it skipped its two optional phases. There is no span split of the server and no row for `zoe serve` as on `main`. The server terms come from `/metrics` and the floors, not from the spans of N17.
- **The floor is not a zero-cost storage.** For reads, its DemoStorage loads from a read-only FileStorage, so in `read_hot` it is slower than a hit in ZEO.cache, and `## The client cache` bounds a hit with the ZEO time at concurrency 1 instead. In the threads mode its commit holds a lock for the whole commit, so on 3.14t a pool of connections passes it. In the processes mode with 100 KB objects, the 12 client vCPUs bound it, so the floor of `add` is close to ZEO there.
- **The ZEO server runs on the build of the client.** On 3.14t its one thread is slower, and ZEO's numbers in the 3.14t processes rows fall: `read_cold`, processes, 64, 1 KB gives 32,457 ops/s on 3.13 and 14,001 on 3.14t. 9 of the 26 rows at 3× or above are 3.14t processes rows.
- **zeo spreads in the processes mode** go up to 320 %, so the ratio of a single row there is a range, not a number. The table of `## Cells under 3×` gives each spread.
- **The POC client is not the native client.** It has no cache, no blobs, no conflict resolution, and its pool does not order the invalidations of its connections. The numbers bound what a native client of the same design can do. They are not the gate.

## Decisions for the human

1. The gate "at least 3× in every scenario" cannot hold in 41 rows, whatever the native client and the server do: the ZODB floor and the ZODB time of a hit. Should the phase 3 gate leave these rows out, as the phase 2 gate leaves out `read_hot`, or measure them against the floor?
2. The server work above becomes four new cards. The second one, fewer bytes per commit, is a change of the format.
3. The gate itself (ZOE-154) should run the full set of the harness, with a warmup, 20 samples and the spans, on a fixed machine. This reduced run gives the shape, not the gate.
