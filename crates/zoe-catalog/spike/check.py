"""The check of the mirror prototype: typed plans built as the client would
build them, with ZCatalog's own IndexQuery and the conversion of each
index, and the answers of ZCatalog on the same store.

    interpreter check.py STORE PLANS_OUT OVERLAYS_OUT MANIFEST

Each line of PLANS_OUT holds a query, its plan (null when an index of the
query is not mirrored) and the answer of ZCatalog: the rids in order, the
length of the lazy result and actual_result_count. Each line of
OVERLAYS_OUT holds the changed records of a transaction that changed the
catalog, with provisional oids for its new objects, and the answers of
ZCatalog inside that transaction.
"""

import io
import json
import struct
import sys
import time

import zoe
from DateTime import DateTime
from Products.ZCatalog.query import IndexQuery
from ZODB import DB
from ZODB.serialize import ObjectWriter

store, plans_out, overlays_out, manifest_path = sys.argv[1:5]

VALUE = ("FieldIndex", "DateIndex", "UUIDIndex", "KeywordIndex")


def typed(v):
    if v is None:
        return {"n": None}
    if isinstance(v, bool):
        return {"b": v}
    if isinstance(v, int):
        return {"i": v}
    if isinstance(v, float):
        return {"f": v}
    if isinstance(v, str):
        return {"s": v}
    if isinstance(v, tuple):
        return {"t": [typed(x) for x in v]}
    raise TypeError(repr(v))


def plan_of(inner, query):
    # searchResults takes the sort from the query before make_query, which
    # sorts each list of the query.
    merged = inner.merge_query_args(query)
    query = inner.make_query(merged)
    clauses = []
    for name, raw in inner.indexes.items():
        names = inner._get_index_query_names(raw)
        if not any(n in query for n in names):
            continue
        index = inner.getIndex(name)
        meta = index.meta_type
        rec = IndexQuery(query, index.id, index.query_options, index.operators, index.useOperator)
        if rec.keys is None:
            continue
        if meta in VALUE:
            if rec.get("usage", None):
                return None, f"usage on {name}"
            rng = rec.get("range", None)
            not_p = rec.get("not", None)
            if not rec.keys and not_p:
                keys = []
            else:
                keys = [index._convert(k) for k in rec.keys]
            nots = [index._convert(k) for k in (not_p or [])]
            clauses.append(
                {
                    "kind": "value",
                    "index": name,
                    "keys": [typed(k) for k in keys],
                    "and": rec.operator == "and",
                    "range": [rng.find("min") > -1, rng.find("max") > -1] if rng else None,
                    "not": [typed(k) for k in nots],
                }
            )
        elif meta == "BooleanIndex":
            clauses.append(
                {"kind": "boolean", "index": name, "key": bool(rec.keys[0]) if rec.keys else None}
            )
        elif meta == "DateRangeIndex":
            clauses.append(
                {"kind": "daterange", "index": name, "term": index._convertDateTime(rec.keys[0])}
            )
        elif meta == "ExtendedPathIndex":
            paths = []
            for k in rec.keys:
                if isinstance(k, str):
                    paths.append([k, None])
                else:
                    paths.append([k[0], int(k[1])])
            clauses.append(
                {
                    "kind": "path",
                    "index": name,
                    "paths": paths,
                    "level": rec.get("level", 0),
                    "and": rec.get("operator", index.useOperator).lower() == "and",
                    "depth": getattr(rec, "depth", -1),
                    "navtree": bool(rec.get("navtree", 0)),
                    "navtree_start": rec.get("navtree_start", 0),
                    "index_path": list(index.getPhysicalPath()),
                }
            )
        else:
            return None, f"{meta} {name} is not mirrored"
    sort = []
    sort_indexes = inner._getSortIndex(merged)
    if sort_indexes is not None:
        if not isinstance(sort_indexes, list):
            sort_indexes = [sort_indexes]
        order = inner._get_sort_attr("order", merged)
        if order is None:
            order = [""]
        elif isinstance(order, str):
            order = [order]
        reverse = [o.lower() in ("reverse", "descending") for o in order]
        for i, s in enumerate(sort_indexes):
            # One order applies to every index. A list of orders gives the
            # order of each index; an index past its end has none, and
            # ZCatalog's multisort compares it only in its first sort.
            if len(reverse) == 1:
                r = reverse[0]
            else:
                r = reverse[i] if i < len(reverse) else None
            sort.append([s.getId(), r])
    b_size = query.get("b_size", None)
    return {
        "clauses": clauses,
        "sort": sort,
        "limit": inner._get_sort_attr("limit", merged),
        "b_start": int(query.get("b_start", 0)),
        "b_size": int(b_size) if b_size is not None else None,
    }, None


def answer(inner, query):
    started = time.time()
    res = inner.searchResults(dict(query))
    rids = [b.getRID() for b in res]
    return {
        "rids": rids,
        "len": len(res),
        "actual": getattr(res, "actual_result_count", len(res)),
        "seconds": round(time.time() - started, 3),
    }


st = zoe.Storage(store, read_only=True)
db = DB(st, cache_size=300000)
conn = db.open()
app = conn.root()["Application"]
site = app.Plone
inner = site.portal_catalog._catalog
print("catalog", hex(struct.unpack(">Q", inner._p_oid)[0]), len(inner), flush=True)

manifest = json.load(open(manifest_path))
queries = []
for q in manifest["queries"]:
    if q["kind"] == "intersection":
        queries.append((q["name"], dict(q["query"])))

# Paths of the fixture: a unit, a folder of depth 2, a leaf, a document.
paths = inner.paths
some = [paths[r] for r, _ in zip(paths.keys(), range(2000))]
deep = sorted((p for p in some if p.count("/") >= 5), key=len)
doc = deep[0]
parts = doc.split("/")
leaf = "/".join(parts[:-1])
unit = "/".join(parts[:3])
folder2 = "/".join(parts[:4])
print("paths", unit, folder2, leaf, doc, flush=True)
listing = dict(manifest["queries"][0]["query"])
uids = [inner.getIndex("UID").getEntryForObject(r) for r, _ in zip(paths.keys(), range(3))]

variants = [
    ("listing-nolimit", {k: v for k, v in listing.items() if k != "sort_limit"}),
    ("listing-asc", {**listing, "sort_order": "ascending"}),
    ("listing-page", {**{k: v for k, v in listing.items() if k != "sort_limit"}, "b_start": 100, "b_size": 20}),
    ("listing-far-page", {**{k: v for k, v in listing.items() if k != "sort_limit"}, "b_start": 400000, "b_size": 50}),
    ("listing-last-page", {**{k: v for k, v in listing.items() if k != "sort_limit"}, "b_start": 481030, "b_size": 50}),
    ("state-iterate-index", {"portal_type": "Document", "sort_on": "review_state"}),
    ("state-iterate-index-desc", {"portal_type": "News Item", "sort_on": "review_state", "sort_order": "reverse"}),
    ("state-nbest-ties", {"portal_type": "Document", "sort_on": "review_state", "sort_order": "descending", "sort_limit": 25}),
    ("state-nbest-reverse-ties", {"portal_type": "Document", "sort_on": "review_state", "sort_limit": 25}),
    ("creator-nbest-ties", {"portal_type": ["Document", "News Item"], "sort_on": "Creator", "sort_order": "descending", "sort_limit": 40}),
    ("multisort-mixed", {"review_state": "pending", "sort_on": ["portal_type", "sortable_title"], "sort_order": ["ascending", "descending"], "sort_limit": 40}),
    ("multisort-same", {"portal_type": "News Item", "sort_on": ["review_state", "Creator"], "sort_order": "descending", "sort_limit": 10}),
    ("unsorted-batch", {"portal_type": "Folder", "b_start": 10, "b_size": 15}),
    ("unsorted-small", {"Creator": "editor-001", "review_state": "pending"}),
    ("path-subtree", {"path": unit, "portal_type": "Document", "sort_on": "sortable_title", "sort_limit": 50}),
    ("path-subtree-count", {"path": folder2}),
    ("path-children", {"path": {"query": leaf, "depth": 1}, "sort_on": "sortable_title"}),
    ("path-object", {"path": {"query": doc, "depth": 0}}),
    ("path-depth2", {"path": {"query": folder2, "depth": 2}}),
    ("path-two", {"path": {"query": [leaf, folder2], "depth": 1}}),
    ("navtree", {"path": {"query": doc, "navtree": 1, "navtree_start": 1}, "is_default_page": False}),
    ("navtree-depth999", {"path": {"query": leaf, "navtree": 1, "depth": 999}}),
    ("breadcrumbs", {"path": {"query": doc, "navtree": 1, "depth": 0}}),
    ("path-relative-level", {"path": {"query": parts[-2], "level": 3}}),
    ("path-any-level", {"path": {"query": parts[-2], "level": -1}}),
    ("not-only", {"review_state": {"not": "published"}, "portal_type": "Folder"}),
    ("not-with-keys", {"portal_type": {"query": ["Document", "News Item"], "not": "News Item"}, "Creator": "editor-002"}),
    ("keyword-and", {"Subject": {"query": ["hegenifu", manifest["queries"][2]["query"]["Subject"]], "operator": "and"}}),
    ("keyword-or", {"allowedRolesAndUsers": ["user:editor-003", "group:unit-003"], "portal_type": "Folder"}),
    ("range-int", {"bench_field_01": {"query": [100, 2000], "range": "min:max"}, "review_state": "pending"}),
    ("range-date-max", {"created": {"query": "2024-02-01T00:00:00+00:00", "range": "max"}, "sort_on": "created", "sort_limit": 5}),
    ("range-date-min-sorted", {"modified": {"query": "2026-01-01T00:00:00+00:00", "range": "min"}, "sort_on": "modified", "sort_order": "descending", "sort_limit": 15}),
    ("uid-one", {"UID": uids[0]}),
    ("uid-list", {"UID": uids}),
    ("boolean", {"is_folderish": True, "path": unit}),
    ("daterange-alone", {"effectiveRange": DateTime("2025-12-25T18:40:00+00:00"), "sort_on": "effective", "sort_order": "descending", "sort_limit": 10}),
    ("limit-over", {"Creator": "editor-004", "review_state": "private", "sort_on": "modified", "sort_limit": 100000}),
    ("empty-keys", {"portal_type": []}),
    ("unknown-key", {"Language": "en", "portal_type": "Collection", "sort_on": "id"}),
    ("unmirrored-index", {"start": {"query": DateTime("2025-01-01"), "range": "min"}, "portal_type": "Document"}),
]
queries.extend(variants)

with open(plans_out, "w") as out:
    for name, query in queries:
        plan, reason = plan_of(inner, query)
        expected = answer(inner, query)
        line = {"name": name, "query": repr(query), "plan": plan, "reason": reason, "expected": expected}
        out.write(json.dumps(line) + "\n")
        print(name, expected["len"], expected["actual"], expected["seconds"], "s", reason or "", flush=True)
        conn.cacheGC()


# The overlay of a transaction that changed the catalog.
class Stub:
    def __init__(self, path, **values):
        self.__dict__.update(values)
        self._physical = tuple(path.split("/"))

    def getPhysicalPath(self):
        return self._physical


class Writer(ObjectWriter):
    """ObjectWriter, but a new object gets a provisional oid and keeps none."""

    def __init__(self, jar, provisional):
        super().__init__()
        self._jar = jar
        self.provisional = provisional

    def persistent_id(self, obj):
        if getattr(obj, "_p_oid", 1) is None and not isinstance(obj, type):
            if id(obj) not in self.provisional:
                oid = struct.pack(">Q", (1 << 63) + len(self.provisional))
                self.provisional[id(obj)] = (oid, obj)
            oid = self.provisional[id(obj)][0]
            return oid, type(obj)
        return super().persistent_id(obj)


def overlay_records(conn):
    provisional = {}
    out = []
    started = time.time()
    for obj in list(conn._registered_objects):
        data = Writer(conn, provisional).serialize(obj)
        out.append([struct.unpack(">Q", obj._p_oid)[0], data])
    done = set()
    while len(done) < len(provisional):
        for key, (oid, obj) in list(provisional.items()):
            if key in done:
                continue
            done.add(key)
            data = Writer(conn, provisional).serialize(obj)
            out.append([struct.unpack(">Q", oid)[0], data])
    return out, time.time() - started


def transaction_check(name, change, checks):
    import transaction

    transaction.abort()
    conn.cacheMinimize()
    change()
    records, seconds = overlay_records(conn)
    plans = []
    for qname, query in checks:
        plan, reason = plan_of(inner, query)
        plans.append({"name": qname, "plan": plan, "reason": reason, "expected": answer(inner, query)})
    line = {
        "name": name,
        "records": [[f"{oid:x}", data.hex()] for oid, data in records],
        "registered": len(conn._registered_objects),
        "pickle_seconds": seconds,
        "plans": plans,
    }
    with open(overlays_out, "a") as out:
        out.write(json.dumps(line) + "\n")
    print(name, len(records), "records", sum(len(d) for _, d in records), "bytes", round(seconds, 3), "s", flush=True)
    transaction.abort()


open(overlays_out, "w").close()
rid_doc = inner.uids[doc]
editor = inner.getIndex("Creator").getEntryForObject(rid_doc)


def edit():
    stub = Stub(
        doc,
        review_state="private",
        Subject=("spikesubject", "hegenifu"),
        modified=DateTime("2026-06-01T10:00:00+00:00"),
        sortable_title="aaaa spike title",
    )
    inner.catalogObject(stub, doc, idxs=["review_state", "Subject", "modified", "sortable_title"], update_metadata=False)


def create():
    edit()
    new_path = leaf + "/spike-new-document"
    stub = Stub(
        new_path,
        portal_type="Document",
        review_state="published",
        allowedRolesAndUsers=["Anonymous"],
        effective=DateTime("2025-12-01T10:00:00+00:00"),
        expires=DateTime("2499-12-31T00:00:00+00:00"),
        created=DateTime("2025-12-01T10:00:00+00:00"),
        modified=DateTime("2025-12-01T10:00:00+00:00"),
        is_folderish=False,
        is_default_page=False,
        exclude_from_nav=False,
        Subject=("spikesubject",),
        sortable_title="zzzz spike new",
        getId="spike-new-document",
        id="spike-new-document",
        Creator=editor,
        Title="Spike new",
    )
    inner.catalogObject(stub, new_path)
    old = inner.paths[next(iter(inner.getIndex("path")._index_parents[leaf]))]
    if old != doc:
        inner.uncatalogObject(old)


checks = [
    ("listing", listing),
    ("subject", {"Subject": "spikesubject", "sort_on": "modified", "sort_order": "descending"}),
    ("private-of-editor", {"Creator": editor, "review_state": "private", "sort_on": "modified", "sort_order": "descending", "sort_limit": 20}),
    ("leaf-children", {"path": {"query": leaf, "depth": 1}, "sort_on": "sortable_title"}),
    ("leaf-children-sorted-desc", {"path": {"query": leaf, "depth": 1}, "sort_on": "sortable_title", "sort_order": "descending", "sort_limit": 3}),
]
transaction_check("edit", edit, checks)
transaction_check("create-edit-remove", create, checks)
db.close()
print("done", flush=True)
