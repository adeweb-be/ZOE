"""Print the tables of the finding from the JSON reports of the driver."""

import json
import sys

MB = 1e6


def build(r, label):
    b = r["build"]
    print(f"\n## build {label}: {b['seconds']:.1f} s, {b['threads']} threads, {b['records']:,} records, "
          f"{b['bytes'] / MB:.0f} MB, {b['rows']:,} rows, open {b['open_seconds']:.2f} s, "
          f"RSS open {b['rss_open'] / MB:.0f} MB built {b['rss_built'] / MB:.0f} MB peak {b['rss_peak'] / MB:.0f} MB")
    parts = sorted(b["parts"], key=lambda p: -(p["walk_s"] + p["build_s"]))
    print("| part | records | MB | walk s | build s |\n| --- | ---: | ---: | ---: | ---: |")
    for p in parts[:12]:
        print(f"| {p['name']} | {p['records']:,} | {p['bytes'] / MB:.1f} | {p['walk_s']:.2f} | {p['build_s']:.2f} |")
    tot_walk = sum(p["walk_s"] for p in b["parts"])
    tot_build = sum(p["build_s"] for p in b["parts"])
    print(f"sum walk {tot_walk:.1f} s, sum build {tot_build:.1f} s")


def memory(r):
    m = r["memory"]
    print(f"\n## memory: {m['total'] / MB:.0f} MB, {m['per_entry']:.0f} B/entry; RSS mirror {r['rss_mirror'] / MB:.0f} MB; shapes {r['shapes_bytes'] / MB:.0f} MB")
    for k, v in sorted(m["by_kind"].items(), key=lambda x: -x[1]):
        print(f"- {k}: {v / MB:.1f} MB")
    parts = sorted(m["parts"], key=lambda p: -p["bytes"])
    print("| part | kind | MB |\n| --- | --- | ---: |")
    for p in parts[:25]:
        print(f"| {p['name']} | {p['kind']} | {p['bytes'] / MB:.1f} |")
    for u in m["unavailable"]:
        print("unavailable:", u["name"], u["class"], u["reason"])


def queries(r):
    q = r.get("queries")
    if not q:
        return
    print(f"\n## queries: checked {q['checked']}, mismatches {q['mismatches']}, fallbacks {q['fallbacks']}")
    print("| query | check | len | actual | p50 ms | p99 ms | p99 with rows |\n| --- | --- | ---: | ---: | ---: | ---: | ---: |")
    for p in q["plans"]:
        t = p["timing"] or {}
        tr = p["timing_with_rows"] or {}
        check = p["check"] if p["check"] != "fallback" else f"fallback: {p['fallback']}"
        print(f"| {p['name']} | {check} | {p['len']} | {p['actual']} | {t.get('p50_ms', 0):.3f} | {t.get('p99_ms', 0):.3f} | {tr.get('p99_ms', 0):.3f} |")


def maintenance(r):
    print("\n## maintenance")
    print("| shape | rows | records | MB | changes | apply p50 ms | p99 ms | overlay read p50 | overlay+listing p50 | p99 |\n| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |")
    for m in r["maintenance"]:
        o = m["overlay_read"] or {}
        oq = m["overlay_read_and_listing"] or {}
        print(f"| {m['shape']} | {m['rows_per_commit']} | {m['records_mean']:.1f} | {m['bytes_mean'] / MB:.3f} | {m['changes_mean']:.1f} | "
              f"{m['apply']['p50_ms']:.3f} | {m['apply']['p99_ms']:.3f} | {o.get('p50_ms', 0):.3f} | {oq.get('p50_ms', 0):.3f} | {oq.get('p99_ms', 0):.3f} |")
    print("stale:", r.get("stale"))
    v = r["versions"]
    print(f"\n## versions: {v['deltas']} deltas, {v['deltas_bytes'] / 1e3:.0f} KB, {v['bytes_per_delta']:.0f} B/delta, head rows {v['head_rows']}")
    for k in ("patch_at_minus_1_ms", "patch_at_minus_10_ms", "patch_at_minus_100_ms"):
        print(f"- {k}: {v[k]:.3f}")
    for p in v.get("queries_at_head", []):
        print(f"- at head {p['name']}: p50 {p['timing']['p50_ms']:.3f} p99 {p['timing']['p99_ms']:.3f}")


def overlays(r):
    for o in r.get("overlays", []):
        print(f"\n## overlay {o['name']}: {o['records']} records, {o['bytes'] / 1e3:.1f} KB, read {o['read_ms']:.2f} ms")
        for c in o["checks"]:
            print("  ", c)


for path in sys.argv[1:]:
    r = json.load(open(path))
    print(f"\n# {path}")
    build(r, path)
    memory(r)
    queries(r)
    overlays(r)
    maintenance(r)
    print(f"\nRSS end {r['rss_end'] / MB:.0f} MB, peak {r['rss_peak'] / MB:.0f} MB")
