"""Write the test data of `tests/mirror.rs`: the records of a catalog that
Products.ZCatalog writes, the records of three commits, and the answers of
ZCatalog to a set of queries on FieldIndexes.

    PYTHONHASHSEED=0 python zcatalog.py OUT.json

Run it with an interpreter that has Products.ZCatalog 7.2, ZODB 6 and
BTrees 6, such as the interpreter of the zoe scenario of `just plone`.
`PYTHONHASHSEED` fixes the order of the keywords of the KeywordIndex,
which `_unindex` keeps in the order of a set.

The catalog is a `Products.ZCatalog.Catalog.Catalog` at `root["catalog"]`
of a `MappingStorage`, with six FieldIndexes, one KeywordIndex and one
column, and an `OOBTree` at `root["deep"]` whose root has inner nodes as
children. A seeded random gives each entry its rid and its values. Each
query comes with its plan in the form of ADR 0059, section 8, and with the
answer of `searchResults` after the base and after each commit: the rids
in order, `len()` and `actual_result_count`.

The commits: `split` catalogs new entries, so a bucket of each `_unindex`
splits; `remove` removes entries and changes the values of others; `clear`
clears the index `review_state`, which writes a new root of its
`_unindex`.
"""

import json
import random
import struct
import sys

import Products.ZCatalog.Catalog as catalog_module
import transaction
from Acquisition import aq_base
from BTrees.OOBTree import OOBTree
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex
from Products.ZCatalog.Catalog import Catalog
from Products.ZCatalog.query import IndexQuery
from ZODB import DB
from ZODB.MappingStorage import MappingStorage

out = sys.argv[1]
rnd = random.Random(286)
catalog_module.randint = random.Random(287).randint

TYPES = ["Document", "News Item", "Folder", "File", "Event"]
STATES = ["published", "private", "pending"]
WORDS = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta"]
FIELDS = ["portal_type", "review_state", "Creator", "sortable_title", "num", "getId"]


class Stub:
    def __init__(self, path, **values):
        self.__dict__.update(values)
        self._physical = tuple(path.split("/"))

    def getPhysicalPath(self):
        return self._physical


def values(i):
    v = {
        "portal_type": rnd.choice(TYPES),
        "review_state": rnd.choice(STATES),
        "Creator": f"editor-{rnd.randint(0, 7)}",
        "sortable_title": rnd.choice(WORDS) + " " + rnd.choice(WORDS),
        "num": rnd.randint(0, 2999),
        "getId": f"item-{i:04d}",
        "Subject": tuple(sorted(set(rnd.sample(WORDS, rnd.randint(0, 3))))),
    }
    if rnd.random() < 0.05:
        del v["sortable_title"]
    if rnd.random() < 0.1:
        del v["num"]
    return v


storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
cat = Catalog()
root["catalog"] = cat
for name in FIELDS:
    cat.addIndex(name, FieldIndex(name))
cat.addIndex("Subject", KeywordIndex("Subject"))
cat.addColumn("getId")
transaction.commit()

entries = {}


def add(i):
    path = f"/site/folder-{i % 7}/item-{i:04d}"
    # A random start for the next rid, so the rids spread over the range.
    cat._v_nextid = rnd.randint(-2_000_000_000, 2_000_000_000)
    v = values(i)
    cat.catalogObject(Stub(path, **v), path)
    entries[path] = v


for i in range(400):
    add(i)
transaction.commit()

# A tree whose root splits, so its children are inner nodes. The root of an
# OOBTree splits at 500 children of 15 items.
deep = OOBTree()
for k in range(7600):
    deep[k] = k
root["deep"] = deep
transaction.commit()
assert isinstance(deep.__getstate__()[0][0], OOBTree), "no inner child"


def records():
    return {struct.unpack(">Q", oid)[0]: storage.load(oid) for oid in storage._data}


def as_hex(found):
    return {f"{oid:016x}": data.hex() for oid, (data, _) in sorted(found.items())}


base = records()


def plan_of(query):
    """The plan of `query` for the catalog, as zoe.plone.catalog builds it."""
    merged = cat.merge_query_args(query)
    made = cat.make_query(merged)
    clauses = []
    for name in sorted(cat.indexes):
        index = cat.getIndex(name)
        if not any(n in made for n in cat._get_index_query_names(index)):
            continue
        rec = IndexQuery(
            made, index.id, index.query_options, index.operators, index.useOperator
        )
        if rec.keys is None:
            continue
        assert type(aq_base(index)).__name__ == "FieldIndex", name
        nots = [index._convert(k) for k in (rec.get("not", None) or [])]
        rng = rec.get("range", None)
        clause = {"k": "v", "i": name, "q": [], "and": rec.operator == "and"}
        if rng:
            lo = min(rec.keys) if "min" in rng else None
            hi = max(rec.keys) if "max" in rng else None
            clause["r"] = [lo, hi if hi else None]
        else:
            clause["q"] = [index._convert(k) for k in rec.keys if k is not None]
            clause["r"] = None
        clause["not"] = nots
        clauses.append(clause)
    sort = []
    sort_indexes = cat._getSortIndex(merged)
    if sort_indexes is not None:
        if not isinstance(sort_indexes, list):
            sort_indexes = [sort_indexes]
        order = cat._get_sort_attr("order", merged)
        if order is None:
            order = [""]
        elif isinstance(order, str):
            order = [order]
        reverse = [o.lower() in ("reverse", "descending") for o in order]
        for i, s in enumerate(sort_indexes):
            if len(reverse) == 1:
                r = reverse[0]
            else:
                r = reverse[i] if i < len(reverse) else None
            sort.append([s.getId(), r])
    b_size = made.get("b_size", None)
    return {
        "c": clauses,
        "s": sort,
        "l": cat._get_sort_attr("limit", merged),
        "bs": int(made.get("b_start", 0)),
        "bz": int(b_size) if b_size is not None else None,
    }


# The catalog has no acquisition parent, so it cannot make brains: each
# item of a result is its rid.
Catalog.instantiate = lambda self, record, score_data=None: record[0]


def answer(query):
    res = cat.searchResults(dict(query))
    return {
        "r": list(res),
        "n": len(res),
        "a": getattr(res, "actual_result_count", len(res)),
    }


ALL = {"portal_type": TYPES}
QUERIES = {
    "eq-one": {"portal_type": "Document"},
    "eq-list": {"portal_type": ["Document", "News Item"]},
    "eq-absent": {"portal_type": "Nothing"},
    "eq-two-clauses": {"portal_type": "Folder", "review_state": "private"},
    "eq-none-key": {"portal_type": {"query": None}},
    "range-min-max": {"num": {"query": [100, 900], "range": "min:max"}},
    "range-min": {"num": {"query": 2500, "range": "min"}},
    "range-max": {"num": {"query": 300, "range": "max"}},
    "range-max-zero": {"num": {"query": 0, "range": "max"}},
    "range-str": {"sortable_title": {"query": ["beta", "delta"], "range": "min:max"}},
    "not-with-keys": {
        "portal_type": {"query": ["Document", "News Item"], "not": "News Item"},
        "Creator": "editor-2",
    },
    "not-only": {"review_state": {"not": "published"}},
    "not-only-list": {
        "Creator": {"not": ["editor-1", "editor-2"]},
        "portal_type": "File",
    },
    "not-only-absent": {"num": {"not": [-5]}},
    "iterate-index": {"portal_type": "Document", "sort_on": "review_state"},
    "iterate-index-desc": {
        "portal_type": "Document",
        "sort_on": "review_state",
        "sort_order": "reverse",
    },
    "resultset": {"portal_type": "Document", "sort_on": "sortable_title"},
    "resultset-desc": {
        "portal_type": "Document",
        "sort_on": "sortable_title",
        "sort_order": "descending",
    },
    "resultset-limit": {
        "portal_type": "Document",
        "sort_on": "Creator",
        "sort_limit": 30,
    },
    "nbest": {
        **ALL,
        "sort_on": "review_state",
        "sort_order": "descending",
        "sort_limit": 10,
    },
    "nbest-reverse": {**ALL, "sort_on": "review_state", "sort_limit": 10},
    "nbest-missing": {
        **ALL,
        "sort_on": "sortable_title",
        "sort_order": "descending",
        "sort_limit": 25,
    },
    "nbest-num": {**ALL, "sort_on": "num", "sort_limit": 12},
    "multisort-mixed": {
        "review_state": "pending",
        "sort_on": ["portal_type", "sortable_title"],
        "sort_order": ["ascending", "descending"],
        "sort_limit": 40,
    },
    "multisort-same": {
        "portal_type": "News Item",
        "sort_on": ["review_state", "Creator"],
        "sort_order": "descending",
        "sort_limit": 10,
    },
    "batch": {
        "portal_type": ["Document", "News Item"],
        "sort_on": "sortable_title",
        "b_start": 10,
        "b_size": 20,
    },
    "batch-past-middle": {
        "portal_type": ["Document", "News Item"],
        "sort_on": "sortable_title",
        "b_start": 150,
        "b_size": 20,
    },
    "batch-past-end": {
        "portal_type": "Document",
        "sort_on": "getId",
        "b_start": 5000,
        "b_size": 20,
    },
    "batch-unsorted": {"review_state": "private", "b_start": 5, "b_size": 10},
    "limit-over": {"Creator": "editor-4", "sort_on": "num", "sort_limit": 100000},
    "generic-numbers": {
        "num": {"query": [50, 2950], "range": "min:max"},
        "portal_type": "Document",
        "review_state": "published",
        "Creator": ["editor-1", "editor-2", "editor-3"],
        "sortable_title": {"not": "alpha alpha"},
        "sort_on": "getId",
        "sort_limit": 20,
    },
}

plans = {name: plan_of(query) for name, query in QUERIES.items()}
answers = {name: [answer(query)] for name, query in QUERIES.items()}

commits = []


def commit(name, change):
    before = records()
    change()
    transaction.commit()
    after = records()
    changed = {oid: found for oid, found in after.items() if before.get(oid) != found}
    commits.append({"name": name, "records": as_hex(changed)})
    for q, query in QUERIES.items():
        answers[q].append(answer(query))
    return before, changed


def split():
    for i in range(400, 480):
        add(i)


before, changed = commit("split", split)
# The commit wrote a bucket of an `_unindex` that the base did not hold.
new_buckets = [
    oid
    for oid, (data, _) in changed.items()
    if oid not in before and b"IOBucket" in data[:40]
]
assert new_buckets, "no bucket split"


def remove():
    paths = sorted(entries)
    for path in rnd.sample(paths, 6):
        cat.uncatalogObject(path)
        del entries[path]
    for path in rnd.sample(sorted(entries), 6):
        v = dict(entries[path])
        v["portal_type"] = rnd.choice(TYPES)
        v["num"] = rnd.randint(0, 2999)
        v.pop("sortable_title", None)
        cat.catalogObject(Stub(path, **v), path)
        entries[path] = v


commit("remove", remove)
commit("clear", lambda: cat.getIndex("review_state").clear())

with open(out, "w") as f:
    json.dump(
        {
            "catalog": f"{struct.unpack('>Q', cat._p_oid)[0]:016x}",
            "base": as_hex(base),
            "commits": commits,
            "plans": plans,
            "answers": answers,
        },
        f,
        indent=0,
        sort_keys=True,
    )
db.close()
