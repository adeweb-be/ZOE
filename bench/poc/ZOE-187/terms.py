"""POC ZOE-187 (not for merge): the median terms of a hit, from traces of --terms.

    python bench/poc/ZOE-187/terms.py TRACE_DIR...
"""
import json, statistics, sys
from collections import defaultdict
from pathlib import Path

for root in map(Path, sys.argv[1:]):
    acc = defaultdict(lambda: defaultdict(list))
    for valuedir in sorted(p for p in root.iterdir() if p.is_dir()):
        storage = valuedir.name.split("-processes")[0].split("-threads")[0]
        for f in valuedir.glob("*.json"):
            r = json.loads(f.read_text())
            for k, v in r.get("terms", {}).items():
                if k != "bytes":
                    acc[storage][k].append(v * 1e6)
    for storage, d in sorted(acc.items()):
        print(f"{root.name:<16} {storage:<11} " + "  ".join(
            f"{k} {statistics.median(v):6.2f}" for k, v in d.items()) + "  (us per object, median of workers)")
