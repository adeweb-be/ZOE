"""Turn the JSON lines of walproto into the tables of the finding."""
import json, sys
from collections import defaultdict

rows = [json.loads(l) for l in open(sys.argv[1]) if l.startswith("{")]
best = {}
for r in rows:
    key = (r["scenario"], r["size"], r["c"], r["layout"])
    cur = best.get(key)
    if cur is None or r["finish_p50_us"] < cur["finish_p50_us"]:
        best[key] = r

def kb(size): return "1 KB" if size == 1024 else "100 KB"
def fmt(n): return f"{int(round(n)):,}"

print("| scenario | object size | concurrency | layout | bytes per commit | synced per commit | finish p50 | finish p99 | commits/s |")
print("| --- | --- | --- | --- | --- | --- | --- | --- | --- |")
for size in (1024, 102400):
    for scenario in ("add", "update"):
        for c in (1, 16, 64):
            for layout in ("main", "log", "log-nofold", "log-pre"):
                r = best.get((scenario, size, c, layout))
                if not r: continue
                if layout == "main":
                    by = r["bytes_per_commit"]; synced = by
                else:
                    by = r["log_bytes_per_commit"] + r["tree_bytes_per_commit"]; synced = r["log_bytes_per_commit"]
                print(f"| {scenario} | {kb(size)} | {c} | {layout} | {fmt(by)} | {fmt(synced)} | {fmt(r['finish_p50_us'])} µs | {fmt(r['finish_p99_us'])} µs | {fmt(r['commits_per_s'])} |")

print()
print("| scenario | object size | concurrency | layout | folds | commits per fold | records per second | amplification | fold ms (total / longest) | stalls |")
print("| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |")
for size in (1024, 102400):
    for scenario in ("add", "update"):
        for c in (1, 16, 64):
            for layout in ("log", "log-nofold", "log-pre"):
                r = best.get((scenario, size, c, layout))
                if not r: continue
                cp = r["cp"]
                per = cp["commits"] / cp["folds"] if cp["folds"] else 0
                print(f"| {scenario} | {kb(size)} | {c} | {layout} | {cp['folds']} | {per:.0f} | {fmt(cp['records_per_s'])} | {cp['amplification']:.2f} | {cp['fold_ms']:.0f} / {cp['longest_fold_ms']:.0f} | {r['stalls']} |")

import os
reads = sys.argv[1] + ".reads"
if os.path.exists(reads):
    print()
    print("| table records | record | main load p50 / p99 | tree load p50 / p99 (a miss, then the tree) | table load p50 / p99 (a hit) | folds during | writer commits |")
    print("| --- | --- | --- | --- | --- | --- | --- |")
    for l in open(reads):
        if not l.startswith("{"): continue
        r = json.loads(l)
        tag = " (under a running checkpoint)" if r["under_fold"] else ""
        print(f"| {r['table']:,}{tag} | {r['size']} B | {r['main_load_p50_us']:.1f} / {r['main_load_p99_us']:.1f} µs | {r['tree_load_p50_us']:.1f} / {r['tree_load_p99_us']:.1f} µs | {r['table_load_p50_us']:.1f} / {r['table_load_p99_us']:.1f} µs | {r['folds']} | {r['writer_commits']} |")
rec = sys.argv[1] + ".recovery"
if os.path.exists(rec):
    print()
    print("| log | commits | records | replay | replay per MB | open | fold after the open |")
    print("| --- | --- | --- | --- | --- | --- | --- |")
    for l in open(rec):
        if not l.startswith("{"): continue
        r = json.loads(l)
        print(f"| {r['log_mb']:.0f} MB | {r['commits']:,} | {r['records']:,} | {r['replay_ms']:.0f} ms | {r['replay_ms_per_mb']:.2f} ms | {r['open_ms']:.0f} ms | {r['fold_ms']:.0f} ms |")
