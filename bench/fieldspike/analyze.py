"""The numbers of the spike on field-addressable objects, from the dumps.

    python3 analyze.py RUN_DIR DUMP_DIR FIELDS

DUMP_DIR holds the files of dump.py and content.jsonl and conflicts.out.jsonl,
which `fieldspike fields` wrote from content.bin and conflicts.bin. FIELDS
are the declared fields (names separated by commas). It prints one JSON
object: for each step, the write volume, what shape (B) would not write and
what shape (A) would add; and each conflict with the fields of both sides.
"""

import collections
import json
import os
import sys

run_dir, dump_dir, fields_arg = sys.argv[1:4]
DECLARED = set(fields_arg.split(","))
# Tier 2 merges this field with `max` in the policy of zoe.plone; a shape
# (B) store needs the same strategy for it.
MAX_FIELDS = {"modification_date"}
result = json.load(open(os.path.join(run_dir, "result.json")))
ok = {s["users"]: s["all"]["outcomes"]["ok"] for s in result["steps_results"]}


def key_bytes(name):
    # BINUNICODE of the key and its BINPUT.
    return len(name.encode()) + 7


def changed(prev, cur):
    names = set(prev) | set(cur)
    return sorted(n for n in names if prev.get(n, [None])[0] != cur.get(n, [None])[0])


steps = {}
for line in open(os.path.join(dump_dir, "txns.jsonl")):
    t = json.loads(line)
    s = steps.setdefault(t["step"], collections.Counter())
    s[f"txns_{t['kind']}"] += 1
    for _, cls, n in t["records"]:
        s[f"bytes_{t['kind']}"] += n
        s[f"records_{t['kind']}"] += 1
        group = (
            "content"
            if cls.startswith(
                ("plone.app.contenttypes.content.", "Products.CMFPlone.Portal.")
            )
            else "btrees"
            if cls.startswith("BTrees.")
            else "other"
        )
        s[f"bytes_{t['kind']}_{group}"] += n

field_changes = collections.Counter()
for line in open(os.path.join(dump_dir, "content.jsonl")):
    g = json.loads(line)
    h = g["h"]
    s = steps[h["step"]]
    prev, cur = g["records"]
    if "fields" not in cur:
        s["content_undecoded"] += 1
        continue
    s["content_records"] += 1
    s["content_bytes"] += cur["bytes"]
    s["a_slot_bytes"] += cur.get("slots", 0)
    if "fields" not in prev:
        s["content_new"] += 1
        s["content_new_bytes"] += cur["bytes"]
        continue
    s["content_updates"] += 1
    s["content_update_bytes"] += cur["bytes"]
    names = changed(prev["fields"], cur["fields"])
    for n in names:
        field_changes[(h["kind"], n)] += 1
    sizes = cur["fields"]
    delta = sum(key_bytes(n) + max(sizes.get(n, [0, 0])[1], 0) for n in names)
    s["changed_field_bytes"] += delta
    # (B) with every field a slot: the update writes its changed fields,
    # each as a slot (7 bytes of header and its value).
    s["b_all_not_written"] += cur["bytes"] - sum(
        7 + max(sizes.get(n, [0, 0])[1], 0) for n in names
    )
    # (B) with the declared fields: an update that changes only declared
    # fields writes them as slots, and no pickle.
    if names and set(names) <= DECLARED:
        s["b_decl_updates"] += 1
        s["b_decl_not_written"] += cur["bytes"] - sum(
            7 + max(sizes.get(n, [0, 0])[1], 0) for n in names
        )
    if not names:
        s["content_same"] += 1

steps_out = {}
for users, s in sorted((k, v) for k, v in steps.items()):
    n = ok.get(users) or 1
    per_op = {k: round(v / n, 2) for k, v in sorted(s.items())}
    per_op["ok_ops"] = ok.get(users)
    steps_out[users] = per_op

conflicts = []
by_oid_fields = {}
conf_fields = os.path.join(dump_dir, "conflicts.out.jsonl")
groups = (
    [json.loads(line) for line in open(conf_fields)]
    if os.path.exists(conf_fields)
    else []
)
gi = 0
for line in open(os.path.join(dump_dir, "conflicts.jsonl")):
    c = json.loads(line)
    entry = {k: c.get(k) for k in ("step", "class", "attempt", "name")}
    entry["catalog"] = (c.get("class") or "").startswith("BTrees.")
    if c.get("group"):
        g = groups[gi]
        gi += 1
        recs = g["records"]
        if len(recs) >= 2 and "fields" in recs[0] and "fields" in recs[1]:
            entry["committed_side"] = changed(recs[0]["fields"], recs[1]["fields"])
        if len(recs) == 4 and "fields" in recs[2] and "fields" in recs[3]:
            entry["retry_side"] = changed(recs[2]["fields"], recs[3]["fields"])
    log = c.get("log")
    if log:
        entry["tier2"] = {
            k: log.get(k)
            for k in ("tier", "mode", "outcome", "code", "reason", "shadow", "fields")
        }
        entry["new_side"] = log.get("keys")
    new = entry.get("new_side") or entry.get("retry_side")
    com = entry.get("committed_side")
    if new is not None and com is not None and not entry["catalog"]:
        overlap = set(new) & set(com)
        entry["overlap"] = sorted(overlap)
        disjoint = not (overlap - MAX_FIELDS)
        entry["disjoint"] = disjoint
        slot_only = lambda f: set(f) <= DECLARED
        # (B): slot writes merge field by field; two pickle writes go to
        # tier 2 as today.
        if slot_only(new) or slot_only(com):
            entry["b_merges"] = disjoint
        else:
            entry["b_merges"] = None
        t2 = entry.get("tier2") or {}
        merged_by_tier2 = (
            t2.get("outcome") == "merged"
            or (t2.get("shadow") or {}).get("outcome") == "merged"
        )
        entry["tier2_merges"] = merged_by_tier2
    conflicts.append(entry)

summary = collections.Counter()
for e in conflicts:
    if e["step"] is None:
        continue
    k = "catalog" if e["catalog"] else "object"
    summary[(e["step"], k)] += 1
    if not e["catalog"]:
        summary[(e["step"], "object:" + (e["class"] or "?"))] += 1
        if e.get("tier2_merges"):
            summary[(e["step"], "tier2 would merge")] += 1
        if e.get("b_merges") and not e.get("tier2_merges"):
            summary[(e["step"], "b beyond tier2")] += 1
        if e.get("disjoint"):
            summary[(e["step"], "disjoint")] += 1

print(
    json.dumps(
        {
            "steps": steps_out,
            "field_changes": {
                f"{k[0]}:{k[1]}": v for k, v in field_changes.most_common()
            },
            "conflict_summary": {
                f"{k[0]}:{k[1]}": v for k, v in sorted(summary.items(), key=str)
            },
            "conflicts": [
                e for e in conflicts if e["step"] is not None and not e["catalog"]
            ],
        },
        indent=1,
    )
)
