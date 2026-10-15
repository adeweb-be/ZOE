"""POC ZOE-187 (not for merge): time per object by core type, from traces.

For each storage and trace directory: the median time per object of the
loops that ran on P-cores only (0-11) and of the loops that ran on E-cores
(12-21), the median of the slowest worker of each value call, and the
median startup overlap of the loops.

    python bench/poc/ZOE-187/cores.py TRACE_DIR...
"""
import json, statistics, sys
from collections import defaultdict
from pathlib import Path

sys.path.insert(0, str(Path(__file__).parent))
from trace import calls, overlap  # noqa: E402

for root in map(Path, sys.argv[1:]):
    acc = defaultdict(lambda: defaultdict(list))
    for valuedir in sorted(p for p in root.iterdir() if p.is_dir()):
        storage = valuedir.name.split("-processes")[0].split("-threads")[0]
        records = [json.loads(f.read_text()) for f in valuedir.glob("*.json")]
        for n, group in enumerate(calls(records)):
            slow = 0
            for r in group:
                others = [o for o in group if o is not r]
                means = []
                for lp in r["loops"]:
                    us = (lp["t1"] - lp["t0"]) / 100 * 1e6
                    p = lp["cpu0"] < 12 and lp["cpu1"] < 12
                    e = lp["cpu0"] >= 12 and lp["cpu1"] >= 12
                    acc[storage]["P" if p else "E" if e else "mixed"].append(us)
                    if "mhz" in lp and (p or e):
                        acc[storage]["mhz" + ("P" if p else "E")].append(lp["mhz"])
                    acc[storage]["cpu"].append((lp["usage"][0] + lp["usage"][1]) / 100 * 1e6 / us)
                    acc[storage]["overlap"].append(overlap(lp, others) / (lp["t1"] - lp["t0"]))
                    means.append(us)
                if means:
                    slow = max(slow, statistics.fmean(means))
            acc[storage]["slowest"].append(slow)
    print(f"== {root}")
    for storage, d in sorted(acc.items()):
        med = lambda k: f"{statistics.median(d[k]):6.1f}" if d[k] else "     -"
        print(f"  {storage:<11} P-core loops {med('P')} us/obj (n={len(d['P'])})  E-core loops {med('E')} (n={len(d['E'])})"
              f"  mixed {med('mixed')} (n={len(d['mixed'])})  slowest worker {med('slowest')}"
              f"  cpu/wall {statistics.median(d['cpu']):.2f}  overlap {statistics.median(d['overlap']):.2f}"
              f"  MHz P {med('mhzP')} E {med('mhzE')}")
