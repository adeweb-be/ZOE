"""POC ZOE-187 (not for merge): read the traces of `drive.py --trace`.

For each value directory: one line per worker with the median time per
object of its loops, the CPUs of its loops (P for 0-11, E for 12-21 on the
reference laptop), its CPU time per object, context switches and minor
page faults per loop, the change of the cache counters over its loops, and
the share of its timed time during which another worker of the same call
was opening or priming (the overlap with startup). Then the terms of a hit
when the run had --terms.

    python bench/poc/ZOE-187/trace.py TRACE_DIR...
"""

import json
import statistics
import sys
from pathlib import Path

P_CORES = range(0, 12)


def kind(cpu):
    return "P" if cpu in P_CORES else "E"


def overlap(loop, others):
    """Return the seconds of loop during which another worker was in open or prime."""
    total = 0.0
    for o in others:
        lo, hi = max(loop["t0"], o["open0"]), min(loop["t1"], o["primed"])
        total += max(0.0, hi - lo)
    return total


def calls(records):
    """Split the records of a value into its calls (warmup, value) by start."""
    records = sorted(records, key=lambda r: r["open0"])
    groups, current = [], []
    for r in records:
        if current and r["open0"] > max(c["end"] for c in current):
            groups.append(current)
            current = []
        current.append(r)
    if current:
        groups.append(current)
    return groups


def stat_delta(loops):
    first, last = loops[0]["stats0"], loops[-1]["stats1"]
    out = {}
    for key, value in last.items():
        if isinstance(value, (int, float)) and key in first:
            if value != first[key]:
                out[key] = value - first[key]
    if first.get("last") != last.get("last"):
        out["last_tid_moved"] = True
    return out


def main(dirs):
    for root in map(Path, dirs):
        for valuedir in sorted(p for p in root.iterdir() if p.is_dir()) or [root]:
            records = [json.loads(f.read_text()) for f in valuedir.glob("*.json")]
            if not records:
                continue
            print(f"== {valuedir.name}: {len(records)} worker records")
            for n, group in enumerate(calls(records)):
                print(f"  call {n} ({'warmup' if n == 0 else 'value'}), {len(group)} workers")
                slowest = 0
                for r in sorted(group, key=lambda r: r["worker"]):
                    loops = r["loops"]
                    if not loops:
                        continue
                    others = [o for o in group if o is not r]
                    per = [(lp["t1"] - lp["t0"]) / 100 * 1e6 for lp in loops]
                    timed = sum(lp["t1"] - lp["t0"] for lp in loops)
                    busy = sum(overlap(lp, others) for lp in loops)
                    cpu = [(lp["usage"][0] + lp["usage"][1]) / 100 * 1e6 for lp in loops]
                    cs = [lp["usage"][2] + lp["usage"][3] for lp in loops]
                    flt = [lp["usage"][4] for lp in loops]
                    kinds = "".join(sorted({kind(lp["cpu0"]) for lp in loops}))
                    mean = timed / len(loops) / 100 * 1e6
                    slowest = max(slowest, mean)
                    print(
                        f"    w{r['worker']:<2} mean {mean:7.1f} us/obj  median {statistics.median(per):7.1f}"
                        f"  cpu {statistics.median(cpu):6.1f} us/obj  cores {kinds:<2}"
                        f"  cs/loop {statistics.median(cs):4.0f}  minflt/loop {statistics.median(flt):5.0f}"
                        f"  startup-overlap {busy / timed:5.0%} (workers x time)"
                        f"  {stat_delta(loops)}"
                    )
                    if "terms" in r:
                        t = r["terms"]
                        print("        terms us/obj: " + "  ".join(
                            f"{k} {v * 1e6:.2f}" for k, v in t.items() if k != "bytes"))
                print(f"    slowest worker mean: {slowest:.1f} us/obj")


if __name__ == "__main__":
    main(sys.argv[1:])
