"""Turn results.jsonl into per-commit tables."""
import json, sys
rows = [json.loads(l) for l in open(sys.argv[1] if len(sys.argv) > 1 else "results.jsonl")]
def poc(r):
    if "poc" not in r: return {}
    return json.loads(r["poc"][4:])
print("| config | mode | scen | ops/s | mean ms | srv CPU s | vote ms | finish ms | batch | CPU ms/commit | disk KiB/commit | check µs/c | encode µs/c | insert µs/c | redb commit µs/c | leader µs/c | precomp µs/c |")
print("|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|---|")
for r in rows:
    sc = next(k for k in ("add", "update", "read_cold", "read_warm", "read_hot") if k in r)
    p = poc(r)
    b = p.get("batches") or 0
    v = p.get("votes") or 0
    per = lambda k, n: f'{p[k] / n / 1e3:.0f}' if n else ""
    print(f'| {r["label"]} | {r["mode"][:4]} | {sc} | {r[sc]["ops"]} | {r[sc]["mean_ms"]} | {r["server_cpu_s"]} | {r.get("vote_ms","")} | {r.get("finish_ms","")} | {r.get("batch","")} | {r.get("cpu_ms_per_commit") or (round(r["server_cpu_s"] / 1922 * 1e3, 2) if sc in ("add", "update") else "")} | {r.get("disk_kb_per_commit","")} | {per("vote_check_ns", v)} | {per("encode_ns", v)} | {per("insert_ns", v)} | {per("commit_ns", b)} | {per("leader_write_ns", b)} | {per("precompress_ns", v)} |')
