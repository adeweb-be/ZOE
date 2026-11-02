"""The records that ZCTextIndex writes for one save, on catalog-1m.

Usage: python zcsave.py VENDOR DATA_FS CORPUS SAVES

Opens the packed Data.fs of the fixture read-only under a DemoStorage,
finds the three ZCTextIndex indexes of portal_catalog, and reindexes
SAVES documents, one transaction each, with the texts of other entries
of the plan, as `ZCTextIndex.index_object` does. It reports the records
and the bytes of each save, and, for pairs of saves, the records that both
write: the surface of a conflict between two saves of different documents.
The Plone classes are absent: the site and the tool load as broken
objects, and the script reads their state.
"""

import collections
import itertools
import json
import random
import statistics
import sys

sys.argv, args = sys.argv[:2], sys.argv[2:]
import zctext  # noqa: E402,F401  (installs the classes of plone_lexicon)

import transaction  # noqa: E402
from ZODB import DB  # noqa: E402
from ZODB.DemoStorage import DemoStorage  # noqa: E402
from ZODB.FileStorage import FileStorage  # noqa: E402
from ZODB.utils import get_pickle_metadata, u64  # noqa: E402


def state(obj):
    obj._p_activate()
    st = getattr(obj, "__Broken_state__", None)
    return st if st is not None else obj.__dict__


def child(container, name):
    st = state(container)
    if name in st:
        return st[name]
    tree = st.get("_tree")
    if tree is not None and name in tree:
        return tree[name]
    objects = st.get("_objects")
    raise KeyError(f"{name} not in {sorted(k for k in st if not k.startswith('_'))[:30]} {objects}")


def main():
    data_fs, corpus, saves = args[0], args[1], int(args[2])
    base = FileStorage(data_fs, read_only=True)
    demo = DemoStorage(base=base)
    db = DB(demo, cache_size=200_000)
    conn = db.open()
    app = conn.root()["Application"]
    site = child(app, "Plone")
    tool = child(site, "portal_catalog")
    catalog = state(tool)["_catalog"]
    indexes = {name: catalog.indexes[name] for name in ("SearchableText", "Title", "Description")}
    okapi = {name: idx.index for name, idx in indexes.items()}
    rng = random.Random(284)
    rids = list(itertools.islice(okapi["SearchableText"]._docweight.keys(), 0, None, 997))
    rng.shuffle(rids)
    lines = [json.loads(line) for line in itertools.islice(open(corpus), 5000, 5000 + 4 * saves)]
    with_text = [d for d in lines if len(d["SearchableText"]) > 1000]
    before = demo.lastTransaction()
    per_save = []
    for n in range(saves):
        src = with_text[n % len(with_text)]
        rid = rids[n]
        for name in ("SearchableText", "Title", "Description"):
            okapi[name].index_doc(rid, [src[name]])
        transaction.get().note(f"save {n}")
        transaction.commit()
    # The records of each save, from the changes of the DemoStorage.
    classes = collections.Counter()
    for txn in demo.changes.iterator():
        if txn.tid <= before:
            continue
        oids, size = set(), 0
        for rec in txn:
            oids.add(u64(rec.oid))
            size += len(rec.data)
            classes[".".join(get_pickle_metadata(rec.data))] += 1
        per_save.append((oids, size))
    records = [len(o) for o, _ in per_save]
    sizes = [s for _, s in per_save]
    pairs = [(a, b) for a, b in itertools.combinations(range(len(per_save)), 2)]
    shared = [len(per_save[a][0] & per_save[b][0]) for a, b in pairs]
    print(
        json.dumps(
            {
                "saves": len(per_save),
                "records_mean": statistics.mean(records),
                "records_p50": statistics.median(records),
                "pickle_bytes_mean": statistics.mean(sizes),
                "pickle_bytes_p50": statistics.median(sizes),
                # A ZOE store adds a 32-byte frame header, the 16-byte key and
                # the 16-byte key of txn_oids to each record.
                "zoe_bytes_mean": statistics.mean(s + 64 * r for s, r in zip(sizes, records)),
                "pairs": len(pairs),
                "pairs_sharing_a_record": sum(1 for s in shared if s) / len(pairs),
                "shared_records_mean": statistics.mean(shared),
                "document_count": okapi["SearchableText"].document_count(),
                "classes": dict(classes.most_common(12)),
            }
        )
    )
    transaction.abort()
    conn.close()
    db.close()


if __name__ == "__main__":
    main()
