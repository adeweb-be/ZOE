"""POC ZOE-187 (not for merge): the A/B of two labels of zoe-native.

For each cell: the median ops/s of the labels A and B, the paired ratio
B/A per round (the k-th value of A with the k-th value of B: median, min,
max), and the ratio of each to the median of zeo under A, with the time
per object of the slowest worker.

    python bench/poc/ZOE-187/abs.py FILE.jsonl A B
"""

import json
import statistics
import sys
from collections import defaultdict


def main(path, a, b):
    rows = [json.loads(line) for line in open(path) if line.strip()]
    ops = defaultdict(list)
    for r in rows:
        ops[(r["label"], r["storage"], r["mode"], r["c"], r["size"])].append(r["ops"])
    cells = sorted({(r["mode"], r["c"], r["size"]) for r in rows})
    print(f"{'cell':<22} {'zeo':>8} {a:>9} {b:>9} {'B/A med':>8} {'min':>5} {'max':>5}"
          f" {'A/zeo':>6} {'B/zeo':>6} {'A us':>7} {'B us':>7}")
    for mode, c, size in cells:
        zeo = ops.get((a, "zeo", mode, c, size), [])
        va = ops.get((a, "zoe-native", mode, c, size), [])
        vb = ops.get((b, "zoe-native", mode, c, size), [])
        if not va or not vb:
            continue
        pairs = [y / x for x, y in zip(va, vb)]
        ma, mb = statistics.median(va), statistics.median(vb)
        mz = statistics.median(zeo) if zeo else float("nan")
        print(f"{mode + ' ' + str(c) + ' ' + str(size // 1024) + 'K':<22} {mz:8.0f} {ma:9.0f} {mb:9.0f}"
              f" {statistics.median(pairs):8.2f} {min(pairs):5.2f} {max(pairs):5.2f}"
              f" {ma / mz:6.2f} {mb / mz:6.2f} {c / ma * 1e6:7.1f} {c / mb * 1e6:7.1f}"
              f"  n={len(pairs)}")


if __name__ == "__main__":
    main(*sys.argv[1:4])
