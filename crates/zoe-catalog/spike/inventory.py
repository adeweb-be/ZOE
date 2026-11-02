"""The inventory of question 6 from the probe records of Catalog.search.

    python3 inventory.py LABEL FILE... [-- LABEL FILE...]

For each group, the share of each index type, each query option, each
sort, the share that the prototype serves, the share in a transaction
that changed the catalog, and the share sorted on getObjPositionInParent.
"""

import collections
import json
import sys

MIRRORED = {"FieldIndex", "KeywordIndex", "DateIndex", "DateRangeIndex", "BooleanIndex", "UUIDIndex", "ExtendedPathIndex"}
SORTABLE = {"FieldIndex", "DateIndex", "UUIDIndex"}
SUPPORTED_OPTS = {"query", "range", "operator", "not", "depth", "navtree", "navtree_start", "level"}


def groups(argv):
    out, label, files = [], None, []
    for a in argv:
        if a == "--":
            out.append((label, files))
            label, files = None, []
        elif label is None:
            label = a
        else:
            files.append(a)
    if label:
        out.append((label, files))
    return out


def served(c, metas):
    """Why the prototype refuses a call, or None when it serves it."""
    q = c.get("query", {})
    for name, v in q.items():
        mt = v.get("meta_type")
        if mt is None:
            continue
        if mt not in MIRRORED:
            return f"index {mt}"
        opts = v["shape"].get("opts", {})
        bad = set(opts) - SUPPORTED_OPTS
        if bad:
            return f"option {sorted(bad)}"
        if mt in ("FieldIndex", "KeywordIndex", "DateIndex", "UUIDIndex"):
            if "range" in opts and opts.get("operator", "'or'") == "'and'":
                return "range with and"
    for s, t in zip(c.get("sort") or [], c.get("sort_types") or []):
        if t not in SORTABLE:
            return f"sort on {t}"
    if c.get("args") and len(c["args"]) > 3 and c["args"][3] == "False":
        return "merge false"
    return None


def report(label, files):
    calls = [json.loads(line) for f in files for line in open(f)]
    n = len(calls) or 1
    print(f"\n## {label}: {len(calls)} calls of Catalog.search\n")
    meta = collections.Counter()
    opts = collections.Counter()
    sorts = collections.Counter()
    sort_kind = collections.Counter()
    batch = collections.Counter()
    refused = collections.Counter()
    changed = 0
    changed_served = 0
    gopip = 0
    rec = []
    for c in calls:
        seen = set()
        for name, v in c.get("query", {}).items():
            mt = v.get("meta_type")
            if mt is None:
                if name in ("b_start", "b_size", "sort_limit"):
                    batch[name] += 1
                continue
            seen.add(mt)
            sh = v["shape"]
            o = sh.get("opts")
            if o is None:
                o = {"query": sh["kind"]}
            key = (mt, tuple(sorted(k if k in ("query", "not") else f"{k}={o[k]}" for k in o)))
            opts[key] += 1
        for mt in seen:
            meta[mt] += 1
        s = c.get("sort")
        types = c.get("sort_types") or []
        if s:
            sort_kind["several indexes" if len(s) > 1 else "one index"] += 1
            reverse = c["args"][1] if len(c.get("args", [])) > 1 else "?"
            limit = c["args"][2] if len(c.get("args", [])) > 2 else "?"
            sorts[(" + ".join(f"{a} ({t})" for a, t in zip(s, types)), reverse, "limit" if limit not in ("None", "?") else "no limit")] += 1
            if "getObjPositionInParent" in s:
                gopip += 1
        else:
            sort_kind["no sort"] += 1
        why = served(c, seen)
        refused[why or "served"] += 1
        ch = c.get("changed") or {}
        if ch.get("records"):
            changed += 1
            rec.append(ch)
            if why is None:
                changed_served += 1
    pct = lambda x: f"{100 * x / n:.1f} %"  # noqa: E731
    print("| Index type | Calls | Share |\n| --- | ---: | ---: |")
    for mt, k in meta.most_common():
        print(f"| {mt} | {k} | {pct(k)} |")
    print("\n| Index type | Options | Calls | Share |\n| --- | --- | ---: | ---: |")
    for (mt, o), k in opts.most_common(40):
        print(f"| {mt} | {', '.join(o)} | {k} | {pct(k)} |")
    print("\n| Sort | Calls | Share |\n| --- | ---: | ---: |")
    for kind, k in sort_kind.most_common():
        print(f"| {kind} | {k} | {pct(k)} |")
    print("\n| sort_on | reverse | limit | Calls | Share |\n| --- | --- | --- | ---: | ---: |")
    for (s, r, lim), k in sorts.most_common(20):
        print(f"| {s} | {r} | {lim} | {k} | {pct(k)} |")
    print("\n| Batch key | Calls | Share |\n| --- | ---: | ---: |")
    for b, k in batch.most_common():
        print(f"| {b} | {k} | {pct(k)} |")
    print("\n| Prototype | Calls | Share |\n| --- | ---: | ---: |")
    for why, k in refused.most_common(15):
        print(f"| {why} | {k} | {pct(k)} |")
    print(f"\nSorted on getObjPositionInParent: {gopip} ({pct(gopip)}).")
    print(f"In a transaction that changed the catalog: {changed} ({pct(changed)}), of which the prototype serves {changed_served}.")
    if rec:
        for key in ("records", "bytes", "new", "new_bytes"):
            vals = sorted(r.get(key, 0) for r in rec)
            print(f"- {key}: p50 {vals[len(vals) // 2]}, p90 {vals[int(len(vals) * 0.9)]}, max {vals[-1]}")
        ps = sorted(r["pickle_seconds"] for r in rec if "pickle_seconds" in r)
        if ps:
            print(f"- pickle ms: p50 {ps[len(ps) // 2] * 1e3:.2f}, p90 {ps[int(len(ps) * 0.9)] * 1e3:.2f}, max {ps[-1] * 1e3:.2f}")


for label, files in groups(sys.argv[1:]):
    report(label, files)
