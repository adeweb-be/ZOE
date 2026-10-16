# Spike: the cells of one call at a time

Card ZOE-188, requirement N4. Tracked in #386.

## Verdict

The 7 cells of this card do not reach 3× ZEO with the remedies that this spike measured. Two remedies close part of the gap. Two cells have a bound that no card of the client or the server can move on the machine of this spike.

- **`read_warm` at threads 1 is slower than ZEO because of a lookup that the timer counts.** zodbshootout times each `read_warm` sample on a new DB, so each sample is the first transaction of a new client. The native client looks up the canonical name of the server (`socket.gethostbyaddr`) at the first `sortKey()`, in the first commit. That lookup takes 2 to 3 ms on this machine (37 ms when it is cold). ZEO makes the same lookup at the connection, before the timer starts. A prototype that looks up the name at the connection moves the cell from 216.3 to 199.5 µs per object (cost ratio 0.87 to 0.92). The first commit of a new client also opens one more connection of the pool: 0.43 to 0.74 ms.
- **A load at concurrency 1 costs 54 µs through the native client and 64.5 µs through ZEO here.** The floor of a call is about 20 µs: a loopback round trip of 16.4 µs and a read of the store of 3.9 µs. Four handoffs between threads take about 24 µs of the 54: two in the client and two in the server. A prototype that runs `loadBefore` on the task of the reader of `zoe serve` moves `read_cold` at threads 1 from 67.1 to 57.6 µs per object (cost ratio 1.13 to 1.34) and `read_warm` from 213.9 to 194.3 (0.92 to 1.02).
- **The commit cells at threads 1 are bound by the flush of the disk.** A transaction at concurrency 1 waits for at least one flush. Here a flush (`F_FULLFSYNC`) of 4 KiB takes 3.0 ms. Beyond the floor, 3× ZEO allows a storage cost of 2.1 ms per transaction in `add` and 2.7 ms in `update`. So neither cell can reach 3× on this disk. The flush of the laptop of the gate is not measured. This is a question for the human (see Cards).
- **A process pays nothing that a thread does not pay, on the side of the native client.** At processes 16 and 1 KB, the native client takes about the time that it takes at threads 16. ZEO is faster with 16 processes than with 16 threads, because each process has its own IO thread. `update` at processes 16 is the commit path of the server under 16 writers (ZOE-184). `read_hot` at processes 16 spreads too much to decide: 0.51 to 2.55 on the laptop, 0.84 to 126.45 here.

All the numbers of this finding come from one Apple M3 Pro with macOS, not from the laptop of the gate (see Method). The human chose this machine for the spike in the build session; the comment of the builder on #387 records the decision. The numbers give the terms and the shape. They are not the gate.

## Question

What bounds the gate cells where one call waits for the one before it, and is 3× ZEO reachable there? The cells of this card, with the cost-gate report of the laptop of the gate (#393, `main` at c0db22b, CPython 3.14t, µs per object of the slowest worker, medians of 7 rounds):

| scenario | mode | c | size | zeo µs | zoe-native µs | floor µs | cost |
| --- | --- | --- | --- | --- | --- | --- | --- |
| read_warm | threads | 1 | 1 KB | 469.3 | 751.2 | 22.7 | 0.61 |
| read_warm | threads | 1 | 100 KB | 743.6 | 912.2 | 104.3 | 0.81 |
| update | threads | 1 | 1 KB | 126.0 | 167.3 | 13.8 | 1.03 |
| add | threads | 1 | 1 KB | 84.2 | 68.9 | 9.5 | 1.22 |
| update | processes | 16 | 1 KB | 1812.1 | 945.8 | 58.7 | 1.75 |
| read_hot | processes | 16 | 1 KB | 45.5 | 38.2 | 27.4 | 1.99 |
| read_cold | threads | 1 | 1 KB | 150.7 | 70.9 | 8.0 | 2.25 |

The same cell threads 1 at 1 KB on the machine of this spike (the first gate run, no switch on; see Method):

| scenario | zeo µs | zoe-native µs | floor µs | cost | range |
| --- | --- | --- | --- | --- | --- |
| add | 70.6 | 57.6 | 6.4 | 1.26 | 1.21–1.30 |
| update | 89.2 | 75.1 | 7.7 | 1.21 | 1.19–1.22 |
| read_cold | 75.2 | 67.1 | 4.7 | 1.13 | 1.11–1.21 |
| read_warm | 185.0 | 212.9 | 13.6 | 0.87 | 0.78–0.99 |
| read_hot | 6.5 | 5.0 | 4.5 | 3.48 | 3.11–4.15 |

The runs of a cell on this machine move by a few µs from run to run: the tables below give the run that each comparison used.

## Method

**Machine.** Apple M3 Pro: 6 performance cores and 6 efficiency cores, 36 GiB of memory, macOS 27.0.1. The stores are on the internal SSD (APFS), never on tmpfs: the work directories of the harness are under `target/`, and those of the scripts are in a directory of the user's disk. macOS has no CPU pinning and no governor that a user can set. Its scheduler moves a thread between the two kinds of cores, so a thread that waits often can run on an efficiency core. The rounds of the harness are interleaved, so this falls on every storage.

**Python and the GIL.** CPython 3.14.7t, the free-threaded build. The harness records the GIL of each worker process: it stayed off in each run. One run of the gap analysis of `read_warm` ran on CPython 3.13 by mistake. This finding uses it only to find the gap; every number in the tables below comes from 3.14t.

**Durability.** On macOS, `fsync` does not write the cache of the drive to the medium; only `fcntl(F_FULLFSYNC)` does. Rust's `File::sync_data` uses `F_FULLFSYNC`, so `zoe serve` makes durable commits. The prototype switch `SPIKE_ZEO_FULLFSYNC=1` makes `os.fsync` of the ZEO server use `F_FULLFSYNC` too. Every run of this finding uses it.

**Code.** `main` at 1180b2f. The prototypes and the scripts are on the branch `poc/ZOE-188`, behind environment switches, so one build measures both sides (`bench/spike-call-latency/README.md` there):

- `ZOE_SPIKE_TRACE`: probes on a clock that the processes share (`CLOCK_UPTIME_RAW`), in `zoe-client`, the extension and `zoe serve`, and, through a `sitecustomize`, in ZODB's DB and in ZEO's client and server. They split one call into its terms.
- `ZOE_SPIKE_EAGER_ADDR`: the native client looks up the canonical name of the server at the connection.
- `ZOE_SPIKE_INLINE_LOAD`: `zoe serve` runs each `loadBefore` on the task of the reader, with no thread of the read pool.

**Gate runs.** One job per cell, as the planner's comment on the card says: `just bench run --gate --storage zeo zoe-native floor --cell <cell>`, then `just bench gate --storage zoe-native --python cp314t`. Each cell runs 7 rounds of the three storages in a rotating order (ADR 0037). Each A/B ran its two sides one after the other, with the same `zoe` binary.

**Focused reproductions.** Scripts of the poc branch, with the server setup of the harness: `phases.py` times the phases of each scenario at threads 1 and the calls of the storage; `shoot.py` runs one zodbshootout value as the harness does; `terms.py` splits each traced `loadBefore`; `rtt.py` measures a loopback round trip with blocking sockets; `fsync.py` measures one durable write of 4 KiB.

## The time of one call

The median of about 3,000 traced loads of `read_cold` at threads 1 and 1 KB for each storage, and of 40 transactions of 100 objects for `update` and `add`. Each row is the median of its own term, so the terms of a storage do not add up to its total, the median of the whole call: the native terms add up to 53.3 µs against 54.2, and the terms of ZEO to 60.8 µs against 64.5. The probes add about 1 µs to a load. The commit rows give the time of the call as the client sees it, then the time on the server.

| scenario | storage | term | µs |
| --- | --- | --- | --- |
| read_cold | zoe-native | Python and ZODB to the extension | 0.5 |
| read_cold | zoe-native | extension: the pool, `block_on`, to the call | 0.4 |
| read_cold | zoe-native | client: encode, queue the frame | 1.8 |
| read_cold | zoe-native | client: the writer task wakes and writes (handoff) | 5.8 |
| read_cold | zoe-native | socket to the server; the reader task wakes and reads | 10.4 |
| read_cold | zoe-native | server: decode, ticket, a thread of the read pool (handoff) | 6.9 |
| read_cold | zoe-native | server: the load in the store | 3.9 |
| read_cold | zoe-native | server: the outbox; the writer task wakes and writes (handoff) | 6.3 |
| read_cold | zoe-native | socket to the client; the reader task wakes and reads | 10.2 |
| read_cold | zoe-native | client: decode, dispatch the reply | 1.2 |
| read_cold | zoe-native | client: the caller wakes (handoff) | 5.2 |
| read_cold | zoe-native | extension: the bytes to Python | 0.7 |
| read_cold | zoe-native | total of one `loadBefore` | 54.2 |
| read_cold | zeo | `ClientStorage` to the IO thread | 11.2 |
| read_cold | zeo | IO thread: encode, write | 7.0 |
| read_cold | zeo | socket to the server; the event loop wakes and reads | 7.2 |
| read_cold | zeo | server: decode, dispatch | 3.1 |
| read_cold | zeo | server: the load in FileStorage | 4.4 |
| read_cold | zeo | server: encode, write | 6.6 |
| read_cold | zeo | socket to the client; the event loop wakes and reads | 7.1 |
| read_cold | zeo | IO thread: decode, set the result; the caller wakes | 14.2 |
| read_cold | zeo | total of one `loadBefore` | 64.5 |
| read_cold | both | loopback round trip, blocking sockets, reply of 1.1 KB (no handoff) | 16.4 |
| update | zoe-native | `tpc_begin` | 16 |
| update | zoe-native | 100 `store` | 210 |
| update | zoe-native | `tpc_vote` (server: 56) | 117 |
| update | zoe-native | `tpc_finish` (server `tpc_finish`, the commit of redb with its one fsync, not split: 5124) | 5302 |
| update | zeo | `tpc_begin` | 156 |
| update | zeo | 100 `store` | 910 |
| update | zeo | `tpc_vote` (server: 879) | 1124 |
| update | zeo | `tpc_finish` (server: 3640, of which the fsync: 3550) | 4997 |
| add | zoe-native | `tpc_begin` | 19 |
| add | zoe-native | 100 `new_oid` | 150 |
| add | zoe-native | 101 `store` | 172 |
| add | zoe-native | `tpc_vote` (server: 42) | 133 |
| add | zoe-native | `tpc_finish` (server `tpc_finish`, not split: 4265) | 4418 |
| add | zeo | `tpc_begin` | 120 |
| add | zeo | 100 `new_oid` | 290 |
| add | zeo | 101 `store` | 778 |
| add | zeo | `tpc_vote` | 989 |
| add | zeo | `tpc_finish` | 4556 |
| update | zoe-native | the whole `transaction.commit()` of 100 objects | 6440 |
| update | zeo | the whole `transaction.commit()` of 100 objects | 7870 |
| add | zoe-native | the whole `transaction.commit()` of 100 objects | 5530 |
| add | zeo | the whole `transaction.commit()` of 100 objects | 7350 |
| update | both | one durable write of 4 KiB (`F_FULLFSYNC`) | 2978 |
| read_hot | zoe-native | one hit of the client cache, beyond the floor (5.0 against 4.5 per object) | 0.5 |
| read_hot | zeo | one hit of the client cache, beyond the floor (6.5 against 4.5 per object) | 2.0 |

What the terms say:

- **The client of ZOE is cheaper than the client of ZEO** (15.6 µs against 32.4 µs). The server of ZOE is a little dearer (17.1 µs against 14.1 µs), and so are its two socket legs (20.6 µs against 14.3 µs).
- **Four handoffs between threads take about 22 to 24 µs of the 54 of a native load**: the writer task of the client (5.8), the thread of the read pool (most of 6.9), the writer task of the server (6.3), and the wake of the caller (5.2). ZEO's server reads, runs the call and writes on one thread, its event loop. Each socket leg of ZOE also takes about 3 µs more than ZEO's: the reader is a task of tokio that a worker thread must run after the event, where the event loop of ZEO waits in `kqueue` itself.
- **A commit at concurrency 1 is its flush.** `tpc_finish` takes 4.4 to 5.3 ms of the 5.5 to 6.4 ms of the whole `transaction.commit()` of a native transaction (the last rows of the table). redb makes one fsync per commit (`docs/spikes/record-store.md`). The `tpc_finish` of the server, the commit of redb with its one fsync, takes longer than ZEO's (5.1 ms against 3.6 ms). The spike did not split the fsync out of it. It takes longer because redb writes scattered pages and FileStorage appends (`docs/spikes/write-log.md`). That is the subject of ZOE-184. The client of ZOE spends less around the flush than ZEO: for `tpc_begin`, the `new_oid` calls, the stores and the vote, 0.34 ms against 2.19 ms in `update`, and 0.47 ms against 2.18 ms in `add`.

## The floor

The floor of the harness (ADR 0037) gives the time of ZODB and zodbshootout in each cell. 3× ZEO then sets the largest time per object that `zoe-native` may take: `floor + (zeo − floor) ÷ 3`. The rows of this Mac come from the run without a switch of the eager A/B (see `read_warm`), threads 1 at 1 KB:

| scenario | zeo µs | zoe-native µs | floor µs | cost | range |
| --- | --- | --- | --- | --- | --- |
| add | 70.6 | 56.1 | 6.5 | 1.28 | 1.24–1.32 |
| update | 88.7 | 76.4 | 7.8 | 1.18 | 1.16–1.19 |
| read_cold | 75.4 | 67.1 | 4.8 | 1.12 | 1.11–1.17 |
| read_warm | 189.2 | 216.3 | 13.6 | 0.87 | 0.61–1.09 |
| read_hot | 6.5 | 5.1 | 4.5 | 3.34 | 3.18–4.64 |
 The table compares the limit with the bound of a storage that talks to a server on this machine: a round trip and a read of the store for a load (about 20 µs), and one flush for a commit (3.0 ms for 100 objects, so 30 µs per object).

| cell | machine | zeo µs | zoe-native µs | floor µs | 3× allows µs | bound of a storage µs | reachable |
| --- | --- | --- | --- | --- | --- | --- | --- |
| read_cold threads 1, 1 KB | laptop | 150.7 | 70.9 | 8.0 | 55.6 | not measured | yes, if the call loses 15 µs |
| read_cold threads 1, 1 KB | this Mac | 75.4 | 67.1 | 4.8 | 28.3 | 25 (floor + 20) | only with a call of at most 23 µs |
| read_warm threads 1, 1 KB | laptop | 469.3 | 751.2 | 22.7 | 171.6 | not measured | see `read_warm` |
| read_warm threads 1, 1 KB | this Mac | 189.2 | 216.3 | 13.6 | 72.1 | 64 (floor + 20 + 30) | only at the bound |
| update threads 1, 1 KB | laptop | 126.0 | 167.3 | 13.8 | 51.2 | not measured | depends on the flush of the laptop |
| update threads 1, 1 KB | this Mac | 88.7 | 76.4 | 7.8 | 34.8 | 38 (floor + 30) | no |
| add threads 1, 1 KB | laptop | 84.2 | 68.9 | 9.5 | 34.4 | not measured | depends on the flush of the laptop |
| add threads 1, 1 KB | this Mac | 70.6 | 56.1 | 6.5 | 27.9 | 37 (floor + 30) | no |
| update processes 16, 1 KB | this Mac | 965.9 | 395.8 | 29.1 | 341.4 | the commit path under 16 writers | yes: the native client is at 2.56 |
| read_hot processes 16, 1 KB | this Mac | 16.9 | 12.3 | 11.4 | 13.2 | a hit costs 0.5 µs at threads 1 | yes: the median passes here (6.33) |

The rule for the laptop: a commit cell at threads 1 can reach 3× only when one flush of its disk, plus the calls of the commit (0.34 to 0.47 ms on this Mac), is at most `(zeo − floor) ÷ 3` per transaction: 3.7 ms for `update` and 2.5 ms for `add` with the laptop's numbers. Measure the flush there with `fsync.py` of the poc branch (`plain`, since Linux `fsync` flushes the drive) before a card for these cells.

## read_warm

**What zodbshootout times.** `read_warm` is `bench_read_after_write`. The harness runs one pyperf value with one warmup, one loop, and no inner loops for this scenario. pyperf calls the benchmark function once for the warmup and once for the value, and each call opens a new DB. So each sample is the first transaction of a new client: the open of a connection, 100 loads of a cold cache (106 with the tree), the update of the 100 objects, one commit, and the read of the objects that the connection holds.

**Does it miss the client cache?** Yes, by design: the cache of a new client is empty, for both storages. Each load goes to the server and costs what a load of `read_cold` costs. The cache does not explain the gap.

**What each sample costs.** A new DB for each sample, timed as zodbshootout times it, with no wrapper, 90 samples of each storage, interleaved (µs per object, medians):

| term | zeo | zoe-native |
| --- | --- | --- |
| the open and 100 loads | 75.7 | 68.7 |
| the first `sortKey()`, called by itself before the commit | 0.0 | 15.0 |
| the commit of 100 objects | 69.4 | 67.8 |
| total | 146.0 | 151.8 |

Without its first `sortKey()`, the native client is faster than ZEO (136.8 against 146.0).

**The first `sortKey()`.** ZODB sorts the resources of a transaction by `sortKey()` before `tpc_begin`. `sortKey()` of `ZEO.ClientStorage` reads `_server_addr`, the canonical name of the server and its port. ZEO's `set_server_addr` calls `socket.gethostbyaddr` in `notify_connected`, during the open, so the timer does not count it. The native client defers the lookup to the first read of `_server_addr` (the docstring of `set_server_addr` in `zoe.zeo` says: the lookup is slow, so the open does not wait for it). The first read is in the first commit, inside the timer. In the run that found the gap (on CPython 3.13, see Method), the gap between the last load and `tpc_begin` was 3958 µs (median) for the native client, and 29.5 µs for ZEO. On 3.14t, the first `sortKey()` takes 15.0 µs per object in the table above: 1.5 ms per sample, the median of 90 samples. A log of three opens gives 2.1 to 2.8 ms for the lookup itself. `gethostbyaddr` does not always take the same time. `gethostbyaddr('127.0.0.1')` alone takes 3 ms on this machine, and 37 ms when it is cold.

**The prototype.** `ZOE_SPIKE_EAGER_ADDR` makes the lookup at the connection. A log of each `set_server_addr` and each lookup shows one lookup per new client, in the open, outside the timer. The gate cell threads 1 at 1 KB, one run after the other:

| run | zeo µs | zoe-native µs | floor µs | cost | range |
| --- | --- | --- | --- | --- | --- |
| switch off | 189.2 | 216.3 | 13.6 | 0.87 | 0.61–1.09 |
| switch on | 187.7 | 199.5 | 13.4 | 0.92 | 0.87–1.16 |

The other scenarios of the cell do not move.

The A/B of the inline load (`ZOE_SPIKE_INLINE_LOAD`) on the same cell, with the eager lookup off on both sides:

| scenario | run | zeo µs | zoe-native µs | floor µs | cost | range |
| --- | --- | --- | --- | --- | --- | --- |
| read_warm | switch off | 201.9 | 213.9 | 13.7 | 0.92 | 0.84–1.08 |
| read_warm | switch on | 195.4 | 194.3 | 13.9 | 1.02 | 0.93–1.12 |
| read_cold | switch off | 75.7 | 67.1 | 4.8 | 1.13 | 1.12–1.22 |
| read_cold | switch on | 75.4 | 57.6 | 4.7 | 1.34 | 1.31–1.38 |

The commit scenarios of the cell do not move.

**The rest.** The first commit of a new client also opens one more connection: the listener of the pool never commits (ADR 0036), so the first transaction finds no free connection. Its `tpc_begin` takes 0.43 to 0.74 ms, against 0.016 ms on a pool that has its connection. The inline load of the server (Cards, item 1) alone brings `read_warm` to the level of ZEO on this machine (1.02, table above). No run had both switches on. 3× allows 72.1 µs here, and 100 loads and one flush cost at least 64 µs. So the cell reaches 3× only at the bound of the machine.

**The laptop.** There the gap is 28 ms per transaction (751.2 against 469.3 µs per object), about ten times the gap here. The cost of the lookup depends on the resolver of the machine: on Linux, the order of `/etc/nsswitch.conf` and a reverse query to a DNS server can make it take tens of milliseconds. This spike did not measure it there. Run `python3 -c "import socket, time; t = time.perf_counter(); socket.gethostbyaddr('127.0.0.1'); print(time.perf_counter() - t)"` on the laptop before the card. The first commit on the laptop is also slower than on this Mac (`update` at threads 1: 167.3 µs per object there).

## Processes and threads

The cells of 16 workers at 1 KB on this machine (cost-gate report, 7 rounds):

| scenario | zeo µs | zoe-native µs | floor µs | cost | range | verdict |
| --- | --- | --- | --- | --- | --- | --- |
| add | 931.4 | 218.5 | 45.3 | 4.95 | 4.53–5.82 | pass |
| update | 965.9 | 395.8 | 29.1 | 2.56 | 2.45–2.77 | FAIL |
| read_cold | 594.8 | 259.5 | 12.9 | 2.35 | 2.28–2.47 | FAIL |
| read_warm | 1739.9 | 640.1 | 94.0 | 3.15 | 2.18–5.00 | pass |
| read_hot | 16.9 | 12.3 | 11.4 | 6.33 | 0.84–126.45 | pass |

**What a process pays that a thread does not.** Each process opens its own client: a pool, a cache, the threads of tokio and the thread of the deliveries. The open is outside the timer. In the scenarios with inner loops, its first-transaction costs (the lookup, the second connection) come once per 30 transactions. On the laptop, the native client takes about the same time at processes 16 as at threads 16 (`update`: 945.8 against 970.0 µs per object), while ZEO is faster with processes (1812.1 against 2604.1). These four numbers are from the cost-gate report on #393. Each process of ZEO has its own client and its own IO thread. So the processes cells are harder for the native client than the threads cells because ZEO gains, not because the native client loses.

**`update` at processes 16** is 16 writers at once. Its time is the commit path of the server with its group commit, the subject of ZOE-184.

**`read_hot` at processes 16** cannot be decided with 7 rounds. The floor is close to both storages (11.4 against 12.3 and 16.9 here, 27.4 against 38.2 and 45.5 on the laptop), so a small change of the slowest worker moves the ratio a lot. The laptop has 6 performance cores, 8 efficiency cores and 2 low-power cores, and this Mac has 6 and 6. With 16 processes, the slowest worker can run on a slower core in one round and not in the next. A hit of the native cache costs 0.5 µs beyond the floor at threads 1, so the client cache is not the term here.

## Cards

Ranked by the gain measured on this Mac. The first two are measured prototypes; the gains of the others are the terms of the table, not runs. On the laptop, item 2 can gain more than item 1, if the lookup is the gap there.

1. **`zoe serve` runs a `loadBefore` on the task of the reader** (measured: `read_cold` threads 1 at 1 KB, 67.1 to 57.6 µs per object; `read_warm` threads 1 at 1 KB, 213.9 to 194.3). Moves `read_cold` threads 1, `read_warm` threads 1, and `read_cold` processes 16 on this Mac. This changes an invariant of `zoe-server`: "Core calls run on blocking threads of the runtime, never on the async runtime threads." A load that reads the disk would then hold a worker of tokio. The card needs the human's decision first, and a rule for which loads may run there (for example, a load of a page that redb holds in memory).
2. **The native client looks up the name of the server at the connection** (measured: `read_warm` threads 1 at 1 KB, 216.3 to 199.5 µs per object on this Mac; up to 28 ms per transaction on the laptop if the lookup is the gap there). Moves `read_warm` threads 1 at 1 KB and 100 KB. It adds margin to `read_warm` processes 16 at 1 KB, which passes on the laptop (3.02). The lookup can run on a thread that starts at the connection, so the open does not wait and the first `sortKey()` waits only for what is left. No public API changes: `sortKey()` gives what it gives today. Check first on the laptop how long the lookup takes.
3. **`zoe serve` writes a reply from the thread of the call when its writer is idle** (estimate: up to 6.3 µs per call, the handoff to the writer task). Moves `read_cold` threads 1 and `read_warm` threads 1.
4. **The native client writes its own frame when no other frame waits** (estimate: up to 5.8 µs per call, the handoff to the writer task). Moves `read_cold` threads 1 and `read_warm` threads 1, and the round trips of the vote and the finish in `update` and `add` threads 1 by a few µs.
5. **The native client opens the first connection for commits at the open, in the background** (estimate: 0.43 to 0.74 ms per sample, about 5 µs per object). Moves `read_warm` threads 1. The listener still never commits (ADR 0036).
6. **No card: `add` threads 1 and `update` threads 1 at 1 KB.** On this Mac, 3× allows a storage cost of 2.1 ms and 2.7 ms per transaction beyond the floor, and one flush takes 3.0 ms. No change of the client or the server moves the flush. The human decides whether these cells stay in the gate, or whether the gate machine's flush makes them reachable (measure it first, see The floor). On the laptop, ZOE-184 owns the part of the commit that comes from the bytes that redb writes.
7. **`update` processes 16 at 1 KB goes to ZOE-184**: 16 writers on the commit path of the server.
8. **No card: `read_hot` processes 16 at 1 KB** until the spread is smaller. The median passes on this Mac (6.33) and fails on the laptop (1.99), with ranges of 0.84–126.45 and 0.51–2.55. The human decides whether the gate counts this cell with 7 rounds on a CPU with two or three kinds of cores.
