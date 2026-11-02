"""Sample records for the micro-benchmarks of the spike.

Run inside Zope with plonebench/inzope/run.py on a read-only store:
python run.py ZOPE_CONF .../sample.py OUT_DIR

It writes OUT_DIR/docs.bin, the current records of the Documents and News
Items, and OUT_DIR/buckets.bin, the buckets of the trees of the indexes
and of the metadata of the catalog, each group labelled with its tree.
"""

import json
import os
import struct
import sys

out_dir = sys.argv[1]
os.makedirs(out_dir, exist_ok=True)
site = [o for o in app.objectValues() if o.meta_type == "Plone Site"][0]  # noqa: F821
catalog = site.portal_catalog
zcat = catalog._catalog
storage = site._p_jar.db().storage


def group(out, header, records):
    h = json.dumps(header).encode()
    out.write(struct.pack("<I", len(h)) + h + struct.pack("<I", len(records)))
    for r in records:
        out.write(struct.pack("<I", len(r)) + r)


def raw(obj):
    data, _ = storage.load(obj._p_oid)
    return data


with open(os.path.join(out_dir, "docs.bin"), "wb") as out:
    n = 0
    for brain in catalog.unrestrictedSearchResults(
        portal_type=["Document", "News Item"]
    ):
        obj = brain._unrestrictedGetObject()
        group(out, {"type": brain.portal_type}, [raw(obj)])
        n += 1
print("docs", n)


def buckets(tree):
    bucket = getattr(tree, "_firstbucket", None)
    if bucket is None:
        # A tree of one bucket keeps its items inline: no bucket record.
        return []
    found = []
    while bucket is not None:
        found.append(bucket)
        bucket = bucket._next
    return found


with open(os.path.join(out_dir, "buckets.bin"), "wb") as out:
    counts = {}
    trees = [("data", zcat.data)]
    for index_id in zcat.indexes.keys():
        index = zcat.getIndex(index_id)
        for name in ("_unindex", "_index"):
            tree = getattr(index, name, None)
            if tree is not None and hasattr(tree, "_firstbucket"):
                trees.append((f"{index.meta_type}.{index_id}.{name}", tree))
    for label, tree in trees:
        found = buckets(tree)
        counts[label] = len(found)
        found = [b for b in found if b._p_oid is not None]
        counts[label] = len(found)
        for bucket in found:
            group(out, label, [raw(bucket)])
print(json.dumps(counts, indent=1))
