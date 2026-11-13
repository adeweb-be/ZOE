"""Write the test data of `tests/order.rs`: the folders of plone.folder and
the catalog entries of their items, the records of the order and of the
positions of each folder as `DefaultOrdering` writes them, and the answers
of ZCatalog to listings sorted on the `GopipIndex` of plone.folder, after
each commit.

    PYTHONHASHSEED=0 python order.py OUT.json

Run it with an interpreter that has Products.ZCatalog 7.2,
Products.ExtendedPathIndex 5, plone.folder 4, ZODB 6 and BTrees 6, such as
the interpreter of the zoe scenario of `just plone`.

The Zope root holds the folder `site`, which holds the folders `f0` and
`f1` and two items; `f0` also holds the folder `sub` with two items. Each
folder is an `OrderedBTreeFolderBase` with `DefaultOrdering`, so its order
is a `PersistentList` and its positions an `OIBTree` in its annotations.
The catalog is a `Products.ZCatalog.Catalog.Catalog` at `root["catalog"]`
of a `MappingStorage`, with the ExtendedPathIndex `path` (physical path
`/site/portal_catalog/path`), the FieldIndex `portal_type`, the
`GopipIndex` `getObjPositionInParent` and the column `getId`. A seeded
random gives each entry its rid. `/site/f0/ghost` has an entry and no item
in `f0`: an id without a place. `/site/f1/item-zero` has the rid 0, which
`documentToKeyMap` gives no place.

The states, each a commit after the one before:

- `base`: the folders, their items in a shuffled order, and the entries.
- `move`: `moveObjectsByDelta` of two items by -3, then `moveObjectsToTop`.
- `rename`: a rename of an item, as `manage_renameObject` of plone.folder
  does it: the item keeps its place, the entry gets a new rid.
- `create-delete`: a new item at the end, and the delete of an item, which
  rebuilds the positions.
- `disagree`: the first two ids of the order of `f0` change places, and the
  positions stay: the order and the positions disagree.
- `move-again`: `moveObjectsToBottom` of the first item of `f0`. The test
  also reads its records as the transaction overlay of a query at the tid
  of `disagree`.

Each state holds the entries, the ids of the order of each folder, the
records of the annotations, the order and the positions of `f0` (hex, with
the oids of the `MappingStorage`), the items of the positions of each
folder, and the answer of `searchResults` to each query: the rids in
order, `len()` and `actual_result_count`; or the reason `sort` for a query
whose result has several containers, which the mirror refuses.
"""

import json
import random
import struct
import sys

import Products.ZCatalog.Catalog as catalog_module
import transaction
from Acquisition import Implicit
from OFS.SimpleItem import SimpleItem
from plone.folder.default import DefaultOrdering
from plone.folder.interfaces import IOrderableFolder
from plone.folder.interfaces import IOrdering
from plone.folder.nogopip import GopipIndex
from plone.folder.ordered import OrderedBTreeFolderBase
from Products.CMFCore.interfaces import ISiteRoot
from Products.ExtendedPathIndex.ExtendedPathIndex import ExtendedPathIndex
from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
from Products.ZCatalog.Catalog import Catalog
from ZODB import DB
from ZODB.MappingStorage import MappingStorage
from zope.annotation.attribute import AttributeAnnotations
from zope.component import provideAdapter
from zope.component import provideUtility

out = sys.argv[1]
rnd = random.Random(302)
catalog_module.randint = random.Random(303).randint

provideAdapter(AttributeAnnotations)
provideAdapter(DefaultOrdering, (IOrderableFolder,), IOrdering)

# The catalog has no acquisition parent: the index takes the physical path
# of the index of a site.
ExtendedPathIndex.getPhysicalPath = lambda self: ("", "site", "portal_catalog", "path")
# The catalog cannot make brains: each item of a result is its rid.
Catalog.instantiate = lambda self, record, score_data=None: record[0]

ORDER_KEY = "plone.folder.ordered.order"
POS_KEY = "plone.folder.ordered.pos"


class Folder(Implicit, OrderedBTreeFolderBase):
    meta_type = "Folder"


class Item(SimpleItem):
    meta_type = "Item"

    def __init__(self, id):
        self.id = id


class Stub:
    def __init__(self, physical, **values):
        self.__dict__.update(values)
        self._physical = tuple(physical.split("/"))

    def getPhysicalPath(self):
        return self._physical


class Holder:
    def __init__(self, catalog):
        self._catalog = catalog


storage = MappingStorage()
db = DB(storage)
conn = db.open()
root = conn.root()
app = Folder("")
root["app"] = app


class SiteRoot:
    def getPhysicalRoot(self):
        return app


provideUtility(SiteRoot(), ISiteRoot)

cat = Catalog()
root["catalog"] = cat
cat.addIndex("path", ExtendedPathIndex("path"))
cat.addIndex("portal_type", FieldIndex("portal_type"))
cat.addIndex(
    "getObjPositionInParent",
    GopipIndex("getObjPositionInParent", caller=Holder(cat)),
)
cat.addColumn("getId")
transaction.commit()

entries = {}


def index(path, portal_type, rid=None):
    if rid is None:
        cat._v_nextid = rnd.randint(-2_000_000_000, 2_000_000_000)
    else:
        cat._v_nextid = rid
        if rid % 4000 == 0:
            saved = catalog_module.randint
            catalog_module.randint = lambda a, b: rid
    name = path.rsplit("/", 1)[1]
    cat.catalogObject(Stub(path, portal_type=portal_type, getId=name), path)
    if rid is not None and rid % 4000 == 0:
        catalog_module.randint = saved
    entries[path] = (cat.uids[path], portal_type)


def unindex(path):
    cat.uncatalogObject(path)
    del entries[path]


def add(folder, path, id, portal_type):
    folder._setOb(id, Item(id) if portal_type != "Folder" else Folder(id))
    index(f"{path}/{id}", portal_type)


def type_of(i):
    return "News Item" if i % 3 == 0 else "Document"


site = Folder("site")
app._setOb("site", site)
for id in ("f0", "f1"):
    site._setOb(id, Folder(id))
    index(f"/site/{id}", "Folder")
add(site, "/site", "doc-a", "Document")
add(site, "/site", "doc-b", "News Item")
f0, f1 = site["f0"], site["f1"]
names = [f"item-{i:02d}" for i in range(30)]
rnd.shuffle(names)
for name in names:
    add(f0, "/site/f0", name, type_of(int(name[-2:])))
add(f0, "/site/f0", "sub", "Folder")
sub = f0["sub"]
for k in range(2):
    add(sub, "/site/f0/sub", f"deep-{k}", "Document")
for i in range(12):
    add(f1, "/site/f1", f"item-{i:02d}", type_of(i))
# An entry with no item in the folder: an id without a place.
index("/site/f0/ghost", "Document")
# An item of f1 whose entry has the rid 0.
f1._setOb("item-zero", Item("item-zero"))
index("/site/f1/item-zero", "Document", rid=0)
transaction.commit()

FOLDERS = {"/site": site, "/site/f0": f0, "/site/f1": f1, "/site/f0/sub": sub}


def listing(path, **extra):
    query = {"path": {"query": path, "depth": 1}, "sort_on": "getObjPositionInParent"}
    query.update(extra)
    return query


QUERIES = {
    "f0": listing("/site/f0"),
    "f0-reverse": listing("/site/f0", sort_order="reverse"),
    "f0-limit": listing("/site/f0", sort_limit=5),
    "f0-limit-reverse": listing("/site/f0", sort_limit=5, sort_order="reverse"),
    "f0-limit-large": listing("/site/f0", sort_limit=10),
    "f0-batch": listing("/site/f0", b_start=0, b_size=10),
    "f0-batch-middle": listing("/site/f0", b_start=10, b_size=10),
    "f0-batch-end": listing("/site/f0", b_start=25, b_size=10),
    "f0-batch-end-reverse": listing(
        "/site/f0", b_start=20, b_size=5, sort_order="reverse"
    ),
    "f0-batch-past": listing("/site/f0", b_start=40, b_size=10),
    "f0-documents": listing("/site/f0", portal_type="Document"),
    "f0-news-limit": listing("/site/f0", portal_type="News Item", sort_limit=2),
    "f0-trailing-slash": listing("/site/f0/"),
    "site": listing("/site"),
    "f1": listing("/site/f1"),
    "f1-reverse": listing("/site/f1", sort_order="descending"),
    "sub": listing("/site/f0/sub"),
    "nothing": listing("/site/f0", portal_type="Event"),
}

# Queries with an order clause whose result has several containers: the
# mirror refuses them with `sort`.
SEVERAL = {
    "f0-depth-2": (
        {
            "path": {"query": "/site/f0", "depth": 2},
            "sort_on": "getObjPositionInParent",
        },
        "/site/f0",
    ),
}


def plan_of(query, container):
    """The plan of `query`, as zoe.plone.catalog builds it, without its
    order clause: the test adds it with the oid of the order of
    `container`."""
    made = cat.make_query(cat.merge_query_args(query))
    path = made["path"]
    path = path if isinstance(path, dict) else {"query": path}
    clauses = [
        {
            "k": "p",
            "i": "path",
            "q": [[path["query"], None]],
            "lv": 0,
            "and": False,
            "d": int(path.get("depth", -1)),
            "nt": False,
            "ns": 0,
            "ip": ["site", "portal_catalog", "path"],
        }
    ]
    if "portal_type" in made:
        clauses.append(
            {
                "k": "v",
                "i": "portal_type",
                "q": [made["portal_type"]],
                "and": False,
                "r": None,
                "not": [],
            }
        )
    order = str(made.get("sort_order", ""))
    b_size = made.get("b_size", None)
    return {
        "c": clauses,
        "s": [["getObjPositionInParent", order.lower() in ("reverse", "descending")]],
        "l": made.get("sort_limit", None),
        "bs": int(made.get("b_start", 0)),
        "bz": int(b_size) if b_size is not None else None,
        "container": container,
    }


def container_of(query):
    path = query["path"]["query"]
    return "/" + "/".join(p for p in path.split("/") if p)


def answer(query):
    res = cat.searchResults(dict(query))
    return {
        "r": list(res),
        "n": len(res),
        "a": getattr(res, "actual_result_count", len(res)),
    }


def by_positions(query, folder):
    """The rids of the answer to `query` in the order that the positions of
    `folder` give, ascending: what a sort on the positions would give."""
    pos = folder.getOrdering()._pos()
    res = cat.searchResults(
        {k: v for k, v in query.items() if not k.startswith("sort")}
    )
    ids = [(cat.paths[rid].rsplit("/", 1)[1], rid) for rid in res]
    return [rid for _, rid in sorted((pos[id], rid) for id, rid in ids if id in pos)]


def records_of(folder):
    annotations = folder.__annotations__
    oids = {
        "annotations": annotations._p_oid,
        "order": annotations[ORDER_KEY]._p_oid,
        "positions": annotations[POS_KEY]._p_oid,
    }
    return {
        name: {
            "oid": struct.unpack(">Q", oid)[0],
            "record": storage.load(oid)[0].hex(),
        }
        for name, oid in oids.items()
    }


states = []


def state(name):
    transaction.commit()
    states.append(
        {
            "name": name,
            "entries": sorted(
                [rid, path, kind] for path, (rid, kind) in entries.items()
            ),
            "orders": {
                path: list(folder.getOrdering()._order())
                for path, folder in FOLDERS.items()
            },
            "positions": {
                path: sorted(folder.getOrdering()._pos().items())
                for path, folder in FOLDERS.items()
            },
            "records": records_of(f0),
            "answers": {
                **{name: answer(query) for name, query in QUERIES.items()},
                **{name: {"f": "sort"} for name in SEVERAL},
            },
            "by_positions": by_positions(QUERIES["f0"], f0),
        }
    )


state("base")

f0.moveObjectsByDelta([names[7], names[19]], -3)
f0.moveObjectsToTop([names[25]])
state("move")


def rename(folder, path, old, new):
    position = folder.getObjectPosition(old)
    item = folder._getOb(old)
    kind = entries[f"{path}/{old}"][1]
    folder._delOb(old)
    unindex(f"{path}/{old}")
    item.id = new
    folder._setOb(new, item)
    folder.moveObjectToPosition(new, position, suppress_events=True)
    index(f"{path}/{new}", kind)


rename(f0, "/site/f0", names[3], "item-renamed")
state("rename")

add(f0, "/site/f0", "item-30", "Document")
f0._delOb(names[11])
unindex(f"/site/f0/{names[11]}")
state("create-delete")

order = f0.getOrdering()._order()
order[0], order[1] = order[1], order[0]
state("disagree")

f0.moveObjectsToBottom([f0.getOrdering().idsInOrder()[0]])
state("move-again")

data = {
    "queries": {
        **{
            name: plan_of(query, container_of(query)) for name, query in QUERIES.items()
        },
        **{
            name: plan_of(query, container)
            for name, (query, container) in SEVERAL.items()
        },
    },
    "states": states,
}
with open(out, "w") as f:
    json.dump(data, f, indent=1, sort_keys=True)
    f.write("\n")
