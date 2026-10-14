"""Tables of the A/B scripts of driver.py (ab-client.sh, ab-relstorage.sh). Not for merge.

For each cell: the mean ops/s of the rounds, the ratio to ZEO with its stock
client, and the mean CPU seconds of the client and of the server in a run.

    python summarize_client.py results/ab-client.jsonl

The ratio is against the label zeo/stock. The rows keep the order of the
labels in the file.
"""

import json
import sys
from collections import defaultdict

KEYS = {"add": "add", "update": "update", "read_cold": "read_cold"}


def main():
    cells = defaultdict(list)
    order = []
    for line in open(sys.argv[1]):
        r = json.loads(line)
        if r["label"] not in order:
            order.append(r["label"])
        for k in KEYS:
            if k in r:
                cells[(r["mode"], k, r["label"])].append(r)
    for mode in ("threads", "processes"):
        print(f"\n### {mode}\n")
        print("| config | add ops/s | update ops/s | read_cold ops/s | client CPU s (add / update / cold) | server CPU s (add / update / cold) |")
        print("|---|---|---|---|---|---|")
        for label in order:
            row, cpu, srv = [], [], []
            for k in KEYS:
                runs = cells.get((mode, k, label), [])
                base = cells.get((mode, k, "zeo/stock"), [])
                if not runs:
                    row.append("")
                    cpu.append("")
                    srv.append("")
                    continue
                ops = sum(r[k]["ops"] for r in runs) / len(runs)
                ref = sum(r[k]["ops"] for r in base) / len(base) if base else 0
                spread = f" ({min(r[k]['ops'] for r in runs):,}–{max(r[k]['ops'] for r in runs):,})" if len(runs) > 1 else ""
                row.append(f"{ops:,.0f} ({ops / ref:.2f}×){spread}" if ref else f"{ops:,.0f}")
                cpu.append(f"{sum(r['client_cpu_s'] for r in runs) / len(runs):.1f}")
                srv.append(f"{sum(r['server_cpu_s'] for r in runs) / len(runs):.1f}")
            print(f"| {label} | " + " | ".join(row) + " | " + " / ".join(cpu) + " | " + " / ".join(srv) + " |")


if __name__ == "__main__":
    main()
