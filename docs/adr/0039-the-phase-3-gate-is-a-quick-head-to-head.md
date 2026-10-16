# 0039. The phase 3 gate is a quick head-to-head: faster than ZEO in every cell

Status: accepted. The human asked for a gate that runs very quickly and still asks ZOE to beat ZEO in every situation, and chose the design of this ADR. It supersedes ADR 0037.

## Context

ADR 0037 set the phase 3 gate: in each of 30 cells, the storage cost of `zoe serve` through the native client at most a third of the cost of ZEO 6 through `ZEO.ClientStorage`, over a floor storage of the harness, in 7 interleaved rounds of zodbshootout. Two facts make it a poor gate:

- **It is slow.** The gate set took 3 h 22 min on the development laptop. Each of its 630 values starts a zodbshootout process, a pyperf worker, a server and a fresh store, populates it, runs a warmup and keeps one sample. The cells of 16 workers at 100 KB write about 4.8 GB per call of `add` or `update`, and took about 80 minutes each. A gate that runs for hours runs rarely, and tells late.
- **The bar of 3× is out of reach for this phase.** The run of ZOE-193 passed 9 of 30 cells. Two cells are below 1× in their whole time (`read_warm` at threads 1), and one is level (`update` at threads 1 and 1 KB). The work of phase 3 is to beat ZEO everywhere first.

## Decision

**The bar.** `zoe serve` through the native client (`zoe-native`) is faster than ZEO 6 through `ZEO.ClientStorage` (`zeo`) in every cell of the gate set, on evidence. The floor storage and the noise exception go: with a bar of 1, the cost over a floor and the whole time give the same answer.

**The cells stay.** The 5 scenarios of zodbshootout (`add`, `update`, `read_cold`, `read_warm`, `read_hot`) in 3 shapes (threads 1, threads 16, processes 16), at 1 KB and 100 KB: 30 cells. A transaction holds 100 objects at 1 KB and 10 objects at 100 KB, so that one batch of 16 workers at 100 KB writes 16 MB, not 160 MB.

**A driver of the harness, not zodbshootout.** `shootout/native.py` runs the gate. Its workers use zodbshootout's `SpeedTestData`, so each transaction does the work of zodbshootout: the same objects, the same data, the same cache clears. For each cell, each storage gets one server on a fresh store, one populate, and one set of worker processes that live for the whole cell. zodbshootout stays the driver of the quick and full sets and of the parity gate of phase 2.

**ZOE stores raw.** The gate runs `zoe serve` with its defaults, which store records raw (ADR 0041), as FileStorage does. On the development laptop, the zstd decompression of one 100 KB record of zodbshootout costs about 60 µs, and a cold read of it through ZEO takes about 140 µs in all, so with zstd the cold read at 100 KB and concurrency 1 is a tie. The human chose to compare raw with raw, and then made raw the default.

**Pairs of batches.** A batch runs the same number of transactions in a row in each worker of one storage, as the inner loops of zodbshootout do. The warmup pair of a scenario sets that number: a batch lasts about 20 ms, with at most 20 transactions, and both storages run the same number. The time of a batch is the time of the slowest worker for one transaction. A pair is one batch of each storage, back to back, with the first storage swapped from one pair to the next. So a drift of the machine falls on both storages. One pair warms each scenario up and does not count. A batch of one transaction would measure each storage after a pause of the length of a batch of the other, which zodbshootout never does.

**A sequential verdict.** The ratio of a pair is `t_zeo / t_zoe`. After at least 10 pairs, a scenario passes when the 99 % confidence interval (Student's t) of the mean of the log of the ratios is above 0, and fails when it is below 0. It stops at the first verdict, at 100 pairs, or after 10 s of pairs. A scenario with no verdict then is undecided, and it fails the gate: a tie is not faster. The level of 99 % pays for the repeated looks at the same pairs.

**The Python build.** The phase 3 gate counts a run on CPython 3.14t. A run on another build is a report.

## Consequences

- A clear cell decides in about 10 pairs, so the gate runs in about a minute, and an undecided cell costs at most 10 s of pairs. It can run in the edit-and-test loop of a card, not only at the end of a phase.
- The gate measures ZOE and ZEO in the same processes, with the same objects, on the same machine state. It does not measure the start of a process, and no zodbshootout number counts for phase 3.
- A transaction of 10 objects at 100 KB is a smaller commit than the transaction of 100 objects of zodbshootout. The full set keeps the larger one.
- A near tie stays undecided. The answer for such a cell is to make ZOE faster, not to run the gate longer.
- The results of the gate validate against `bench/schema/native.schema.json`, and keep every pair. The report applies the rule to the pairs again.

## Alternatives

- **Keep ADR 0037 and lower 3 to 1.** It keeps the 3 hours, and the floor changes no verdict at a bar of 1.
- **Fewer rounds or fewer objects in zodbshootout.** The cost of each value is the start of zodbshootout, pyperf, the server and the populate, not the rounds. It cannot go below minutes.
- **A margin above 1, such as 1.25.** It asks more than the phase needs. The interval already asks for evidence, not for luck.
- **A fixed number of pairs.** A clear cell then waits for the noisy ones, and a near tie gets a verdict by chance.
