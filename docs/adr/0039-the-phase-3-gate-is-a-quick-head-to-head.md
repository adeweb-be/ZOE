# 0039. The phase 3 gate is a quick head-to-head: faster than ZEO in every cell

Status: accepted.

## Context

N4 sets the phase 3 gate: `zoe serve` through the native client against ZEO 6 through `ZEO.ClientStorage`, in the zodbshootout cells of the gate set. A gate that runs for hours runs rarely, and tells late. The work of phase 3 is to beat ZEO everywhere first.

Several facts of the measures shape the gate:

- **Part of the time of a cell is not the storage.** The time of a cell holds ZODB, persistent, zodbpickle and the loop of zodbshootout as well as the client, the wire and the server. In `read_hot` at 100 KB, ZODB alone takes 17 µs per object at threads 1, against 10.5 µs for a 3× of ZEO (31.5 µs ÷ 3), and 82 to 92 µs at processes 16 against 42 µs (finding of ZOE-187). There, no storage reaches 3× of the whole time.
- **A floor storage is not a bound.** A DemoStorage over a FileStorage takes 44 µs per object in `read_hot` at threads 1 and 100 KB on the machine of the phase 3 budget spike, and the native client takes 34 µs in the gate run of #382.
- **A few values decide nothing in the noisy cells.** A value of a processes cell spreads up to ±50 % on the laptop of ZOE-187. A ratio of 0.88× at processes 16 became 1.03 to 1.34× in five paired rounds. A fixed order of the variants gave a false regression of 0.83×. zodbshootout reports a time even when the benchmark function raises.
- **zodbshootout is slow as a gate.** A gate of 7 interleaved rounds of three storages (zoe-native, ZEO and a floor) in 30 cells took 3 h 22 min on the development laptop. Each of its 630 values starts a zodbshootout process, a pyperf worker, a server and a fresh store, populates it, runs a warmup and keeps one sample. The cells of 16 workers at 100 KB write about 4.8 GB per call of `add` or `update`, and took about 80 minutes each.
- **The bar of 3× is out of reach for this phase.** A run of that gate passed 9 of 30 cells. Two cells are below 1× in their whole time (`read_warm` at threads 1), and one is level (`update` at threads 1 and 1 KB).

## Decision

**The bar.** `zoe serve` through the native client (`zoe-native`) is faster than ZEO 6 through `ZEO.ClientStorage` (`zeo`) in every cell of the gate set, on evidence. No floor storage and no noise exception: with a bar of 1, the cost over a floor and the whole time give the same answer.

**The cells.** The 5 scenarios of zodbshootout (`add`, `update`, `read_cold`, `read_warm`, `read_hot`) in 3 shapes (threads 1, threads 16, processes 16), at 1 KB and 100 KB: 30 cells. A transaction holds 100 objects at 1 KB and 10 objects at 100 KB, so that one batch of 16 workers at 100 KB writes 16 MB, not 160 MB.

**A driver of the harness, not zodbshootout.** `shootout/native.py` runs the gate. Its workers use zodbshootout's `SpeedTestData`, so each transaction does the work of zodbshootout: the same objects, the same data, the same cache clears. For each cell, each storage gets one server on a fresh store, one populate, and one set of worker processes that live for the whole cell. zodbshootout stays the driver of the quick and full sets and of the parity gate of phase 2.

**ZOE stores raw.** The gate runs `zoe serve` with its defaults, which store records raw (ADR 0020), as FileStorage does. With zstd, the cold read at 100 KB and concurrency 1 is a tie. The gate compares raw with raw.

**Pairs of batches.** A batch runs the same number of transactions in a row in each worker of one storage, as the inner loops of zodbshootout do. The warmup pair of a scenario sets that number: a batch lasts about 20 ms, with at most 20 transactions, and both storages run the same number. The time of a batch is the time of the slowest worker for one transaction. A pair is one batch of each storage, back to back, with the first storage swapped from one pair to the next. So a drift of the machine falls on both storages. One pair warms each scenario up and does not count. A batch of one transaction would measure each storage after a pause of the length of a batch of the other, which zodbshootout never does.

**A sequential verdict.** The ratio of a pair is `t_zeo / t_zoe`. After at least 10 pairs, a scenario passes when the 99 % confidence interval (Student's t) of the mean of the log of the ratios is above 0, and fails when it is below 0. It stops at the first verdict, at 100 pairs, or after 10 s of pairs. A scenario with no verdict then is undecided, and it fails the gate: a tie is not faster. The level of 99 % only lowers the cost of the repeated looks at the same pairs: in 20,000 simulated true ties, the rule gives a pass by chance in about 4 % of the cells and a fail in about 4 %, not 0.5 %. The cap of 10 s lowers it in practice.

**The Python build.** The phase 3 gate counts a run on CPython 3.14t. A run on another build is a report.

## Consequences

- A clear cell decides in about 10 pairs, so the gate runs in about a minute, and an undecided cell costs at most 10 s of pairs. It can run in the edit-and-test loop of a card, not only at the end of a phase.
- The gate measures ZOE and ZEO in the same processes, with the same objects, on the same machine state. It does not measure the start of a process, and no zodbshootout number counts for phase 3.
- A transaction of 10 objects at 100 KB is a smaller commit than the transaction of 100 objects of zodbshootout. The full set keeps the larger one.
- A near tie stays undecided. The answer for such a cell is to make ZOE faster, not to run the gate longer.
- The results of the gate validate against `bench/schema/native.schema.json`, and keep every pair. The report applies the rule to the pairs again.

## Alternatives

- **The cost over a floor, at 3×.** The harness measures a third storage, `floor`, in each cell: a storage of the harness in the zodbshootout process, with no server, that keeps each record in memory and runs the two-phase commit with no lock across transactions. A cell passes when `(t_zeo − t_floor) / (t_zoe − t_floor) ≥ 3`, on the median of 7 interleaved rounds, and a cell where the cost of ZEO is below 10 % of its time is below the noise. It takes hours, and with a bar of 1 the floor changes no verdict. The candidates for the floor each fail:
  - **MappingStorage** holds its commit lock from `tpc_begin` to `tpc_finish`, so the threads commit one at a time, and its floor of the threads cells of `add` and `update` would be above the time of the native client.
  - **DemoStorage over a read-only FileStorage** (the phase 3 budget spike) reads the base file, and the native client is below it.
  - **The diagnostic lookup of ZOE-187** (the native client that serves `loadBefore` from a dict of the process) is a floor of reads only. It does not cover the commits, or the first read of a cold cell.
- **A share of the reachable ratio:** pass when the ratio is at least `min(3, k × t_zeo ÷ t_floor)`. It needs a value of `k` that no measure gives.
- **3× of the whole time, with cells left out by hand** (ZOE-190). Each group needs a spike, and the list grows.
- **The floor rule with a bar of 1.** It keeps the 3 hours, and the floor changes no verdict at a bar of 1.
- **Fewer rounds or fewer objects in zodbshootout.** The cost of each value is the start of zodbshootout, pyperf, the server and the populate, not the rounds. It cannot go below minutes.
- **More values without interleaving.** A drift of the machine then falls on one storage.
- **A margin above 1, such as 1.25.** It asks more than the phase needs. The interval already asks for evidence, not for luck.
- **A fixed number of pairs.** A clear cell then waits for the noisy ones, and a near tie gets a verdict by chance.

## Merged ADRs

- ADR 0037, the phase 3 gate counts the cost of the storage against a floor: its facts are in Context, and its rule and its floors are in Alternatives, "The cost over a floor, at 3×". This ADR replaces its decision.
