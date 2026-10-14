"""The table of the finding, with the term of each row. Not for merge.

    python terms.py results/aws/run.jsonl

It reads the runs as summarize.py does and names, for each row, the term
that keeps the ratio of the POC client under 3x, by these rules:

- read_hot: the POC client has no cache, so each hit goes to the server.
  A hit through ZEO.cache at concurrency 1 is almost all ZODB (py-spy), so
  its time bounds any cache: the client CPUs (one core under the GIL)
  divided by that time. When the bound is under 3x ZEO, no card can.
- the ZODB floor: when the in-process storage reaches less than 3x ZEO,
  no client and no server can.
- else, by the kind of the scenario and the size of the objects.
"""

import statistics
import sys

from summarize import cells, load, med, spread, key_order, size_text

GATE = 3.0


CLIENT_CPUS = 12


def term(k, z, q, f, proto, hit):
    py, sc, mode, c, size = k
    if sc == "read_hot":
        cores = 1 if (py == "3.13" and mode == "threads") else min(c, CLIENT_CPUS)
        bound = cores / hit[(py, size)] / z
        if bound < GATE:
            return f"ZODB per hit ({bound:.1f}x)", "no card can: ZODB alone takes the time of a hit"
        if mode == "processes":
            return "client cache", "ZOE-156, ZOE-162"
        return "client cache", "ZOE-156"
    floor_ratio = f / z if f else None
    floor_binds = floor_ratio is not None and floor_ratio < GATE and not (py == "3.14t" and mode == "threads")
    if floor_binds:
        return f"ZODB floor ({floor_ratio:.1f}x)", "no card can: ZODB and zodbshootout alone reach less than 3x ZEO"
    finish = statistics.median(r.get("finish_ms") or 0 for r in proto)
    if sc in ("add", "update", "read_warm"):
        if size > 1024:
            return f"100 KB commit (finish {finish:.0f} ms)", "new card: compression off the commit path"
        if mode == "threads" and c == 1:
            return f"commit latency (finish {finish:.1f} ms)", "new card: a cheaper redb finish"
        return f"commit (finish {finish:.1f} ms)", "new card: compression off the commit path, a cheaper redb finish"
    # read_cold
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
        table.append(f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {z:,.0f} | {q:,.0f} | {ratio:.2f} | {t} |")
        if ratio < GATE:
            fl = f"{f / z:.1f}" if f else "-"
            dist = sum(r["disturbed"] for r in zeo[k] + proto[k])
            under.append(
                f"| {sc} | {mode} | {c} | {size_text(size)} | {py} | {ratio:.2f} | {fl} "
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
