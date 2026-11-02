"""The conflicts of a B5 run by vote, for the spike.

    python3 analyze_votes.py RUN_DIR DUMP_DIR FIELDS

It reads votes.jsonl and votes.out.jsonl (`fieldspike fields` of votes.bin)
of DUMP_DIR and the probe records of RUN_DIR, and prints the conflicts of
each step: the ConflictErrors of the probe by the owner of their oid, and
for each line of a content class, the fields of both sides, what tier 2
gave, and what shape (B) would give.
"""

import collections
import glob
import json
import os
import sys

run_dir, dump_dir, fields_arg = sys.argv[1:4]
DECLARED = set(fields_arg.split(","))
MAX_FIELDS = {"modification_date"}

votes = [json.loads(x) for x in open(os.path.join(dump_dir, "votes.jsonl"))]
groups = [json.loads(x) for x in open(os.path.join(dump_dir, "votes.out.jsonl"))]
owner_of = {}
gi = 0
content_lines = []
for vote in votes:
    for line in vote["lines"]:
        owner_of[line["oid"][2:]] = line["owner"]
        if line.get("group"):
            g = groups[gi]
            gi += 1
            a, b = g["records"]
            if "fields" in a and "fields" in b:
                names = set(a["fields"]) | set(b["fields"])
                line["committed_side"] = sorted(
                    n
                    for n in names
                    if a["fields"].get(n, [None])[0] != b["fields"].get(n, [None])[0]
                )
            content_lines.append((vote, line))

result = json.load(open(os.path.join(run_dir, "result.json")))
start = result["measured"]["start"]
trim = result.get("trim_seconds", 5.0)
windows = []
t = start
for users, seconds in result["steps"]:
    windows.append((users, t + trim, t + seconds))
    t += seconds


def step_of(when):
    for users, lo, hi in windows:
        if lo <= when < hi:
            return users
    return None


def kind(owner):
    if owner.startswith(
        (
            "Products.ZCatalog",
            "Products.PluginIndexes",
            "Products.CMFPlone.CatalogTool",
            "Products.ExtendedPathIndex",
            "Products.DateRecurringIndex",
            "plone.app.event",
            "Products.ZCTextIndex",
            "Products.CMFCore.indexing",
        )
    ):
        return "catalog"
    if owner.startswith(("Products.ZopeVersionControl", "Products.CMFEditions")):
        return "versions (CMFEditions)"
    if owner.startswith(
        ("Products.PlonePAS", "Products.PluggableAuthService", "zoe.plone.properties")
    ):
        return "members (PAS)"
    if owner.startswith("plone.app.contenttypes.content."):
        return "content"
    return owner


errors = collections.Counter()
for path in sorted(glob.glob(os.path.join(run_dir, "probe", "instance*.jsonl"))):
    for raw in open(path):
        request = json.loads(raw)
        for error in request.get("errors", []):
            if error.get("error") not in ("ConflictError", "ReadConflictError"):
                continue
            step = step_of(request.get("t") or 0)
            if step is None:
                continue
            oid = error.get("oid") or ""
            cls = error.get("class") or "?"
            own = owner_of.get(oid, cls if not cls.startswith("BTrees.") else "?")
            errors[(step, kind(own), cls)] += 1

per_step = collections.defaultdict(collections.Counter)
details = []
for vote, line in content_lines:
    step = vote["step"]
    if step is None:
        continue
    s = per_step[step]
    s["content lines"] += 1
    s[f"content lines: {line['class']}"] += 1
    others = [x for x in vote["lines"] if x is not line]
    other_fail = any(x["outcome"] != "merged" for x in others if x["mode"] != "shadow")
    t2 = line["outcome"] if line["mode"] != "shadow" else line["outcome"]
    s[f"tier 2: {t2}"] += 1
    new = set(line.get("keys") or [])
    com = set(line.get("committed_side") or [])
    overlap = (new & com) - MAX_FIELDS
    disjoint = not overlap
    s["disjoint" if disjoint else "overlap"] += 1
    slot_new, slot_com = new <= DECLARED, com <= DECLARED
    if slot_new or slot_com:
        b = "merges" if disjoint else "refuses"
    else:
        b = "two pickles: tier 2"
    s[f"(B): {b}"] += 1
    beyond = (b == "merges") and t2 != "merged"
    if beyond:
        s["(B) beyond tier 2"] += 1
    if other_fail:
        s["the vote also fails on another oid"] += 1
    details.append(
        {
            "step": step,
            "class": line["class"],
            "tier2": t2,
            "tier2_fields": line.get("fields"),
            "new_side": sorted(new),
            "committed_side": sorted(com),
            "overlap": sorted(overlap),
            "b": b,
            "other_fail": other_fail,
            "others": sorted(
                {
                    kind(x["owner"]) + ":" + x["class"] + ":" + x["outcome"]
                    for x in others
                }
            ),
        }
    )

pairs = collections.Counter(
    (tuple(d["new_side"]), tuple(d["committed_side"])) for d in details
)
print(
    json.dumps(
        {
            "conflict_errors": {
                f"{k[0]}|{k[1]}|{k[2]}": v for k, v in sorted(errors.items(), key=str)
            },
            "content_lines": {str(k): dict(v) for k, v in sorted(per_step.items())},
            "field_pairs": [
                [list(k[0]), list(k[1]), v] for k, v in pairs.most_common(12)
            ],
            "refused": [d for d in details if d["tier2"] != "merged"],
            "others": collections.Counter(
                o for d in details for o in d["others"]
            ).most_common(10),
        },
        indent=1,
    )
)
