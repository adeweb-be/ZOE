"""The table of the finding, with the term of each row. Not for merge.

    python terms.py results/aws/run.jsonl

It reads the runs as summarize.py does and names, for each row, the term
that keeps the ratio of the POC client under 3x, by these rules, in order:

- read_hot: the POC client has no cache, so each hit goes to the server.
  At 1 KB, a hit through ZEO.cache at concurrency 1 is almost all ZODB
  (py-spy), so its time bounds any cache for each worker: the workers that
  run in parallel (one under the GIL of 3.13 in the threads mode, the
  client vCPUs in the threads mode of 3.14t, every worker in the processes
  mode) divided by that time. When the bound is under 3x ZEO, no card can.
  At 100 KB no profile exists, so the row is a client cache row.
- add in the processes mode: the reduced run measures the start of the
  workers, not the commits, so the bound is not known from this run.
- else, by the kind of the scenario and the size of the objects.

The floor of a cell bounds no row: above concurrency 1 it adds the
DemoStorage, the commit lock and the switching of the GIL to ZODB.
"""

import statistics
import sys

from summarize import cells, load, med, spread, key_order, size_text

GATE = 3.0


CLIENT_CPUS = 12


def fmt(ratio):
    # Two decimals keep a ratio under 0.5 within 5 % only with three.
    return f"{ratio:.2f}" if ratio >= 0.5 else f"{ratio:.3f}"


def term(k, z, q, f, proto, hit):
    py, sc, mode, c, size = k
    if sc == "read_hot":
        card = "ZOE-156, ZOE-162" if mode == "processes" else "ZOE-156"
        if size > 1024:
            return "client cache (no profile at 100 KB)", card
        if py == "3.13" and mode == "threads":
            workers = 1
        elif mode == "threads":
            workers = min(c, CLIENT_CPUS)
        else:
            workers = c
        bound = workers / hit[(py, size)] / z
        if bound < GATE:
            return f"ZODB per hit ({bound:.1f}x)", "no card can: ZODB alone takes the time of a hit (profile at 1 KB)"
        return "client cache", card
    if sc == "add" and mode == "processes":
        return "start-up of the workers (reduced run)", "not known from this run: ZOE-154 runs the full set"
    finish = statistics.median(r.get("finish_ms") or 0 for r in proto)
    if sc in ("add", "update", "read_warm"):
        if size > 1024:
            return f"100 KB commit (finish {finish:.0f} ms)", "new card: compression off the commit path"
        if mode == "threads" and c == 1:
            return f"commit latency (finish {finish:.1f} ms)", "new card: a cheaper redb finish"
        return f"commit (finish {finish:.1f} ms)", "new card: compression off the commit path, a cheaper redb finish"
    if size > 1024:
        return "100 KB load", "new card: the read path of the server"
    return "load round trip", "new card: the read path of the server; ZOE-153 for the client"


def main():
    runs, picked = load(sys.argv[1], 1.5)
    zeo = cells(picked, "main", "zeo")
    proto = cells(picked, "main", "poc-proto")
    floor = cells(picked, "floor", "floor")
    keys = sorted(set(zeo) & set(proto), key=key_order)
    # The time of one hit through ZEO.cache at concurrency 1, in seconds.
    hit = {(py, size): 1 / med(zeo[(py, "read_hot", "threads", 1, size)]) for py in ("3.13", "3.14t") for size in (1024, 102400)}
    table, under = [], []
    for k in keys:
        py, sc, mode, c, size = k
        z, q = med(zeo[k]), med(proto[k])
        f = med(floor[k]) if k in floor else None
        ratio = q / z
        t, card = ("-", "") if ratio >= GATE else term(k, z, q, f, proto[k], hit)
        table.append(f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {z:,.0f} | {q:,.0f} | {fmt(ratio)} | {t} |")
        if ratio < GATE:
            fl = f"{f / z:.1f}" if f else "-"
            dist = sum(r["disturbed"] for r in zeo[k] + proto[k])
            under.append(
                f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {fmt(ratio)} | {fl} "
                f"| {spread(zeo[k]):.0f} % / {spread(proto[k]):.0f} % | {t} | {card} |"
            )
    print("| scenario | mode | concurrency | object size | python | zeo ops/s | poc ops/s | ratio | term |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    print("\n".join(table))
    print()
    print("| scenario | mode | concurrency | object size | python | ratio | floor / zeo | spread zeo / poc | term | card |")
    print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
    print("\n".join(under))
    print(f"\n{len(table)} rows, {len(under)} under 3x")


if __name__ == "__main__":
    main()
