# ZOE-152: the prototype of the client cache

A proof of concept for the spike ZOE-152. Not for merge. The finding is
`docs/spikes/client-cache.md` on `main`.

| Path | Holds |
| --- | --- |
| `cachex/src/cache.rs` | The layout: a header page, an index, a ring of checksummed entries in one mapping; `Session`, the view of one process |
| `cachex/src/lib.rs` | `_cachex.Cache`, a PyO3 module with the methods of `ZEO.cache.ClientCache` that `ZEO.ClientStorage` calls (feature `python`) |
| `cachex/src/bin/sim.rs` | `sim run`: worker processes on one shared file against a model of the history, with SIGKILLs; `sim damage`: bit flips, cut files, power-loss page mixes |
| `cachex/src/bin/micro.rs` | zstd on real pickles, the lookup without Python, the writer lock |
| `hit.py` | The cost of a hit through ZODB and the stock `ZEO.ClientStorage`, `ZEO.cache` against the prototype |
| `verify.py` | ZEO's `verify` and lock, run by the stock client on the prototype |
| `dump_pickles.py` | The pickles of zodbshootout's objects, for `micro` |
| `timing.sh` | The timing runs of the finding |
| `results/` | Raw results |

Build in `devenv shell`: `cachex/build.sh` (it uses `target/bench/venv-3.13`
from `just bench test`, or `POC_PYTHON`). Canaries: `CACHEX_NO_SEEN_RULE=1`
turns off the rule for current stores, `CACHEX_NO_BOOT_RULE=1` the boot rule.

## After the review

- `Session::clear_and_apply`: a verification that finds the cache too old
  clears it and moves `applied` in one hold of the writer lock.
  `CACHEX_TWO_HOLDS=1` makes `sim run` use two holds instead (a canary).
- `Session::store_seen`: a reply is stored with the tid up to which the
  process had read its stream when the reply arrived. In `sim run`, one reply
  in four is stored after the worker read up to 4 more tids.
  `CACHEX_SEEN_AT_STORE=1` takes the tid of the store instead (a canary).
- `sim run` counts the branch of each verification, the kills that landed
  while the victim held the writer lock (`--kill-in-lock 1` waits up to 5 ms
  for that before each kill), and the bad entries that readers met.
  `--too-old 0` sends every joiner with a gap through the too-old branch.
- `sim race`: the two races of the review, in a fixed order, with and without
  the rule (`results/race.jsonl`).
- `hit.py`: `Proto.getLastTid` and `setLastTid` take `_lock`, as ZEO's do.
- `results/sim2.jsonl`: the runs after these changes, 40,000 tids each.
