# 0037. The phase 3 gate counts the cost of the storage against a floor

Status: superseded by ADR 0039. Accepted before that: the human chose the rule, the statistics and the end of ZOE-190 (#389, #390) after the finding of ZOE-187 (#385), and accepted this ADR with the noise threshold of 10 % and the comparison on one Python build. The harness builds it in ZOE-192 (the floor storage) and ZOE-193 (the rule and the report).

## Context

N4 sets the phase 3 gate: `zoe serve` through the native client at least 3× ZEO 6 through `ZEO.ClientStorage`, in every zodbshootout cell of the gate set but `read_hot` at 1 KB. The gate run on the cloud machine of the gate after #382 passes 4 of 27 cells. Three facts make the 3× of the whole time a poor gate:

- **Part of the time of a cell is not the storage.** The time of a cell holds ZODB, persistent, zodbpickle and the loop of zodbshootout as well as the client, the wire and the server. In `read_hot` at 100 KB, ZODB alone takes 17 µs per object at threads 1 against a target of 10.5 µs (ZEO 31.5 µs ÷ 3), and 82 to 92 µs at processes 16 against 42 µs (finding of ZOE-187). There, no storage reaches 3×. The phase 3 budget spike left `read_hot` at 1 KB out for the same reason, and ZOE-190 proposed to leave out more cells, one group at a time.
- **The floor that the spike measured is not a bound.** Its DemoStorage over a FileStorage took 44 µs per object in `read_hot` at threads 1 and 100 KB on the machine of the budget spike, and the native client takes 34 µs in the gate run of #382.
- **Three values decide nothing in the noisy cells.** A value of a processes cell spreads up to ±50 % on the laptop of ZOE-187. The 0.88× of #382 at processes 16 became 1.03 to 1.34× in five paired rounds. A fixed order of the variants gave a false regression of 0.83×. zodbshootout reports a time even when the benchmark function raises.

## Decision

**The gate compares the cost of each storage, not its whole time.** The harness measures a third storage, `floor`, in each cell. The cost of a storage is its time minus the time of the floor. `zoe-native` passes a cell when its cost is at most a third of the cost of ZEO:

    (t_zeo − t_floor) / (t_zoe − t_floor) ≥ 3

`t` is the time per object of the slowest worker, as zodbshootout and the harness count it today. A cost of `zoe-native` at or below 0 passes. When ZODB costs nothing, the rule is the 3× of the whole time. Where ZODB costs most of the time, the bar stays reachable: the cost of the storage can go to 0.

**The floor.** `floor` is a storage of the harness, in the zodbshootout process, with no server. It keeps each record in memory and gives back the same `bytes` that it stored, without a copy. It runs the two-phase commit with no lock across transactions: a lock is held only while `tpc_finish` takes the next tid and publishes the records. So the threads commit in parallel, as the pool of the native client does (ADR 0036). In the processes mode, each worker starts from the records of the populate, which the floor writes to a file of the value directory when the populate closes its DB. The floor is a measure, not a product: it never leaves the harness. Its time is ZODB, zodbshootout and the least work of a storage in Python.

**Seven values per cell, interleaved.** Each round of a cell runs one value of `zeo`, `zoe-native` and `floor`, each on a fresh store as today, in an order that rotates from round to round. A cell has 7 rounds. For each round the gate computes the ratio of the costs of that round. The verdict uses the median of the 7 ratios. The report prints the median and the range.

**A cell below the noise is reported, not gated.** When the cost of ZEO is below 10 % of the time of ZEO (the medians of the 7 rounds), the cell is printed with the verdict `below the noise` and counts neither as below nor as missing.

**The rule replaces the left-out cells of `zoe-native`.** `read_hot` counts in every size, by the same rule. ZOE-190 does not merge. The parity gate of phase 2, and every other storage, keep their rules.

**Each Python build is its own group, as `gate.py` does.** The floor runs in the interpreter of `zoe-native`, so its costs compare with ZEO of the same build. The phase 3 gate counts the results on CPython 3.14t, and the report of 3.13 is printed beside it.

## Consequences

- The gate set runs about 3.5 times the values of today: 7 values of 3 storages against 3 values of 2. A value of the floor needs no server. ZOE-193 measures the time of the gate set against the budget of a benchmark job (about 30 minutes), and stops and asks if it is above.
- Every cell of the gate set counts, unless it is below the noise. A cell bound by ZODB counts only the part that ZOE controls.
- The floor has a cost of its own, small but not 0. A storage cost is the cost beyond a minimal storage in Python, not beyond nothing.
- A change of ZODB or of zodbshootout moves the floor and both storages together. The bar does not move.
- The posted tables of the phase 3 gate (#313, #382) divided by ZEO's better build of 3.14t and 3.13. The new report uses the build of `zoe-native` only.

## Alternatives

- **A share of the reachable ratio:** pass when the ratio is at least `min(3, k × t_zeo ÷ t_floor)`. It needs a value of `k` that no measure gives.
- **3× of the whole time, with cells left out by hand** (ZOE-190). Each group needs a spike, and the list grows.
- **The diagnostic lookup of ZOE-187** (the native client that serves `loadBefore` from a dict of the process). It is a floor of reads only. It does not cover the commits, or the first read of a cold cell.
- **MappingStorage.** It holds its commit lock from `tpc_begin` to `tpc_finish`, so the threads commit one at a time. Its floor of the threads cells of `add` and `update` would be above the time of the native client.
- **DemoStorage over a read-only FileStorage** (the phase 3 budget spike). It reads the base file, and the native client is below it.
- **More values without interleaving.** A drift of the machine then falls on one storage.
