"""Tables of the client A/B (ab-client.sh). Not for merge.

For each cell: the mean ops/s of the rounds, the ratio to ZEO with its stock
client, and the client CPU per object (both modes) and per run.

    python summarize_client.py results/ab-client.jsonl
"""

import json
import sys
from collections import defaultdict

KEYS = {"add": "add", "update": "update", "read_cold": "read_cold"}
ORDER = ["zeo/stock", "zeo-msgpack/rust", "zoe-base/stock", "zoe-base/rust", "zoe-best/stock", "zoe-best/rust"]


def main():
    cells = defaultdict(list)
    for line in open(sys.argv[1]):
        r = json.loads(line)
        for k in KEYS:
            if k in r:
                cells[(r["mode"], k, r["label"])].append(r)
    for mode in ("threads", "processes"):
        print(f"\n### {mode}\n")
        print("| config | add ops/s | update ops/s | read_cold ops/s | client CPU s (add / update / cold) |")
        print("|---|---|---|---|---|")
        for label in ORDER:
            row, cpu = [], []
            for k in KEYS:
                runs = cells.get((mode, k, label), [])
                base = cells.get((mode, k, "zeo/stock"), [])
                if not runs:
                    row.append("")
                    cpu.append("")
                    continue
                ops = sum(r[k]["ops"] for r in runs) / len(runs)
                ref = sum(r[k]["ops"] for r in base) / len(base) if base else 0
                spread = f" ({min(r[k]['ops'] for r in runs):,}–{max(r[k]['ops'] for r in runs):,})" if len(runs) > 1 else ""
                row.append(f"{ops:,.0f} ({ops / ref:.2f}×){spread}" if ref else f"{ops:,.0f}")
                cpu.append(f"{sum(r['client_cpu_s'] for r in runs) / len(runs):.1f}")
            print(f"| {label} | " + " | ".join(row) + " | " + " / ".join(cpu) + " |")


if __name__ == "__main__":
    main()
