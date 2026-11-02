"""Write the digest of the catalog of the site `Plone` of a store, its
counts, and the hits of a query set, as one JSON document.

    <scenario>/parts/instance/bin/interpreter catalog_digest.py \
        filestorage|zoe PATH OUT_JSON [QUERIES_JSON]

The script opens the store read-only, a FileStorage `Data.fs` or a ZOE
store directory, with no configuration of Zope, and changes no byte of it.
It never commits, so it frees the object cache itself as it walks.

The digest holds a hash of each index, of each metadata column, of `uids`
and of `paths`, and a hash of these. The hash of an object covers its
state: each attribute in the order of the names, each tree item by item in
the order of the keys. Two stores with the same digest hold the same
catalog. The counts are the length of the catalog, the entries of each
index (the documents of a ZCTextIndex) and the values of each column that
are not Missing.Value. QUERIES_JSON holds a list of queries, or a document
with the list under `queries`, each with a `name` and a `query` of keyword
arguments for `unrestrictedSearchResults`; the document gives the length of
the result of each.
"""

import hashlib
import json
import sys
import types

from Acquisition import aq_base
from BTrees.Length import Length
from DateTime import DateTime
from Missing import MV
from ZODB import DB

SITE_ID = "Plone"
# The items of a tree in one update of the hash, and the updates between
# two calls of cacheGC.
CHUNK = 1_000
GC_EVERY = 50
PLAIN = (type(None), bool, int, float, str, bytes)
# A class or a function stands for its name.
CODE = (type, types.FunctionType, types.BuiltinFunctionType, types.MethodType)

kind, path, out_path, *rest = sys.argv[1:]
if kind == "filestorage":
    from ZODB.FileStorage import FileStorage

    storage = FileStorage(path, read_only=True)
elif kind == "zoe":
    import zoe

    storage = zoe.Storage(path, read_only=True)
else:
    sys.exit(f"catalog_digest: unknown kind {kind}")
db = DB(storage, cache_size=100_000)
connection = db.open()
updates = 0
# The objects that the walk is in, so that a cycle ends.
visiting = set()


def tick():
    global updates
    updates += 1
    if updates % GC_EVERY == 0:
        connection.cacheGC()


def is_tree(value):
    return type(value).__module__.startswith("BTrees.") and hasattr(value, "keys")


def canon(value):
    """A value made of plain types that stands for `value` and its state."""
    if type(value) in PLAIN:
        return value
    if value is MV:
        return "<Missing.Value>"
    if isinstance(value, DateTime):
        return ("DateTime", value.micros(), value.timezone())
    if isinstance(value, (list, tuple)):
        return (type(value).__name__, [canon(item) for item in value])
    if isinstance(value, dict):
        items = sorted(value.items(), key=lambda item: repr(item[0]))
        return ("dict", [(canon(k), canon(v)) for k, v in items])
    if isinstance(value, Length):
        return ("Length", value())
    if isinstance(value, CODE):
        return ("code", _name(value))
    if is_tree(value):
        name = type(value).__name__
        if hasattr(value, "items"):
            return (name, [(canon(k), canon(v)) for k, v in value.items()])
        return (name, list(value.keys()))
    value = aq_base(value)
    if id(value) in visiting:
        return ("cycle", _class_name(value))
    visiting.add(id(value))
    try:
        return (_class_name(value), [(name, canon(v)) for name, v in _state(value)])
    finally:
        visiting.discard(id(value))


def _name(value):
    return f"{value.__module__}.{value.__qualname__}"


def _class_name(value):
    return _name(type(value))


def _state(value):
    """The attributes of an object in the order of their names, without the
    volatile and the persistence ones."""
    if hasattr(value, "_p_activate"):
        value._p_activate()
    state = getattr(value, "__dict__", {})
    return [
        (name, state[name])
        for name in sorted(state)
        if not name.startswith(("_v_", "_p_"))
    ]


def feed(digest, value):
    """Update the hash `digest` with `value` and its state, a large tree
    a chunk of items at a time."""
    if is_tree(value):
        digest.update(type(value).__name__.encode())
        items = value.items() if hasattr(value, "items") else value.keys()
        chunk = []
        for item in items:
            chunk.append(canon(item))
            if len(chunk) == CHUNK:
                digest.update(repr(chunk).encode())
                chunk = []
                tick()
        digest.update(repr(chunk).encode())
    elif type(value) in PLAIN or value is MV or isinstance(value, (DateTime, Length)):
        digest.update(repr(canon(value)).encode())
    elif isinstance(value, (list, tuple, dict, *CODE)):
        digest.update(repr(canon(value)).encode())
    else:
        value = aq_base(value)
        digest.update(_class_name(value).encode())
        if id(value) in visiting:
            digest.update(b"cycle")
            return
        visiting.add(id(value))
        try:
            for name, item in _state(value):
                digest.update(name.encode())
                feed(digest, item)
        finally:
            visiting.discard(id(value))


def hash_of(value):
    digest = hashlib.sha256()
    feed(digest, value)
    return digest.hexdigest()


try:
    site = connection.root()["Application"][SITE_ID]
    catalog = site["portal_catalog"]
    inner = catalog._catalog
    # An index that refers to the catalog, as GopipIndex does, holds its
    # name only: the catalog is not a part of the index.
    visiting.add(id(aq_base(inner)))
    indexes = {}
    for name in sorted(inner.indexes):
        index = aq_base(inner.indexes[name])
        if index.meta_type == "ZCTextIndex":
            entries = index.indexSize()
        else:
            entries = index.numObjects()
        indexes[name] = {
            "kind": index.meta_type,
            "entries": entries,
            "sha": hash_of(index),
        }

    names = list(inner.names)
    digests = [hashlib.sha256() for _ in names]
    values = [0] * len(names)
    chunks = [[] for _ in names]
    rows = 0
    for rid, record in inner.data.items():
        for position, value in enumerate(record):
            if value is not MV:
                values[position] += 1
            chunks[position].append((rid, canon(value)))
        rows += 1
        if rows % CHUNK == 0:
            for digest, chunk in zip(digests, chunks, strict=True):
                digest.update(repr(chunk).encode())
                chunk.clear()
            tick()
    for digest, chunk in zip(digests, chunks, strict=True):
        digest.update(repr(chunk).encode())
    columns = {
        name: {"entries": values[position], "sha": digests[position].hexdigest()}
        for position, name in enumerate(names)
    }
    catalog_digest = {
        "size": len(inner),
        "indexes": indexes,
        "columns": dict(sorted(columns.items())),
        "uids": hash_of(inner.uids),
        "paths": hash_of(inner.paths),
    }
    whole = json.dumps(catalog_digest, sort_keys=True)
    catalog_digest["digest"] = hashlib.sha256(whole.encode()).hexdigest()

    queries = {}
    if rest:
        with open(rest[0]) as file:
            query_set = json.load(file)
        if isinstance(query_set, dict):
            query_set = query_set["queries"]
        for query in query_set:
            results = catalog.unrestrictedSearchResults(**query["query"])
            queries[query["name"]] = results.actual_result_count
            connection.cacheGC()
    catalog_digest["queries"] = queries
finally:
    connection.close()
    db.close()

with open(out_path, "w") as file:
    json.dump(catalog_digest, file, indent=1, sort_keys=True)
size, whole = catalog_digest["size"], catalog_digest["digest"]
print(f"catalog_digest: {size} entries, digest {whole}")
