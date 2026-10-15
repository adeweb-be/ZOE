"""POC ZOE-187 (not for merge): summarise the JSON lines of drive.py.

For each label, storage and cell: the number of values, the median, min
and max of ops/s, the spread ((max - min) / median), and the median time
per object of the slowest worker in microseconds. Then, for each label and
cell, the ratio of the median of each storage to the median of zeo.

    python bench/poc/ZOE-187/summ.py FILE.jsonl...
"""

import json
import statistics
import sys
from collections import defaultdict


def main(paths):
    rows = [json.loads(line) for path in paths for line in open(path) if line.strip()]
    groups = defaultdict(list)
    loads = defaultdict(list)
    for r in rows:
        key = (r["label"], r["mode"], r["c"], r["size"], r["storage"])
        groups[key].append(r["ops"])
        if r.get("server_loads") is not None:
            loads[key].append(r["server_loads"])
    fmt = "{:<16} {:<9} {:>3} {:>6} {:<11} {:>2} {:>9} {:>9} {:>9} {:>6} {:>8} {:>8} {}"
    print(fmt.format("label", "mode", "c", "size", "storage", "n", "median", "min", "max",
                     "spread", "us/obj", "vs zeo", "server loads"))
    for key in sorted(groups):
        label, mode, c, size, storage = key
        ops = groups[key]
        med = statistics.median(ops)
        zeo = groups.get((label, mode, c, size, "zeo"))
        ratio = f"{med / statistics.median(zeo):.2f}" if zeo else "-"
        us = c * 100 / med / 100 * 1e6
        print(fmt.format(label, mode, c, size // 1024 * 1024 and f"{size // 1024}K", storage,
                         len(ops), f"{med:.0f}", f"{min(ops):.0f}", f"{max(ops):.0f}",
                         f"{(max(ops) - min(ops)) / med:.0%}", f"{us:.1f}", ratio,
                         loads.get(key, "")))


if __name__ == "__main__":
    main(sys.argv[1:])
