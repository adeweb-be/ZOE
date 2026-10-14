"""Turn the JSON lines of run.py into the tables of the finding. Not for merge.

    python summarize.py results/full.jsonl

For each Python build, scenario and cell, it takes the median of the rounds
of each configuration. A round that other load disturbed counts only when
no attempt of that round was clean. It prints:

- the table of the finding: zeo, poc (the prototypes) and their ratio;
- the same cells for `zoe serve` as on main, and the spread of the rounds;
- the terms of one transaction: the floor, the client CPU, the server
  steps (from /metrics and the spans of the server);
- the GIL, the governor and the frequency of each run.
"""

import argparse
import json
import statistics
from collections import defaultdict

ORDER = ["add", "update", "read_cold", "read_warm", "read_hot"]
GATE = 3.0


def major(python):
    return "3.14t" if python.endswith("t") else ".".join(python.split(".")[:2])


def size_text(size):
    return f"{size // 1024} KB"


def load(path, limit):
    runs = [json.loads(line) for line in open(path) if line.strip()]
    rounds = defaultdict(list)
    for r in runs:
        if r.get("tag") not in ("main", "floor", "spans", "pyspy"):
            continue
        # One run holds every scenario of a cell. Split it into one record
        # per scenario; the CPU and the counters stay those of the run.
        for name, sc in (r.get("scenarios") or {"-": {}}).items():
            one = {**r, **sc, "scenario": name, "run_objects": sum(r.get("objects", {}).values())}
            if not r.get("ok"):
                one["ok"] = False
            rounds[(r["tag"], major(r["python"]), name, r["mode"], r["c"], r["size"], r["config"], r["round"])].append(one)
    picked = {}
    for k, attempts in rounds.items():
        ok = [a for a in attempts if a.get("ok")]
        clean = [a for a in ok if a.get("foreign_cores", 0) <= limit]
        if clean or ok:
            picked[k] = (clean or ok)[-1]
            picked[k]["disturbed"] = not clean
    return runs, picked


def cells(picked, tag, config):
    out = defaultdict(list)
    for (t, py, sc, mode, c, size, cfg, _), r in picked.items():
        if t == tag and cfg == config:
            out[(py, sc, mode, c, size)].append(r)
    return out


def med(rs, field="ops"):
    return statistics.median(r[field] for r in rs)


def spread(rs):
    values = [r["ops"] for r in rs]
    m = statistics.median(values)
    return (max(values) - min(values)) / m * 100 if m else 0


def key_order(k):
    py, sc, mode, c, size = k
    return (py, ORDER.index(sc), mode != "threads", size, c)


def per_call_objects(r):
    # The client CPU is that of the whole run: every scenario of the cell.
    return r["run_objects"]


def main():
    p = argparse.ArgumentParser()
    p.add_argument("results")
    p.add_argument("--foreign-limit", type=float, default=1.5)
    p.add_argument("--json", help="write the per-cell numbers here")
    args = p.parse_args()
    runs, picked = load(args.results, args.foreign_limit)
    zeo = cells(picked, "main", "zeo")
    main_ = cells(picked, "main", "poc-main")
    proto = cells(picked, "main", "poc-proto")
    floor = cells(picked, "floor", "floor")
    spans = cells(picked, "spans", "poc-proto")
    spans_main = cells(picked, "spans", "poc-main")
    keys = sorted(set(zeo) & set(proto), key=key_order)

    print("| scenario | mode | concurrency | object size | python | zeo ops/s | poc ops/s | ratio | term |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    for k in keys:
        py, sc, mode, c, size = k
        z, q = med(zeo[k]), med(proto[k])
        print(f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {z:,.0f} | {q:,.0f} | {q / z:.2f} | |")

    print("\n| scenario | mode | c | size | python | rounds | zeo spread | poc spread | poc-main ops/s | main ratio | main spread | floor ops/s | disturbed |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    data = {}
    for k in keys:
        py, sc, mode, c, size = k
        z = med(zeo[k])
        m = main_.get(k)
        f = floor.get(k)
        dist = sum(r["disturbed"] for r in zeo[k] + proto[k] + (m or []))
        main_cells = f"{med(m):,.0f} | {med(m) / z:.2f} | {spread(m):.0f} %" if m else "- | - | -"
        floor_cell = f"{med(f):,.0f}" if f else "-"
        print(
            f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {len(zeo[k])}/{len(proto[k])}/{len(m or [])} "
            f"| {spread(zeo[k]):.0f} % | {spread(proto[k]):.0f} % | {main_cells} | {floor_cell} | {dist} |"
        )
        row = {
            "zeo_ops": z, "poc_ops": med(proto[k]), "main_ops": med(m) if m else None,
            "floor_ops": med(f) if f else None,
            "zeo_ms": med(zeo[k], "mean_ms"), "poc_ms": med(proto[k], "mean_ms"),
            "main_ms": med(m, "mean_ms") if m else None, "floor_ms": med(f, "mean_ms") if f else None,
        }
        for name, rs in (("zeo", zeo[k]), ("poc", proto[k]), ("main", m), ("floor", f)):
            if not rs:
                continue
            objs = per_call_objects(rs[0])
            row[f"{name}_client_us_per_object"] = statistics.median(r["client_cpu_s"] for r in rs) / objs * 1e6
            row[f"{name}_server_us_per_object"] = statistics.median(r["server_cpu_s"] for r in rs) / objs * 1e6
            for field in ("vote_ms", "finish_ms", "batch"):
                if rs[0].get(field) is not None:
                    row[f"{name}_{field}"] = statistics.median(r.get(field) or 0 for r in rs)
        s = spans.get(k)
        if s:
            row["spans_proto"] = s[0].get("spans")
            row["poc_counters"] = s[0].get("poc")
        s = spans_main.get(k)
        if s:
            row["spans_main"] = s[0].get("spans")
        data["|".join(map(str, k))] = row

    print("\n| scenario | mode | c | size | python | zeo ms/txn | poc ms/txn | floor ms/txn | 3x ms/txn | client µs/obj zeo | client µs/obj poc | client µs/obj floor | vote ms | finish ms | batch |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    for k in keys:
        row = data["|".join(map(str, k))]
        py, sc, mode, c, size = k

        def f(x, fmt="{:.2f}"):
            return fmt.format(x) if x is not None else "-"

        print(
            f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {f(row['zeo_ms'])} | {f(row['poc_ms'])} "
            f"| {f(row['floor_ms'])} | {f(row['zeo_ms'] / GATE)} | {f(row.get('zeo_client_us_per_object'), '{:.1f}')} "
            f"| {f(row.get('poc_client_us_per_object'), '{:.1f}')} | {f(row.get('floor_client_us_per_object'), '{:.1f}')} "
            f"| {f(row.get('poc_vote_ms'), '{:.3f}')} | {f(row.get('poc_finish_ms'), '{:.3f}')} | {f(row.get('poc_batch'))} |"
        )

    print("\nGIL, governor and frequency")
    gil = defaultdict(set)
    gov = set()
    mhz = []
    for r in runs:
        if r.get("tag") in ("main", "floor"):
            gil[major(r["python"])].update(r.get("gil_enabled_by", []))
            for state in (r.get("cpu_before"), r.get("cpu_after")):
                if state:
                    gov.update(state["governor"])
                    if state.get("mhz_mean"):
                        mhz.append(state["mhz_mean"])
    for py, mods in sorted(gil.items()):
        print(f"- {py}: GIL turned back on by {sorted(mods) or 'no module'}")
    if mhz:
        print(f"- governor {sorted(gov)}; mean frequency of all CPUs at the start and end of the runs: median {statistics.median(mhz)} MHz, {min(mhz)} to {max(mhz)} MHz")
    print(f"- runs: {len(runs)}, disturbed runs kept in the raw results: {sum(1 for r in runs if r.get('foreign_cores', 0) > args.foreign_limit)}")
    if args.json:
        with open(args.json, "w") as fh:
            json.dump(data, fh, indent=1)


if __name__ == "__main__":
    main()
