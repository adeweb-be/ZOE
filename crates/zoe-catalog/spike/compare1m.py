"""Compare the compatibility mode with ZCTextIndex on catalog-1m.

Usage: python compare1m.py ZCQUERY COMPAT1M FIXTURE_PATHS PLAN_PATHS

The rids of the fixture are those of ZCatalog; the rids of the prototype
are the positions of the entries of the plan. The paths map one to the
other. For each query: the count, the sum of the scores, and the 200
first in the order of `IIBucket.byValue(0)`, with the ties broken by the
rid of the fixture.
"""

import json
import sys

zc_path, rs_path, fixture_paths, plan_paths = sys.argv[1:5]
by_path = {}
for line in open(fixture_paths):
    rid, path = line.rstrip("\n").split("\t")
    by_path[path] = int(rid)
n_to_rid = {}
for line in open(plan_paths):
    n, path = line.rstrip("\n").split("\t")
    n_to_rid[int(n)] = by_path[path]
zc = [json.loads(line) for line in open(zc_path)]
rs = [json.loads(line) for line in open(rs_path)]
kinds = {}
for a, b in zip(zc, rs, strict=True):
    assert a["query"] == b["query"]
    head = sorted(((n_to_rid[n], s) for n, s in b["head"]), key=lambda p: (-p[1], -p[0]))[:200]
    first = [tuple(p) for p in a["first"]]
    if (a["count"], a["sum"], first) == (b["count"], b["sum"], head):
        kind = "identical"
    elif a["count"] != b["count"]:
        kind = "count differs"
    elif a["sum"] != b["sum"]:
        kind = "scores differ"
    else:
        kind = "order differs"
    kinds[kind] = kinds.get(kind, 0) + 1
    if kind != "identical":
        print(kind, a["index"], a["query"], a["count"], b["count"], a["sum"], b["sum"], first[:3], head[:3])
zc_s = sorted(q["seconds"] * 1000 for q in zc)
print(json.dumps({"queries": len(zc), "kinds": kinds, "zctextindex_ms_p50": zc_s[len(zc_s) // 2], "zctextindex_ms_max": zc_s[-1]}))
