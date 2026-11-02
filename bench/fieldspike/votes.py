"""The votes with a conflict of a B5 run, from its resolution log.

    interpreter votes.py RUN_DIR OUT_DIR

The server writes one line for each stale written oid of a class of a tier
whose mode is not off, all lines of one vote at one time. This script
groups the lines by vote, names the owner of each oid (the first object
that is not of BTrees on a path of references to it), and writes:

- OUT_DIR/votes.jsonl: each vote with its step, its lines and the owners;
- OUT_DIR/votes.bin: for each line of a class that is not of BTrees, the
  group [read record, committed record], for the fields that the committed
  side changed.
"""

import collections
import json
import os
import struct
import sys

import zoe
from ZODB.serialize import referencesf
from ZODB.utils import get_pickle_metadata

run_dir, out_dir = sys.argv[1:3]
os.makedirs(out_dir, exist_ok=True)
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


store = zoe.Storage(os.path.join(run_dir, "store", "zoe"), read_only=True)
classes = {}
referrers = {}
seen = set()
for txn in store.iterator():
    for rec in txn:
        seen.add(rec.oid)
for oid in seen:
    try:
        data, _ = store.load(oid)
    except Exception:
        continue
    if not data:
        continue
    module, name = get_pickle_metadata(data)
    classes[oid] = f"{module}.{name}"
    for ref in referencesf(data):
        referrers.setdefault(ref, set()).add(oid)


def owner(oid):
    current = oid
    visited = {oid}
    if not classes.get(oid, "").startswith("BTrees."):
        return classes.get(oid, "?")
    for _ in range(16):
        parents = [p for p in referrers.get(current, ()) if p not in visited]
        if not parents:
            break
        parent = sorted(
            parents, key=lambda p: classes.get(p, "").startswith("BTrees.")
        )[0]
        visited.add(parent)
        current = parent
        if not classes.get(parent, "").startswith("BTrees."):
            return classes.get(parent, "?")
    return "?"


lines = [json.loads(x) for x in open(os.path.join(run_dir, "resolution.jsonl"))]
votes = collections.OrderedDict()
for line in lines:
    key = (line["time"], line["user"], line["description"], line.get("undo"))
    votes.setdefault(key, []).append(line)


def group(out, header, records):
    h = json.dumps(header).encode()
    out.write(struct.pack("<I", len(h)) + h + struct.pack("<I", len(records)))
    for r in records:
        r = r or b""
        out.write(struct.pack("<I", len(r)) + r)


with (
    open(os.path.join(out_dir, "votes.jsonl"), "w") as vout,
    open(os.path.join(out_dir, "votes.bin"), "wb") as bout,
):
    for (when, user, description, undo), group_lines in votes.items():
        entries = []
        for line in group_lines:
            oid = bytes.fromhex(line["oid"][2:])
            entry = {
                k: line.get(k)
                for k in (
                    "oid",
                    "class",
                    "tier",
                    "mode",
                    "outcome",
                    "code",
                    "reason",
                    "shadow",
                    "fields",
                    "keys",
                    "split",
                    "order",
                    "read",
                    "committed",
                )
            }
            entry["owner"] = owner(oid)
            if not line["class"].startswith("BTrees."):
                records = []
                for tid in (line["read"], line["committed"]):
                    try:
                        records.append(store.loadSerial(oid, bytes.fromhex(tid[2:])))
                    except Exception:
                        records.append(None)
                group(bout, {"oid": line["oid"], "time": when}, records)
                entry["group"] = True
            entries.append(entry)
        vout.write(
            json.dumps(
                {
                    "time": when,
                    "step": step_of(when),
                    "user": user,
                    "description": description,
                    "undo": undo,
                    "lines": entries,
                }
            )
            + "\n"
        )
store.close()
print("votes", len(votes), "lines", len(lines))
