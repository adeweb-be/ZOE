"""Name the owner of each oid of a conflict: the first object that is not
of BTrees on a path of references to it.

    interpreter owners.py RUN_DIR OIDS_FILE OUT.json

OIDS_FILE holds one oid in hex a line. It reads the current records of the
kept store read-only, builds the map of references, and walks up from each
oid through the objects that refer to it, BTrees first.
"""

import json
import os
import sys

import zoe
from ZODB.serialize import referencesf
from ZODB.utils import get_pickle_metadata

run_dir, oids_path, out_path = sys.argv[1:4]
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
    path = [classes.get(oid, "?")]
    current = oid
    visited = {oid}
    for _ in range(12):
        parents = [p for p in referrers.get(current, ()) if p not in visited]
        if not parents:
            break
        parent = sorted(
            parents, key=lambda p: classes.get(p, "").startswith("BTrees.")
        )[0]
        visited.add(parent)
        current = parent
        path.append(classes.get(parent, "?"))
        if not classes.get(parent, "").startswith("BTrees."):
            break
    return path


result = {}
for line in open(oids_path):
    h = line.strip()
    if h:
        result[h] = owner(bytes.fromhex(h.replace("0x", "")))
json.dump(result, open(out_path, "w"), indent=1)
store.close()
