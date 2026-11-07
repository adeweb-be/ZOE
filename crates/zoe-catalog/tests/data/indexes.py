"""Write the test data of `tests/indexes.rs`: the records of a catalog with a
KeywordIndex, a DateIndex, a DateRangeIndex, a BooleanIndex and a UUIDIndex
that Products.ZCatalog writes, the records of three commits, and the
answers of ZCatalog to a set of queries on these indexes.

    PYTHONHASHSEED=0 python indexes.py OUT.json

Run it with an interpreter that has Products.ZCatalog 7.2, ZODB 6 and
BTrees 6, such as the interpreter of the zoe scenario of `just plone`.
`PYTHONHASHSEED` fixes the order of the keywords of a KeywordIndex, which
`_unindex` keeps in the order of a set.

The catalog is a `Products.ZCatalog.Catalog.Catalog` at `root["catalog"]`
of a `MappingStorage`. A seeded random gives each entry its rid and its
values. Each query comes with its plan in the form of ADR 0059, section 8,
as `zoe.plone.catalog` builds it, and with the answer of `searchResults`
after the base and after each commit: the rids in order, `len()` and
`actual_result_count`, or the reason of the refusal of the mirror.

The commits: `split` catalogs new entries, so buckets of the `_unindex`
trees split; `change` removes entries, changes the values of others, and
removes two entries from `effectiveRange` alone, so they have no entry in
it; `duplicate` gives a second entry the UUID of another.
"""

import json
import random
import struct
import sys

import Products.ZCatalog.Catalog as catalog_module
import transaction
from Acquisition import aq_base
from DateTime import DateTime
from Products.PluginIndexes.BooleanIndex.BooleanIndex import BooleanIndex
from Products.PluginIndexes.DateIndex.DateIndex import DateIndex
from Products.PluginIndexes.DateRangeIndex.DateRangeIndex import DateRangeIndex
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex
from Products.PluginIndexes.UUIDIndex.UUIDIndex import UUIDIndex
from Products.ZCatalog.Catalog import Catalog
from Products.ZCatalog.query import IndexQuery
from ZODB import DB
from ZODB.MappingStorage import MappingStorage

out = sys.argv[1]
rnd = random.Random(287)
catalog_module.randint = random.Random(288).randint

TYPES = ["Document", "News Item", "Folder", "Event"]
STATES = ["published", "private", "pending"]
WORDS = ["alpha", "beta", "gamma", "delta", "eps", "zeta", "eta", "theta"]
START = DateTime("2026/01/01 00:00:00 UTC")


class Stub:
    def __init__(self, path, **values):
        self.__dict__.update(values)
        self._physical = tuple(path.split("/"))

    def getPhysicalPath(self):
        return self._physical


def day(n):
    return START + n


def values(i):
    state = rnd.choice(STATES)
    v = {
        "portal_type": rnd.choice(TYPES),
        "review_state": state,
        "Subject": tuple(rnd.sample(WORDS, rnd.randint(0, 3))),
        "allowedRolesAndUsers": (
            ["Anonymous"]
            if state == "published"
            else ["Manager", "Owner", f"user:editor-{rnd.randint(0, 3)}"]
        ),
        "modified": day(rnd.randint(-300, 300) + rnd.randint(0, 1439) / 1440.0),
        "effective": day(rnd.randint(-200, 100)) if rnd.random() < 0.8 else None,
        "expires": day(rnd.randint(-50, 300)) if rnd.random() < 0.3 else None,
        "is_folderish": rnd.random() < 0.3,
        "UID": f"{rnd.getrandbits(128):032x}",
        "getId": f"item-{i:04d}",
    }
    if rnd.random() < 0.05:
        del v["modified"]
    if rnd.random() < 0.05:
        del v["is_folderish"]
    return v


storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
cat = Catalog()
root["catalog"] = cat
cat.addIndex("portal_type", FieldIndex("portal_type"))
cat.addIndex("review_state", FieldIndex("review_state"))
cat.addIndex("Subject", KeywordIndex("Subject"))
cat.addIndex("allowedRolesAndUsers", KeywordIndex("allowedRolesAndUsers"))
cat.addIndex("modified", DateIndex("modified"))
cat.addIndex("effective", DateIndex("effective"))
cat.addIndex("effectiveRange", DateRangeIndex("effectiveRange", "effective", "expires"))
cat.addIndex("is_folderish", BooleanIndex("is_folderish"))
cat.addIndex("UID", UUIDIndex("UID"))
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


for i in range(300):
    add(i)
transaction.commit()


def records():
    return {struct.unpack(">Q", oid)[0]: storage.load(oid) for oid in storage._data}


def as_hex(found):
    return {f"{oid:016x}": data.hex() for oid, (data, _) in sorted(found.items())}


base = records()

_CLASSES = {
    "FieldIndex": "v",
    "KeywordIndex": "v",
    "DateIndex": "v",
    "UUIDIndex": "v",
    "BooleanIndex": "b",
    "DateRangeIndex": "d",
}


def plan_of(query):
    """The plan of `query`, as zoe.plone.catalog builds it."""
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
        kind = _CLASSES[type(aq_base(index)).__name__]
        if kind == "b":
            keys = list(rec.keys)
            clauses.append({"k": "b", "i": name, "q": bool(keys[0]) if keys else None})
            continue
        if kind == "d":
            clauses.append(
                {"k": "d", "i": name, "t": index._convertDateTime(rec.keys[0])}
            )
            continue
        nots = list(rec.get("not", None) or [])
        rng = rec.get("range", None)
        clause = {"k": "v", "i": name, "q": [], "and": rec.operator == "and"}
        if rng:
            keys = [index._convert(k) for k in rec.keys]
            lo = min(keys) if "min" in rng else None
            hi = max(keys) if "max" in rng else None
            clause["r"] = [lo, hi if hi else None]
        else:
            clause["q"] = [index._convert(k) for k in rec.keys if k is not None]
            clause["r"] = None
        # ZCatalog converts the keys of `not` for a query of `not` alone.
        # With keys, it looks them up as they are.
        has_keys = bool(clause["q"]) or rng
        clause["not"] = nots if has_keys else [index._convert(k) for k in nots]
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

SORT_KEYS = ("sort_on", "sort_order", "sort_limit", "b_start", "b_size")


def ranged(query):
    """The DateRangeIndexes of `query`."""
    return [
        name
        for name in query
        if name in cat.indexes
        and type(aq_base(cat.getIndex(name))).__name__ == "DateRangeIndex"
    ]


def order_dependent(query):
    """Tell if the answer of ZCatalog to `query` depends on the order of its
    plan: a row that the other clauses give has no entry in a
    DateRangeIndex of the query."""
    names = ranged(query)
    clauses = [k for k in query if k not in SORT_KEYS]
    if not names or len(clauses) < 2:
        return False
    if any(cat.getIndex(n)._convertDateTime(query[n]) is None for n in names):
        return True
    others = {k: v for k, v in query.items() if k not in names and k not in SORT_KEYS}
    rows = set(cat.searchResults(others)) if others else set(cat.paths.keys())
    for name in names:
        index = cat.getIndex(name)
        matched = set(index.query_index(IndexQuery(query, name, ("query",))))
        entry = set(index._unindex.keys())
        rows &= matched | (rows - entry)
    return any(rows - set(cat.getIndex(n)._unindex.keys()) for n in names)


def duplicated(query):
    """Tell if a UUIDIndex of the query holds a UUID at two rows."""
    sort_on = query.get("sort_on", [])
    names = [n for n in query if n in cat.indexes]
    names += sort_on if isinstance(sort_on, list) else [sort_on]
    for name in names:
        index = cat.getIndex(name)
        if type(aq_base(index)).__name__ != "UUIDIndex":
            continue
        if len(set(index._unindex.values())) != len(index._unindex):
            return True
    return False


def answer(query):
    if order_dependent(query):
        return {"f": "date_range_order"}
    if duplicated(query):
        return {"f": "index_unavailable"}
    res = cat.searchResults(dict(query))
    return {
        "r": list(res),
        "n": len(res),
        "a": getattr(res, "actual_result_count", len(res)),
    }


def minutes(n):
    return DateIndex("x")._convert(day(n))


some = sorted(entries.values(), key=lambda v: v["getId"])
EFFECTIVE = next(v["effective"] for v in some if v["effective"] is not None)
EXPIRES = next(v["expires"] for v in some if v["expires"] is not None)
MODIFIED = next(v["modified"] for v in some if "modified" in v)
UIDS = [v["UID"] for v in some[:5]]

QUERIES = {
    "kw-one": {"Subject": "alpha"},
    "kw-or": {"Subject": ["alpha", "beta"]},
    "kw-and": {"Subject": {"query": ["alpha", "beta"], "operator": "and"}},
    "kw-and-absent": {"Subject": {"query": ["alpha", "nothing"], "operator": "and"}},
    "kw-or-absent": {"Subject": {"query": ["nothing", "gamma"]}},
    "kw-not": {"Subject": {"query": ["alpha", "beta", "gamma"], "not": "delta"}},
    "kw-not-only": {"Subject": {"not": ["alpha", "eta"]}, "portal_type": "Document"},
    "kw-not-only-alone": {"Subject": {"not": "zeta"}},
    "kw-not-only-absent": {"Subject": {"not": "nothing"}},
    "kw-range": {"Subject": {"query": ["beta", "eps"], "range": "min:max"}},
    "kw-range-min": {"Subject": {"query": "theta", "range": "min"}},
    "kw-range-not": {
        "Subject": {"query": ["alpha", "eps"], "range": "min:max", "not": "beta"}
    },
    "kw-roles": {"allowedRolesAndUsers": ["Anonymous", "user:editor-2"]},
    "date-eq": {"modified": MODIFIED},
    "date-range": {"modified": {"query": [day(-100), day(100)], "range": "min:max"}},
    "date-range-min": {"modified": {"query": day(200), "range": "min"}},
    "date-range-max": {"modified": {"query": day(-200), "range": "max"}},
    "date-not-only": {"effective": {"not": EFFECTIVE}, "portal_type": "Event"},
    "date-range-not-minutes": {
        "effective": {
            "query": [day(-100), day(0)],
            "range": "min:max",
            "not": [minutes(-50), minutes(-20)],
        }
    },
    "date-sort-desc-limit": {
        "portal_type": "Document",
        "sort_on": "modified",
        "sort_order": "reverse",
        "sort_limit": 10,
    },
    "date-sort-asc-limit": {
        "portal_type": ["Document", "News Item"],
        "sort_on": "modified",
        "sort_limit": 15,
    },
    "date-sort-all": {"review_state": "published", "sort_on": "modified"},
    "date-sort-effective": {
        "Subject": ["alpha", "beta"],
        "sort_on": "effective",
        "sort_order": "descending",
    },
    "date-sort-batch": {
        "portal_type": ["Document", "Folder"],
        "sort_on": "modified",
        "b_start": 40,
        "b_size": 10,
    },
    "date-multisort": {
        "review_state": "private",
        "sort_on": ["portal_type", "modified"],
        "sort_order": ["ascending", "descending"],
        "sort_limit": 30,
    },
    "drange-now": {"effectiveRange": START},
    "drange-now-type": {"effectiveRange": START, "portal_type": "Document"},
    "drange-event": {"effectiveRange": START, "portal_type": "Event"},
    "drange-early": {"effectiveRange": day(-500), "review_state": "published"},
    "drange-late": {"effectiveRange": day(400), "review_state": "published"},
    "drange-at-effective": {"effectiveRange": EFFECTIVE, "portal_type": "News Item"},
    "drange-at-expires": {"effectiveRange": EXPIRES, "portal_type": "News Item"},
    "drange-none-alone": {"effectiveRange": DateTime("2700/01/01 UTC")},
    "drange-none-type": {
        "effectiveRange": DateTime("2700/01/01 UTC"),
        "portal_type": "Folder",
    },
    "bool-true": {"is_folderish": True},
    "bool-false": {"is_folderish": False, "portal_type": "Document"},
    "bool-list": {"is_folderish": [0, 1]},
    "bool-empty": {"is_folderish": {"query": []}},
    "uuid-one": {"UID": UIDS[0]},
    "uuid-list": {"UID": UIDS},
    "uuid-absent": {"UID": "0" * 32},
    "uuid-not": {"UID": {"not": UIDS[1:3]}, "portal_type": "Folder"},
    "uuid-range": {"UID": {"query": ["4", "8"], "range": "min:max"}},
    "uuid-sort": {"portal_type": "Event", "sort_on": "UID", "sort_limit": 8},
    "uuid-sort-all": {"review_state": "pending", "sort_on": "UID"},
    "five-indexes": {
        "portal_type": ["Document", "News Item"],
        "review_state": "published",
        "Subject": ["alpha", "beta", "gamma"],
        "effectiveRange": START,
        "allowedRolesAndUsers": ["Anonymous", "Manager"],
        "sort_on": "modified",
        "sort_order": "reverse",
        "sort_limit": 10,
    },
    "five-indexes-member": {
        "portal_type": ["Document", "News Item", "Event"],
        "review_state": ["private", "pending"],
        "Subject": {"not": "theta"},
        "effectiveRange": START,
        "allowedRolesAndUsers": ["Owner", "user:editor-1"],
        "sort_on": "modified",
        "sort_limit": 10,
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
    for i in range(300, 420):
        add(i)


before, changed = commit("split", split)
# The commit wrote a bucket of an `_unindex` that the base did not hold.
new_buckets = [
    oid
    for oid, (data, _) in changed.items()
    if oid not in before and (b"IOBucket" in data[:40] or b"IIBucket" in data[:40])
]
assert new_buckets, "no bucket split"


def change():
    paths = sorted(entries)
    for path in rnd.sample(paths, 8):
        cat.uncatalogObject(path)
        del entries[path]
    for path in rnd.sample(sorted(entries), 12):
        v = dict(entries[path])
        v["Subject"] = tuple(rnd.sample(WORDS, rnd.randint(0, 3)))
        v["modified"] = day(rnd.randint(-300, 300))
        v["effective"] = day(rnd.randint(-200, 100))
        v["expires"] = day(rnd.randint(-50, 300)) if rnd.random() < 0.5 else None
        v["is_folderish"] = not v.get("is_folderish", False)
        v["review_state"] = rnd.choice(STATES)
        cat.catalogObject(Stub(path, **v), path)
        entries[path] = v
    events = [p for p in sorted(entries) if entries[p]["portal_type"] == "Event"]
    index = cat.getIndex("effectiveRange")
    for path in events[:2]:
        index.unindex_object(cat.uids[path])


commit("change", change)


def duplicate():
    paths = sorted(entries)
    v = dict(entries[paths[1]])
    v["UID"] = entries[paths[0]]["UID"]
    cat.catalogObject(Stub(paths[1], **v), paths[1])
    entries[paths[1]] = v


commit("duplicate", duplicate)

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
