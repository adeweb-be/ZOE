"""Write the test data of `tests/path.rs`: the records of a catalog with an
ExtendedPathIndex that Products.ZCatalog writes, the records of seven
commits, and the answers of ZCatalog to a set of path queries.

    PYTHONHASHSEED=0 python path.py OUT.json
    PYTHONHASHSEED=0 python path.py OUT.json multi-valued

Run it with an interpreter that has Products.ZCatalog 7.2,
Products.ExtendedPathIndex 5, ZODB 6 and BTrees 6, such as the
interpreter of the zoe scenario of `just plone`.

The catalog is a `Products.ZCatalog.Catalog.Catalog` at `root["catalog"]`
of a `MappingStorage`, with the ExtendedPathIndex `path`, the FieldIndexes
`portal_type` and `sortable_title`, and the column `getId`. The physical
path of the index is `/site/portal_catalog/path`. With `multi-valued`, the
index has `multi_valued` set: a search at the level 0 with a depth of 0
reads `_index_parents`, and a navtree of depth 1 reads `_index_items`.
`path.json` is the data without it, `path-multi.json` the data with it. A seeded random gives
each entry its rid. The entries are a tree of folders under `/site`, with
no entry for `/site` itself, so no row ends at the level 0; an entry
outside the site, `/other/item`; an entry of one component, `/lone`; and
an entry whose `path` attribute holds empty components.

Each query comes with its plan in the form of ADR 0059, section 8, as
`zoe.plone.catalog` builds it, and with the answer of `searchResults`
after the base and after each commit: the rids in order, `len()` and
`actual_result_count`, or the reason of the refusal of the mirror.

The commits: `split` catalogs new entries, so buckets of `_unindex` split;
`move` moves a folder of three levels to another folder, as CMFCore does:
it uncatalogs each old path and catalogs each new path, with new rids;
`rename` renames a folder of two levels the same way; `repath` gives three
entries a new path under their old uid, so they keep their rids;
`delete` uncatalogs a folder and its subtree; `twin` catalogs `/site` and
a second entry whose `path` attribute is the path of another entry; and
`untwin` uncatalogs the twin and `/lone`. After `untwin`, `_index_items`
holds no rid for the path of the twin, and still holds the rid of `/lone`:
the key that `unindex_object` deletes for a path of one component is
`//lone`.
"""

import json
import random
import struct
import sys

import Products.ZCatalog.Catalog as catalog_module
import transaction
from Acquisition import aq_base
from Products.ExtendedPathIndex.ExtendedPathIndex import ExtendedPathIndex
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.ZCatalog.Catalog import Catalog
from Products.ZCatalog.query import IndexQuery
from ZODB import DB
from ZODB.MappingStorage import MappingStorage

out = sys.argv[1]
multi = sys.argv[2:] == ["multi-valued"]
rnd = random.Random(288)
catalog_module.randint = random.Random(289).randint

# The catalog has no acquisition parent: the index takes the physical path
# of the index of a site.
ExtendedPathIndex.getPhysicalPath = lambda self: ("", "site", "portal_catalog", "path")

TYPES = ["Document", "News Item", "Folder"]


class Stub:
    def __init__(self, physical, **values):
        self.__dict__.update(values)
        self._physical = tuple(physical.split("/"))

    def getPhysicalPath(self):
        return self._physical


storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
cat = Catalog()
root["catalog"] = cat
cat.addIndex("path", ExtendedPathIndex("path", extra={"multi_valued": multi}))
cat.addIndex("portal_type", FieldIndex("portal_type"))
cat.addIndex("sortable_title", FieldIndex("sortable_title"))
cat.addColumn("getId")
transaction.commit()

# The uid of each entry, with the physical path and the values of its
# object.
entries = {}


def add(uid, physical=None, **extra):
    physical = physical or uid
    cat._v_nextid = rnd.randint(-2_000_000_000, 2_000_000_000)
    name = physical.rstrip("/").split("/")[-1]
    values = {
        "portal_type": "Folder"
        if name.startswith(("folder", "sub", "deep"))
        else rnd.choice(TYPES[:2]),
        "sortable_title": f"{name} {rnd.randint(0, 9)}",
        "getId": name,
    }
    values.update(extra)
    cat.catalogObject(Stub(physical, **values), uid)
    entries[uid] = (physical, values)


def drop(uid):
    cat.uncatalogObject(uid)
    del entries[uid]


def tree(top, folders, subs, items, deep):
    """A folder `top` with `folders` folders, each with `subs` subfolders,
    each with `items` items and one deeper folder of `deep` items."""
    for f in range(folders):
        folder = f"{top}/folder-{f}"
        add(folder)
        for s in range(subs):
            sub = f"{folder}/sub-{s}"
            add(sub)
            for i in range(items):
                add(f"{sub}/item-{i}")
            if deep:
                add(f"{sub}/deep")
                for i in range(deep):
                    add(f"{sub}/deep/item-{i}")


tree("/site", 4, 3, 5, 2)
add("/other/item")
add("/lone")
# An entry whose `path` attribute holds empty components.
add("/site/folder-1/odd", path="/site//folder-1/odd/")
transaction.commit()


def records():
    return {struct.unpack(">Q", oid)[0]: storage.load(oid) for oid in storage._data}


def as_hex(found):
    return {f"{oid:016x}": data.hex() for oid, (data, _) in sorted(found.items())}


base = records()


def path_clause(index, name, rec):
    """The clause of `rec` on an ExtendedPathIndex, as zoe.plone.catalog
    builds it."""
    keys = []
    for k in rec.keys:
        if isinstance(k, str):
            keys.append([k, None])
        else:
            keys.append([k[0], int(k[1])])
    navtree = bool(rec.get("navtree", 0))
    return {
        "k": "p",
        "i": name,
        "q": keys,
        "lv": int(rec.get("level", 0)),
        "and": rec.operator == "and",
        "d": int(getattr(rec, "depth", -1)),
        "nt": navtree,
        "ns": int(rec.get("navtree_start", 0)),
        "ip": [p for p in index.getPhysicalPath() if p],
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
        if isinstance(aq_base(index), ExtendedPathIndex):
            clauses.append(path_clause(index, name, rec))
            continue
        clauses.append(
            {
                "k": "v",
                "i": name,
                "q": [index._convert(k) for k in rec.keys if k is not None],
                "and": rec.operator == "and",
                "r": None,
                "not": [],
            }
        )
    sort = []
    sort_indexes = cat._getSortIndex(merged)
    if sort_indexes is not None:
        order = cat._get_sort_attr("order", merged)
        reverse = bool(order) and order.lower() in ("reverse", "descending")
        sort.append([sort_indexes.getId(), reverse])
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


def reads_index_items(query):
    """Tell if `ExtendedPathIndex.search` reads `_index_items` for a key of
    the path clause of `query`: a search at the level 0 with a depth of 0
    without `multi_valued`, a navtree of depth 0, and a navtree of depth 1
    with `multi_valued`, that its `navtree_start` does not empty."""
    made = cat.make_query(cat.merge_query_args(query))
    index = cat.getIndex("path")
    rec = IndexQuery(
        made, "path", index.query_options, index.operators, index.useOperator
    )
    if rec.keys is None:
        return False
    default = rec.get("level", 0)
    depth = getattr(rec, "depth", -1)
    navtree = rec.get("navtree", 0)
    start = rec.get("navtree_start", 0)
    for key in rec.keys:
        path, level = (key, default) if isinstance(key, str) else (key[0], int(key[1]))
        levels = range(index._depth + 1) if level < 0 else [level]
        comps = [p for p in path.split("/") if p]
        for level in levels:
            d = 1 if navtree and depth == -1 else depth
            if navtree and start > min(level + len(comps) - 1 + d, index._depth):
                continue
            if level == 0 and d in (0, 1):
                if navtree and (d == 0 or index.multi_valued):
                    return True
                if not navtree and d == 0 and not index.multi_valued:
                    return True
    return False


def answer(query):
    if query.get("sort_on") == "path":
        return {"f": "sort"}
    if reads_index_items(query):
        return {"f": "index_unavailable"}
    res = cat.searchResults(dict(query))
    return {
        "r": list(res),
        "n": len(res),
        "a": getattr(res, "actual_result_count", len(res)),
    }


QUERIES = {
    "listing": {"path": {"query": "/site/folder-0", "depth": 1}},
    "listing-sub": {"path": {"query": "/site/folder-2/sub-1", "depth": 1}},
    "listing-root": {"path": {"query": "/", "depth": 1}},
    "listing-empty": {"path": {"query": "", "depth": 1}},
    "listing-relative": {"path": {"query": "site/folder-1", "depth": 1}},
    "listing-trailing-slash": {"path": {"query": "/site/folder-0/", "depth": 1}},
    "listing-site": {"path": {"query": "/site", "depth": 1}},
    "site": {"path": "/site"},
    "site-depth-minus-1": {"path": {"query": "/site", "depth": -1}},
    "site-catalog": {"path": "/site/portal_catalog"},
    "subtree": {"path": "/site/folder-1"},
    "subtree-sub": {"path": "/site/folder-0/sub-1"},
    "subtree-absent": {"path": "/site/nothing"},
    "outside": {"path": "/other"},
    "depth-2": {"path": {"query": "/site/folder-0", "depth": 2}},
    "depth-3": {"path": {"query": "/site/folder-2", "depth": 3}},
    "depth-2-site": {"path": {"query": "/site", "depth": 2}},
    "depth-999": {"path": {"query": "/site/folder-3", "depth": 999}},
    "depth-0": {"path": {"query": "/site/folder-0/sub-1", "depth": 0}},
    "depth-0-level-1": {"path": {"query": "folder-0/sub-1", "level": 1, "depth": 0}},
    "several": {"path": ["/site/folder-0", "/site/folder-2/sub-0"]},
    "several-depth-1": {
        "path": {"query": ["/site/folder-0", "/site/folder-3"], "depth": 1}
    },
    "and": {
        "path": {"query": ["/site/folder-1", "/site/folder-1/sub-2"], "operator": "and"}
    },
    "level-any": {"path": {"query": "sub-1", "level": -1}},
    "level-any-depth-1": {"path": {"query": "deep", "level": -1, "depth": 1}},
    "level-any-depth-0": {"path": {"query": "sub-1", "level": -1, "depth": 0}},
    "level-1": {"path": {"query": "folder-2", "level": 1}},
    "level-2-depth-1": {"path": {"query": "sub-2", "level": 2, "depth": 1}},
    "level-1-several": {
        "path": {"query": ["folder-0", "folder-3"], "level": 1, "depth": 1}
    },
    "tuples": {"path": {"query": [("folder-1", 1), ("sub-0", 2)]}},
    "tuple-any-level": {"path": {"query": [("item-2", -1)]}},
    "navtree": {"path": {"query": "/site/folder-0/sub-1/item-3", "navtree": 1}},
    "navtree-start": {
        "path": {"query": "/site/folder-0/sub-1/deep", "navtree": 1, "navtree_start": 2}
    },
    "navtree-portlet": {
        "path": {"query": "/site/folder-2/sub-0", "navtree": 1, "navtree_start": 1},
        "portal_type": ["Document", "Folder"],
    },
    "navtree-depth-2": {
        "path": {"query": "/site/folder-0/sub-1", "navtree": 1, "depth": 2}
    },
    "navtree-depth-2-start": {
        "path": {
            "query": "/site/folder-1/sub-2",
            "navtree": 1,
            "depth": 2,
            "navtree_start": 2,
        }
    },
    "navtree-depth-0": {
        "path": {"query": "/site/folder-0/sub-1", "navtree": 1, "depth": 0}
    },
    "navtree-depth-0-excluded": {
        "path": {
            "query": "/site/folder-0",
            "navtree": 1,
            "depth": 0,
            "navtree_start": 3,
        }
    },
    "navtree-level-1": {"path": {"query": "folder-0/sub-1", "level": 1, "navtree": 1}},
    "navtree-start-beyond": {
        "path": {"query": "/site/folder-0", "navtree": 1, "navtree_start": 9}
    },
    "navtree-depth-999": {
        "path": {"query": "/site/folder-3", "navtree": 1, "depth": 999}
    },
    "with-type": {"path": "/site/folder-0", "portal_type": "Document"},
    "with-type-sorted": {
        "path": {"query": "/site/folder-1", "depth": 2},
        "portal_type": ["Document", "News Item"],
        "sort_on": "sortable_title",
    },
    "with-type-limit": {
        "path": "/site",
        "portal_type": "Folder",
        "sort_on": "sortable_title",
        "sort_order": "reverse",
        "sort_limit": 7,
    },
    "sort-on-path": {"portal_type": "Document", "sort_on": "path"},
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


depth = cat.getIndex("path")._depth


def split():
    for i in range(5, 50):
        add(f"/site/folder-3/sub-2/item-{i}")
        add(f"/site/folder-1/sub-1/deep/item-{i}")


before, changed = commit("split", split)
assert any(
    oid not in before and b"IOBucket" in data[:60] for oid, (data, _) in changed.items()
), "no bucket split"


def move_tree(old, new):
    """Move the subtree at `old` to `new`, as CMFCore does: each old path
    leaves the catalog, each new path comes in with a new rid."""
    moved = sorted(uid for uid in entries if uid == old or uid.startswith(old + "/"))
    for uid in moved:
        _, values = entries[uid]
        drop(uid)
        add(new + uid[len(old) :], **{k: v for k, v in values.items() if k != "getId"})


def move():
    # A folder of three levels: `sub-1`, its items and `deep`, and the
    # items of `deep`.
    move_tree("/site/folder-0/sub-1", "/site/folder-2/sub-moved")


commit("move", move)


def rename():
    move_tree("/site/folder-1/sub-2", "/site/folder-1/sub-renamed")


commit("rename", rename)


def repath():
    # Three entries keep their uid and their rid, and take another path:
    # one level up, one level down, and another folder.
    for uid, physical in [
        ("/site/folder-3/sub-0/item-1", "/site/folder-3/item-1"),
        ("/site/folder-3/sub-0/item-2", "/site/folder-3/sub-0/deep/item-9"),
        ("/site/folder-2/sub-0/item-0", "/site/folder-1/sub-0/item-0"),
    ]:
        _, values = entries[uid]
        cat.catalogObject(Stub(physical, **values), uid)
        entries[uid] = (physical, values)


commit("repath", repath)


def delete():
    for uid in sorted(u for u in entries if u.startswith("/site/folder-2/")):
        drop(uid)
    drop("/site/folder-2")


commit("delete", delete)


def twin():
    add("/site")
    add("/site/twin", path="/site/folder-1/sub-0")


commit("twin", twin)


def untwin():
    drop("/site/twin")
    drop("/lone")


commit("untwin", untwin)
index = cat.getIndex("path")
# The commits stay within the deepest level of the base: the index record
# does not change.
assert index._depth == depth, (index._depth, depth)
# `_index_items` lost the path of the twin, and kept the path of `/lone`.
assert "/site/folder-1/sub-0" not in index._index_items
assert "/lone" in index._index_items

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
