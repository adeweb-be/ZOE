"""Summarize the JSON lines of racex.py: runs and failures for each cell."""

import collections
import json
import sys

cells = collections.defaultdict(lambda: [0, 0])
gil = collections.defaultdict(set)
persistent = set()
for path in sys.argv[1:]:
    for line in open(path):
        r = json.loads(line)
        key = (r["python"], r["test"], r["pool"], r["rule"])
        cells[key][0] += 1
        cells[key][1] += r["status"] == "fail"
        gil[r["python"]].update(r["gil_enabled_by"])
        gil[r["python"]].add(f"gil_enabled={r['gil_enabled']}")
        persistent.add(r["persistent"])
print(f"{'python':9} {'test':45} {'pool':>4} {'rule':4} {'runs':>5} {'fail':>5}")
for (py, test, pool, rule), (runs, fails) in sorted(cells.items()):
    print(f"{py:9} {test:45} {pool:>4} {rule:4} {runs:>5} {fails:>5}")
print("persistent:", sorted(persistent))
for py, mods in sorted(gil.items()):
    print("GIL", py, sorted(mods))
