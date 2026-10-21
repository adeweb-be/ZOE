# 0039. The phase 3 gate is a quick head-to-head: faster than ZEO in every cell

Status: accepted.

## Context

N4 sets the phase 3 gate: `zoe serve` through the native client against ZEO 6 through `ZEO.ClientStorage`, in the cells of the gate set. A gate that runs for hours runs rarely and tells late. The facts of the measures:

- **Part of the time of a cell is not the storage.** In `read_hot` at 100 KB, ZODB alone takes 17 µs per object at threads 1, against 10.5 µs for 3× of ZEO. No storage reaches 3× of the whole time there.
- **A floor storage is not a bound.** In the same cell, a DemoStorage over a FileStorage takes 44 µs per object, and the native client 34 µs.
- **A few values decide nothing in a noisy cell.** A value of a processes cell spreads up to ±50 %. A cell at 0.88× measured 1.03 to 1.34× in five paired rounds.
- **zodbshootout is slow as a gate.** 7 interleaved rounds of three storages in 30 cells took 3 h 22 min: each value starts its own processes, server and store.
- **3× is out of reach in this phase.** That gate passed 9 of 30 cells.

## Decision

The gate as built: specification, "Performance and testing harness", "B. Performance", item 1, and the native gate row of "Acceptance thresholds".

**The bar.** `zoe serve` through the native client (`zoe-native`) is faster than ZEO 6 through `ZEO.ClientStorage` (`zeo`) in every cell of the gate set, on evidence. No floor storage, no noise exception.

**The cells.** The 5 scenarios of zodbshootout (`add`, `update`, `read_cold`, `read_warm`, `read_hot`) in 3 shapes (threads 1, threads 16, processes 16), at 1 KB and 100 KB: 30 cells. A transaction holds 100 objects at 1 KB and 10 at 100 KB.

**A driver of the harness, not zodbshootout.** `shootout/native.py` runs the gate with zodbshootout's `SpeedTestData`: the same objects, data and cache clears. Each storage gets one server, one fresh store, one populate and one set of workers for the whole cell. zodbshootout stays the driver of the quick and full sets and of the phase 2 gate.

**ZOE stores raw.** The gate runs `zoe serve` with its defaults, which store records raw (ADR 0020), as FileStorage does.

**Pairs of batches.** A batch runs about 20 ms of transactions, at most 20, in each worker of one storage. Its time is the time of the slowest worker for one transaction. A pair is one batch of each storage, and the first storage swaps from pair to pair, so a drift of the machine falls on both. One warmup pair sets the batch size and does not count.

**A sequential verdict.** The ratio of a pair is `t_zeo / t_zoe`. After at least 10 pairs, a scenario passes when the 99 % confidence interval (Student's t) of the mean log ratio is above 0, and fails when it is below 0. It stops at its first verdict, at 100 pairs, or after 10 s of pairs. A scenario without a verdict is undecided and fails: a tie is not faster. In 20,000 simulated true ties, the repeated looks give a pass by chance in about 4 % of the cells and a fail in about 4 %; the cap of 10 s lowers this.

**The Python build.** The phase 3 gate counts a run on CPython 3.14t. A run on another build is a report.

## Consequences

- A clear cell decides in about 10 pairs, so the gate runs in about a minute and fits the edit-and-test loop of a card.
- Both storages run in the same processes, with the same objects, on the same machine state. No zodbshootout number counts for phase 3.
- A transaction of 10 objects at 100 KB is a smaller commit than zodbshootout's. The full set keeps the larger one.
- A near tie stays undecided. The answer is to make ZOE faster, not to run the gate longer.
- The results validate against `bench/schema/native.schema.json` and keep every pair, so the report applies the rule again.

## Alternatives

- **The cost over a floor, at 3×.** A cell passes when `(t_zeo − t_floor) / (t_zoe − t_floor) ≥ 3` on the median of 7 interleaved rounds. It takes hours, and no floor works: MappingStorage commits one thread at a time, DemoStorage over FileStorage is slower than the native client, and a dict in the client covers reads only.
- **A share of the reachable ratio, `min(3, k × t_zeo ÷ t_floor)`.** No measure gives `k`.
- **3× of the whole time, with cells left out by hand.** Each group needs a spike, and the list grows.
- **The floor rule with a bar of 1.** It keeps the hours, and the floor changes no verdict.
- **Fewer rounds or objects in zodbshootout.** The cost is the start of each value, which stays in minutes.
- **More values without interleaving.** A drift of the machine falls on one storage.
- **A margin above 1, such as 1.25.** It asks more than the phase needs.
- **A fixed number of pairs.** A clear cell waits for the noisy ones, and a near tie gets a verdict by chance.

## Merged ADRs

- ADR 0037, the phase 3 gate counts the cost of the storage against a floor: its facts are in Context, and its rule and its floors are in Alternatives, "The cost over a floor, at 3×". This ADR replaces its decision.
