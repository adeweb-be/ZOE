"""Summarize the JSON lines of ab-concurrent.sh: mean and spread for each cell."""

import collections
import json
import sys

LABELS = ["one-connection", "pool-without-rule", "pool-with-rule"]
cells = collections.defaultdict(list)
cores = collections.defaultdict(list)
for line in open(sys.argv[1]):
    r = json.loads(line)
    for scenario in ("add", "update"):
        if scenario in r:
            cells[(scenario, r["c"], r["label"])].append(r[scenario]["ops"])
            cores[(scenario, r["c"], r["label"])].append(r["client_cores"])
print("| scenario | concurrency | " + " | ".join(LABELS) + " |")
for scenario in ("add", "update"):
    for c in (16, 64):
        row = []
        for label in LABELS:
            v = cells[(scenario, c, label)]
            k = cores[(scenario, c, label)]
            row.append(
                f"{sum(v) / len(v):,.0f} ({min(v):,}-{max(v):,}, n={len(v)}, cores {sum(k) / len(k):.1f})"
            )
        print(f"| {scenario} | {c} | " + " | ".join(row) + " |")
