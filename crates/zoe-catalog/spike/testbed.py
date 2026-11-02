"""A small catalog in a MappingStorage, its records, and ZCatalog's
answers to random queries: a testbed of the mirror prototype that needs no
Zope and no large store.

    interpreter testbed.py OUT_DIR [SEED] [N]

OUT_DIR gets records.bin (the records at the first commit), plans.jsonl,
overlays.jsonl, commit.bin (the records of a second commit) and
plans-after.jsonl (the answers after it).
"""

import json
import os
import random
import struct
import sys

import transaction
from DateTime import DateTime
from OFS.Folder import Folder
from Products.ExtendedPathIndex.ExtendedPathIndex import ExtendedPathIndex
from Products.PluginIndexes.BooleanIndex.BooleanIndex import BooleanIndex
from Products.PluginIndexes.DateIndex.DateIndex import DateIndex
from Products.PluginIndexes.DateRangeIndex.DateRangeIndex import DateRangeIndex
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex
from Products.PluginIndexes.UUIDIndex.UUIDIndex import UUIDIndex
from Products.ZCatalog.ZCatalog import ZCatalog
from ZODB import DB
from ZODB.MappingStorage import MappingStorage

HERE = os.path.dirname(os.path.abspath(__file__))
sys.argv, real_argv = [sys.argv[0], "x", "x", "x", "x"], sys.argv
# The plan builder and the overlay writer of check.py, without its run.
source = open(os.path.join(HERE, "check.py")).read()
head = source.split("st = zoe.Storage(")[0]
tail = source.split("# The overlay of a transaction that changed the catalog.")[1].split("\nopen(overlays_out, \"w\").close()")[0]
namespace = {"__name__": "check"}
exec(compile(head.replace("import zoe\n", ""), "check.py", "exec"), namespace)
exec(compile(tail, "check.py", "exec"), namespace)
sys.argv = real_argv
plan_of = namespace["plan_of"]
answer = namespace["answer"]
Stub = namespace["Stub"]
overlay_records = namespace["overlay_records"]

out_dir = sys.argv[1]
seed = int(sys.argv[2]) if len(sys.argv) > 2 else 1
n = int(sys.argv[3]) if len(sys.argv) > 3 else 3000
rnd = random.Random(seed)
import Products.ZCatalog.Catalog as catalog_module  # noqa: E402

catalog_module.randint = random.Random(seed + 1).randint

storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
app = Folder("app")
root["app"] = app
app._setObject("Plone", Folder("Plone"))
site = app.Plone
site._setObject("portal_catalog", ZCatalog("portal_catalog"))
zcat = site.portal_catalog
inner = zcat._catalog
for index in (
    FieldIndex("portal_type"),
    FieldIndex("review_state"),
    FieldIndex("Creator"),
    FieldIndex("sortable_title"),
    FieldIndex("bench_int"),
    DateIndex("modified"),
    DateIndex("effective"),
    KeywordIndex("Subject"),
    KeywordIndex("allowedRolesAndUsers"),
    BooleanIndex("is_folderish"),
    BooleanIndex("exclude_from_nav"),
    UUIDIndex("UID"),
    ExtendedPathIndex("path"),
    DateRangeIndex("effectiveRange", since_field="effective", until_field="expires"),
):
    inner.addIndex(index.getId(), index)
for column in ("Title", "portal_type", "review_state", "modified", "effective", "Subject", "is_folderish", "getId"):
    inner.addColumn(column)
transaction.commit()

TYPES = ["Document", "News Item", "Folder", "File", "Event"]
STATES = ["published", "private", "pending"]
WORDS = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta"]
paths = ["/Plone"]
items = []


def values(path, i):
    folder = rnd.random() < 0.15
    v = {
        "portal_type": "Folder" if folder else rnd.choice(TYPES[:2] + TYPES[3:]),
        "review_state": rnd.choice(STATES),
        "Creator": f"editor-{rnd.randint(0, 7)}",
        "sortable_title": rnd.choice(WORDS) + " " + rnd.choice(WORDS),
        "bench_int": rnd.randint(-5, 40),
        "modified": DateTime(1735689600 + rnd.randint(0, 400) * 3600, "UTC"),
        "effective": DateTime(1735689600 + rnd.randint(-200, 300) * 3600, "UTC"),
        "expires": DateTime(1735689600 + rnd.randint(250, 900) * 3600, "UTC") if rnd.random() < 0.5 else None,
        "Subject": tuple(sorted(set(rnd.sample(WORDS, rnd.randint(0, 3))))),
        "allowedRolesAndUsers": ["Anonymous"] if rnd.random() < 0.6 else [f"user:editor-{rnd.randint(0, 7)}"],
        "is_folderish": folder,
        "exclude_from_nav": rnd.random() < 0.1,
        "UID": f"{rnd.getrandbits(128):032x}",
        "Title": f"Item {i}",
        "getId": path.rsplit("/", 1)[1],
    }
    if rnd.random() < 0.05:
        del v["sortable_title"]
    if rnd.random() < 0.05:
        del v["effective"]
        del v["expires"]
    return v, folder


folders = ["/Plone"]
for i in range(n):
    parent = rnd.choice(folders)
    path = f"{parent}/item-{i}"
    v, folder = values(path, i)
    if folder and path.count("/") < 7:
        folders.append(path)
    inner.catalogObject(Stub(path, **v), path)
    items.append((path, v))
    if i % 500 == 0:
        transaction.commit()
transaction.commit()


def records_file(path, oids_tids):
    with open(path, "wb") as out:
        for oid, data in oids_tids:
            out.write(struct.pack(">QI", oid, len(data)))
            out.write(data)


def all_records():
    out = []
    for oid in list(storage._data):
        data, tid = storage.load(oid)
        out.append((struct.unpack(">Q", oid)[0], data))
    return out


records_file(os.path.join(out_dir, "records.bin"), all_records())
cat_oid = struct.unpack(">Q", inner._p_oid)[0]
print("catalog", hex(cat_oid), len(inner), "records", len(storage._data), flush=True)


def some_path(depth_min=2):
    return rnd.choice([p for p in folders if p.count("/") >= depth_min] or folders)


def random_query():
    q = {}
    kinds = rnd.sample(
        ["type", "state", "creator", "subject", "roles", "bool", "bool2", "uid", "path", "range_int",
         "range_date", "daterange", "not", "multi"], rnd.randint(1, 4))
    for k in kinds:
        if k == "type":
            q["portal_type"] = rnd.choice([rnd.choice(TYPES), rnd.sample(TYPES, 2), {"query": rnd.sample(TYPES, 3), "not": rnd.choice(TYPES)}])
        elif k == "state":
            q["review_state"] = rnd.choice(STATES + [STATES[:2], {"not": "published"}, {"not": ["published", "pending"]}])
        elif k == "creator":
            q["Creator"] = rnd.choice([f"editor-{rnd.randint(0, 8)}", [f"editor-{rnd.randint(0, 8)}" for _ in range(3)]])
        elif k == "subject":
            q["Subject"] = rnd.choice([rnd.choice(WORDS), {"query": rnd.sample(WORDS, 2), "operator": "and"}, rnd.sample(WORDS, 3), {"query": "nothere"}])
        elif k == "roles":
            q["allowedRolesAndUsers"] = ["Anonymous", f"user:editor-{rnd.randint(0, 7)}"]
        elif k == "bool":
            q["is_folderish"] = rnd.choice([True, False])
        elif k == "bool2":
            q["exclude_from_nav"] = rnd.choice([True, False, 0, 1])
        elif k == "uid":
            q["UID"] = rnd.choice([rnd.choice(items)[1]["UID"], [rnd.choice(items)[1]["UID"] for _ in range(3)]])
        elif k == "path":
            p = some_path()
            q["path"] = rnd.choice([
                p,
                {"query": p, "depth": 1},
                {"query": p, "depth": 0},
                {"query": p, "depth": 2},
                {"query": [p, some_path()], "depth": 1},
                {"query": rnd.choice(items)[0], "navtree": 1, "navtree_start": rnd.randint(0, 2)},
                {"query": rnd.choice(items)[0], "navtree": 1, "depth": 0},
                {"query": p, "navtree": 1, "depth": 999},
                {"query": p.rsplit("/", 1)[1], "level": p.count("/") - 1},
                {"query": p.rsplit("/", 1)[1], "level": -1},
                {"query": p, "depth": 3, "navtree": 1, "navtree_start": 1},
                "/Plone",
                {"query": "/Plone", "depth": 1},
            ])
        elif k == "range_int":
            lo, hi = sorted(rnd.sample(range(-6, 42), 2))
            q["bench_int"] = rnd.choice([{"query": [lo, hi], "range": "min:max"}, {"query": lo, "range": "min"}, {"query": hi, "range": "max"}, {"query": [0, hi], "range": "min:max"}, {"query": [lo, 0], "range": "min:max"}])
        elif k == "range_date":
            d = DateTime(1735689600 + rnd.randint(0, 400) * 3600, "UTC")
            q["modified"] = rnd.choice([{"query": d, "range": "min"}, {"query": d, "range": "max"}])
        elif k == "daterange":
            q["effectiveRange"] = DateTime(1735689600 + rnd.randint(-100, 700) * 3600, "UTC")
        elif k == "not":
            q["Creator"] = {"not": [f"editor-{rnd.randint(0, 7)}"]}
        elif k == "multi":
            q["portal_type"] = TYPES
    sort = rnd.choice([None, "sortable_title", "modified", "effective", "review_state", "Creator", "bench_int", "portal_type", ["portal_type", "sortable_title"], ["review_state", "modified"], ["Creator", "bench_int", "UID"]])
    if sort:
        q["sort_on"] = sort
        if isinstance(sort, list):
            q["sort_order"] = rnd.choice(["ascending", "descending", ["ascending", "descending"], ["descending", "ascending", "descending"]])
        else:
            q["sort_order"] = rnd.choice(["ascending", "descending", "reverse"])
        if rnd.random() < 0.6:
            q["sort_limit"] = rnd.choice([1, 3, 10, 30, 100, 5000])
    if rnd.random() < 0.3:
        q["b_start"] = rnd.choice([0, 5, 20, 400, 1400, 2500])
        q["b_size"] = rnd.choice([1, 10, 50])
    return q


def plans_file(path, queries):
    with open(path, "w") as out:
        for name, q in queries:
            try:
                plan, reason = plan_of(inner, q)
                expected = answer(inner, q)
            except Exception as error:  # ZCatalog itself raises on some shapes
                plan, reason, expected = None, f"zcatalog raises {error!r}", None
            out.write(json.dumps({"name": name, "query": repr(q), "plan": plan, "reason": reason, "expected": expected}) + "\n")


queries = [(f"q{i}", random_query()) for i in range(600)]
plans_file(os.path.join(out_dir, "plans.jsonl"), queries)

# Overlays: changes in a transaction that does not commit.
overlays_out = os.path.join(out_dir, "overlays.jsonl")
open(overlays_out, "w").close()


def change(k):
    def run():
        for _ in range(k):
            path, v = rnd.choice(items)
            v = dict(v)
            v["review_state"] = rnd.choice(STATES)
            v["Subject"] = tuple(sorted(set(rnd.sample(WORDS, 2))))
            v["modified"] = DateTime(1735689600 + rnd.randint(0, 500) * 3600, "UTC")
            v["sortable_title"] = rnd.choice(WORDS) + " zz"
            inner.catalogObject(Stub(path, **v), path)
        for j in range(k):
            parent = some_path(1)
            path = f"{parent}/new-{k}-{j}-{rnd.randint(0, 10**6)}"
            v, _ = values(path, 10**6 + j)
            inner.catalogObject(Stub(path, **v), path)
        for _ in range(max(1, k // 2)):
            path, _ = rnd.choice(items)
            if inner.uids.get(path) is not None:
                inner.uncatalogObject(path)
    return run


def overlay_check(name, run):
    transaction.abort()
    conn.cacheMinimize()
    run()
    records, seconds = overlay_records(conn)
    checks = []
    for i in range(40):
        q = random_query()
        try:
            plan, reason = plan_of(inner, q)
            expected = answer(inner, q)
        except Exception as error:
            plan, reason, expected = None, repr(error), None
        checks.append({"name": f"{name}-q{i}", "query": repr(q), "plan": plan, "reason": reason, "expected": expected})
    with open(overlays_out, "a") as out:
        out.write(json.dumps({"name": name, "records": [[f"{o:x}", d.hex()] for o, d in records], "plans": checks}) + "\n")
    transaction.abort()


for k in (1, 3, 20, 200):
    overlay_check(f"overlay-{k}", change(k))

# A commit, its records, and the answers after it.
transaction.abort()
before = {oid: storage.load(oid)[1] for oid in storage._data}
change(30)()
transaction.commit()
changed = []
for oid in storage._data:
    data, tid = storage.load(oid)
    if before.get(oid) != tid:
        changed.append((struct.unpack(">Q", oid)[0], data))
records_file(os.path.join(out_dir, "commit.bin"), changed)
queries = [(f"after{i}", random_query()) for i in range(300)]
plans_file(os.path.join(out_dir, "plans-after.jsonl"), queries)
print("commit records", len(changed), flush=True)
db.close()
