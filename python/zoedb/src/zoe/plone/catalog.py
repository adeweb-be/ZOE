"""The catalog backend of `zoe.plone` (ADR 0059): `Catalog.search` answered
by the catalog mirror of the store.

`install()` hooks `Products.ZCatalog.Catalog.Catalog.search`, as
collective.solr does. The hook reads `catalog_supported()` of the storage
of the catalog with `getattr`, as `zoe.kv` reads `kv_supported()`. It
builds the plan of the query with ZCatalog's own `IndexQuery` and the
conversion of each index, sends it with the start of the snapshot of the
Connection to `catalog_query` of the storage, and gives a `LazyMap` of
brains in the order of the reply. For a refusal, or for a query that it
cannot type, the hook runs the stock search, whole.

The brains. The plan names the columns of the `schema` of the catalog, in
the order of their positions (`cols`), and the number of rids that get
their rows (`rn`): a page of `_CELLS` cells, or `b_size` when it is
smaller. The reply gives the row and the path of each of these rids; a
row becomes Python values only when its brain is made. The hook makes the
brain of such a rid as `Catalog.instantiate` makes it from `data`, with the
cells of the row, and its `getPath()` gives the path of the reply: the
Connection loads no bucket of `data` and no bucket of `paths` for it. A
rid whose row is `None` (a cell of a type that the mirror does not model),
a rid past `rn`, and each brain made after the Connection registered an
object, stored a savepoint record or moved to another snapshot, gets the
brain of the stock `Catalog.__getitem__`, which reads `data` as the
Connection sees it then. A schema that does not give each position from 0
once sends no columns.

This version types the clauses of FieldIndex, KeywordIndex, DateIndex and
UUIDIndex (`"k": "v"`), of BooleanIndex (`"k": "b"`, the truth of the first
key), of DateRangeIndex (`"k": "d"`, the date in minutes of
`_convertDateTime`) and of ExtendedPathIndex (`"k": "p"`, each path with
its level, the options of `ExtendedPathIndex.query_index` and the
physical path of the index), and a sort on FieldIndexes, DateIndexes and
UUIDIndexes. A sort on an ExtendedPathIndex takes the Python path
(`sort`). The hook decides the reasons `unsupported`, `index`, `option`,
`and_range`, `sort` and `transaction`; the store decides the others. A search in a
transaction whose connection holds a registered object or a savepoint
record takes the Python path (`transaction`): no overlay carries the
changes of the transaction yet.

The modes: `on`, the default, and `compare`, which also runs the stock
search, logs each difference at ERROR with the indexes and the options of
the query and no value, counts it, and returns the stock result.
`ZOE_PLONE_CATALOG_MODE` gives the mode of a process.

The registration. The backend writes one registration key for the catalog
of each Plone site to the durable namespace `zoe.catalog` of the store that
holds the catalog: the 8 bytes of the oid of the `Catalog` object, with an
empty value. It finds the sites as queued indexing does: at the top of the
Zope root in the root database, and at or just below each mount point in a
mounted database. At each `IDatabaseOpenedWithRoot` it writes the missing
keys and removes the others. A site created later writes its key in the
transaction that creates it, and a site deleted removes its key. A
read-only storage gets no key. On a `DemoStorage` over a ZOE storage, the
keys go to the overlay of `zoe.kv`. `uninstall()` gives the stock search
back in the process and removes, in a transaction of its own, the keys of
the databases that the process has open.

Only `install()` and the functions that Plone calls import Plone.
"""

import collections
import logging
import os
import threading

import transaction

import zoe.kv

__all__ = ["MODES", "NAMESPACE", "install", "installed", "uninstall"]

logger = logging.getLogger("zoe.plone.catalog")

#: The durable namespace of the registration keys.
NAMESPACE = "zoe.catalog"

#: The modes of the backend.
MODES = ("on", "compare")

# The classes of the indexes that the mirror serves, with the kind of their
# clauses, and the classes of the sort indexes that it serves.
_PLUGIN_INDEXES = "Products.PluginIndexes"
_CLAUSES = {
    f"{_PLUGIN_INDEXES}.FieldIndex.FieldIndex.FieldIndex": "v",
    f"{_PLUGIN_INDEXES}.KeywordIndex.KeywordIndex.KeywordIndex": "v",
    f"{_PLUGIN_INDEXES}.DateIndex.DateIndex.DateIndex": "v",
    f"{_PLUGIN_INDEXES}.UUIDIndex.UUIDIndex.UUIDIndex": "v",
    f"{_PLUGIN_INDEXES}.BooleanIndex.BooleanIndex.BooleanIndex": "b",
    f"{_PLUGIN_INDEXES}.DateRangeIndex.DateRangeIndex.DateRangeIndex": "d",
    "Products.ExtendedPathIndex.ExtendedPathIndex.ExtendedPathIndex": "p",
}
_SORTS = {
    f"{_PLUGIN_INDEXES}.FieldIndex.FieldIndex.FieldIndex",
    f"{_PLUGIN_INDEXES}.DateIndex.DateIndex.DateIndex",
    f"{_PLUGIN_INDEXES}.UUIDIndex.UUIDIndex.UUIDIndex",
}

# The stock `Catalog.search` and the mode, while the backend is installed.
_installed = {}

# The root database of the process, from `IDatabaseOpenedWithRoot`.
_root = {}

# The counts of this process: `("queries", path)`, `("fallbacks", reason)`
# for the reasons that the hook decides, and `("mismatches",)`.
_counts = collections.Counter()
_counts_lock = threading.Lock()

# A function that the tests give to change a native result before the
# comparison of the mode `compare`. None outside the tests.
_plant = None

# The largest int of a plan: msgpack carries 64 bits.
_INT = 2**63

# The cells of the rows that a reply gives for a query without `b_size`.
_CELLS = 50_000


def installed():
    """Tell if the backend is installed."""
    return bool(_installed)


def mode():
    """Return the mode of the process: `ZOE_PLONE_CATALOG_MODE`, or `on`.
    Another mode raises `ValueError`."""
    value = os.environ.get("ZOE_PLONE_CATALOG_MODE", "").strip().lower() or "on"
    if value not in MODES:
        raise ValueError(
            f"ZOE_PLONE_CATALOG_MODE={value!r}: the modes are {', '.join(MODES)}"
        )
    return value


def install():
    """Hook `Catalog.search`, in the mode of `ZOE_PLONE_CATALOG_MODE`, and
    write the registration keys of the databases that the process has open.
    A second call does nothing."""
    if _installed:
        return
    from Products.ZCatalog.Catalog import Catalog

    current = mode()
    _installed.update(stock=Catalog.search, mode=current)
    Catalog.search = _search
    db = _root.get("db")
    if db is not None:
        register(db)


def uninstall():
    """Give the stock search back, then remove, in a transaction of its own,
    the registration keys of each database that the process has open. A
    second call does nothing."""
    if not _installed:
        return
    from Products.ZCatalog.Catalog import Catalog

    Catalog.search = _installed["stock"]
    _installed.clear()
    db = _root.get("db")
    if db is None:
        return
    for each in _databases(db):
        _write_keys(each, set())


def database_opened(event):
    """Write the missing registration keys of the databases of the event and
    remove the others, when the backend is installed."""
    _root["db"] = event.database
    if _installed:
        register(event.database)


def register(db):
    """Write the registration key of the catalog of each site of the root
    database `db` and of its mounted databases, in the database of the
    catalog, and remove the keys of the catalogs that are gone."""
    found = {id(each): set() for each in _databases(db)}
    try:
        for catalog in _catalogs(db):
            jar = catalog._p_jar
            found.setdefault(id(jar.db()), set()).add(catalog._p_oid)
    except Exception:
        logger.exception("catalog mirror: the search of the sites failed")
        return
    for each in _databases(db):
        _write_keys(each, found.get(id(each), set()))


def _databases(db):
    """The root database `db` and its mounted databases with KV namespaces
    and a writable storage."""
    out = []
    for each in list(db.databases.values()):
        storage = each.storage
        if storage.isReadOnly():
            continue
        try:
            zoe.kv._storage_of(each)
        except Exception:
            continue
        out.append(each)
    return out


def _write_keys(db, oids):
    """Make the keys of the namespace of `db` the oids `oids`, in a
    transaction of its own."""
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        manager.begin()
        keys = zoe.kv.Namespace(connection, "durable", NAMESPACE)
        present = {bytes(k) for k, _ in zoe.kv.scan_all(keys.scan)}
        if present == oids:
            return
        for oid in oids - present:
            keys.set(oid, b"")
        for oid in present - oids:
            keys.delete(oid)
        manager.commit()
    except Exception:
        logger.exception(
            "catalog mirror: the registration keys of %s were not written",
            db.database_name,
        )
    finally:
        manager.abort()
        connection.close()


def _catalogs(db):
    """Yield the `Catalog` objects of the sites that the root database `db`
    holds at the top of its Zope root, and of the sites at or just below
    each mount point."""
    from Acquisition import aq_base
    from Products.CMFCore.interfaces import ISiteRoot

    from zoe.plone.indexing import _mount_paths

    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        app = connection.root()["Application"]
        candidates = list(app.objectValues())
        for path, _ in _mount_paths():
            try:
                mounted = app.unrestrictedTraverse(path)
            except (KeyError, AttributeError):
                continue
            candidates.append(mounted)
            values = getattr(mounted, "objectValues", None)
            if values is not None:
                candidates.extend(values())
        seen = set()
        for obj in candidates:
            base = aq_base(obj)
            if not ISiteRoot.providedBy(base):
                continue
            tool = getattr(base, "portal_catalog", None)
            catalog = (
                aq_base(getattr(aq_base(tool), "_catalog", None))
                if tool is not None
                else None
            )
            if catalog is None or catalog._p_oid is None or catalog._p_oid in seen:
                continue
            seen.add(catalog._p_oid)
            yield catalog
    finally:
        manager.abort()
        connection.close()


def site_added(site, event):
    """Write the key of the catalog of a new site, in the transaction that
    creates it, before its commit."""
    if not _installed or event.oldParent is not None:
        return
    transaction.get().addBeforeCommitHook(_register_new_site, (site, event.newParent))


def _register_new_site(site, parent):
    from Acquisition import aq_base

    tool = getattr(aq_base(site), "portal_catalog", None)
    if tool is None:
        return
    # An attribute of an acquirer comes in a wrapper: the store takes the
    # object itself.
    catalog = aq_base(getattr(aq_base(tool), "_catalog", None))
    if catalog is None:
        return
    jar = (
        catalog._p_jar
        or aq_base(site)._p_jar
        or getattr(aq_base(parent), "_p_jar", None)
    )
    if jar is None or not zoe.kv._supported(jar) or jar.db().storage.isReadOnly():
        return
    if catalog._p_oid is None:
        jar.add(catalog)
    zoe.kv.Namespace(jar, "durable", NAMESPACE).set(catalog._p_oid, b"")


def site_removed(site, event):
    """Remove the key of the catalog of a deleted site."""
    if not _installed or event.newParent is not None:
        return
    from Acquisition import aq_base

    tool = getattr(aq_base(site), "portal_catalog", None)
    catalog = (
        aq_base(getattr(aq_base(tool), "_catalog", None)) if tool is not None else None
    )
    if catalog is None or catalog._p_oid is None or catalog._p_jar is None:
        return
    jar = catalog._p_jar
    if not zoe.kv._supported(jar) or jar.db().storage.isReadOnly():
        return
    zoe.kv.Namespace(jar, "durable", NAMESPACE).delete(catalog._p_oid)


def _count(*key):
    with _counts_lock:
        _counts[key] += 1


def _counts_now():
    """The counts of this process, with the names of ADR 0059, section 11:
    the queries by path, the fallbacks that the hook decided, with
    `side="client"`, and the mismatches of the mode `compare`."""
    with _counts_lock:
        counts = dict(_counts)
    out = {"queries": {}, "fallbacks": {}, "mismatches": counts.get(("mismatches",), 0)}
    for key, n in counts.items():
        if key[0] == "queries":
            out["queries"][key[1]] = n
        elif key[0] == "fallbacks":
            out["fallbacks"][(key[1], "client")] = n
    return out


def _search(self, query, sort_index=None, reverse=False, limit=None, merge=True):
    """`Catalog.search` with the mirror."""
    stock = _installed.get("stock")
    if stock is None:
        from Products.ZCatalog.Catalog import Catalog

        return Catalog.search(self, query, sort_index, reverse, limit, merge)
    native, reason, plan = _native(self, query, sort_index, reverse, limit, merge)
    if native is None:
        _count("queries", "python")
        if reason is not None:
            _count("fallbacks", reason)
    else:
        _count("queries", "native")
    if _installed.get("mode") == "compare":
        result = stock(self, query, sort_index, reverse, limit, merge)
        if native is not None:
            if _plant is not None:
                native = _plant(native)
            _compare(plan, native, result)
        return result
    if native is not None:
        return native
    return stock(self, query, sort_index, reverse, limit, merge)


def _native(catalog, query, sort_index, reverse, limit, merge):
    """Answer the query from the mirror. Return the result, or None with
    the reason that the hook decided (None for a reason of the store), and
    the plan."""
    jar = getattr(catalog, "_p_jar", None)
    oid = getattr(catalog, "_p_oid", None)
    if jar is None or oid is None:
        return None, "unsupported", None
    storage = jar.db().storage
    supported = getattr(storage, "catalog_supported", None)
    if supported is None or not supported():
        return None, "unsupported", None
    start = getattr(getattr(jar, "_storage", None), "_start", None)
    if start is None:
        return None, "unsupported", None
    if jar._registered_objects or _savepoint_records(jar):
        return None, "transaction", None
    plan, reason = plan_of(catalog, query, sort_index, reverse, limit, merge)
    if plan is None:
        return None, reason, None
    reply = storage.catalog_query(oid, start, plan)
    if "f" in reply:
        return None, None, plan
    return _result(catalog, reply, jar, start), None, plan


def _savepoint_records(jar):
    """Tell if the savepoint storage of the connection holds a record."""
    savepoint = getattr(jar, "_savepoint_storage", None)
    if savepoint is None:
        return False
    try:
        return len(savepoint) > 0
    except TypeError:
        return True


def _result(catalog, reply, jar, start):
    """The lazy result of a reply, as ZCatalog gives it."""
    from ZTUtils.Lazy import LazyCat, LazyMap

    if not reply["r"] and not reply["a"]:
        return LazyCat([])
    return LazyMap(
        _brains(catalog, reply, jar, start),
        list(reply["r"]),
        reply["n"],
        actual_result_count=reply["a"],
    )


def _brains(catalog, reply, jar, start):
    """The function that makes the brain of a rid of `reply`: from its row
    and its path, as `Catalog.instantiate` makes it from `data`, or with the
    stock `Catalog.__getitem__`.

    The `LazyMap` makes a brain when the caller reads it. The stock brain
    then reads `data` and `paths` as the Connection sees them at that
    moment. So a brain comes from the row only while the Connection holds
    no registered object and no savepoint record, and still has the
    snapshot `start` of the query. A row is decoded only when its brain is
    made."""
    from Acquisition import aq_parent

    rows = reply.get("rows")
    paths = reply.get("paths")
    if not rows or not paths:
        return catalog.__getitem__
    rids = reply["r"]
    places = {}

    def place(rid):
        # The place of each rid with a row, made at the first brain.
        if not places:
            places.update((r, k) for k, r in enumerate(rids[: len(rows)]))
        return places.get(rid)

    def moved():
        return (
            jar._registered_objects
            or _savepoint_records(jar)
            or getattr(getattr(jar, "_storage", None), "_start", None) != start
        )

    def brain(rid):
        k = place(rid)
        if k is None or paths[k] is None or moved():
            return catalog.__getitem__(rid)
        row = rows[k]
        klass = catalog._v_result_class
        # The fast path of `instantiate`: one value for each column, then
        # the rid and the two scores.
        if row is None or len(row) + 3 != len(klass.__record_schema__):
            return catalog.__getitem__(rid)
        path = paths[k]
        catalog._maintain_zodb_cache()
        record = klass(row + (rid, 1, 1))
        # A brain takes no attribute outside its schema, but keeps a dict:
        # the path of the reply shadows `getPath`, which reads `paths`.
        record.__dict__["getPath"] = lambda: path
        return record.__of__(aq_parent(catalog))

    return brain


def _columns(catalog):
    """The columns of the schema of `catalog` in the order of their
    positions, or None when the schema does not map strs to ints that give
    each position from 0 once."""
    schema = getattr(catalog, "schema", None)
    if not isinstance(schema, dict):
        return None
    if not all(type(k) is str and type(v) is int for k, v in schema.items()):
        return None
    if sorted(schema.values()) != list(range(len(schema))):
        return None
    return sorted(schema, key=schema.__getitem__)


def _wire(value):
    """Tell if the value goes in a plan: None, a bool, an int of 64 bits, a
    float, a str, or a tuple of them."""
    if value is None or isinstance(value, (bool, float, str)):
        return True
    if isinstance(value, int):
        return -_INT <= value < _INT
    if isinstance(value, tuple):
        return all(_wire(item) for item in value)
    return False


def plan_of(catalog, query, sort_index=None, reverse=False, limit=None, merge=True):
    """Return the plan of a search of `catalog`, in the form of ADR 0059,
    section 8, and None; or None and the reason of the Python path.

    The arguments are those of `Catalog.search`. The plan takes each index
    whose names are in the query after `make_query`, typed with ZCatalog's
    `IndexQuery` and the conversion of the index, and the sort as
    `searchResults` gives it.
    """
    from Products.ZCatalog.query import IndexQuery

    if not merge:
        return None, "sort"
    try:
        made = catalog.make_query(query)
    except Exception:
        return None, "option"
    clauses = []
    for name in list(catalog.indexes):
        index = catalog.getIndex(name)
        if not any(n in made for n in catalog._get_index_query_names(index)):
            continue
        kind = _CLAUSES.get(_class_of(index))
        if kind is None:
            return None, "index"
        try:
            rec = IndexQuery(
                made, index.id, index.query_options, index.operators, index.useOperator
            )
        except Exception:
            return None, "option"
        if rec.keys is None:
            continue
        if kind == "b":
            # `BooleanIndex.query_index` reads the truth of the first key.
            keys = list(rec.keys)
            clauses.append({"k": "b", "i": name, "q": bool(keys[0]) if keys else None})
            continue
        if kind == "p":
            try:
                clause, reason = _path_clause(index, name, rec)
            except Exception:
                return None, "option"
            if clause is None:
                return None, reason
            clauses.append(clause)
            continue
        if kind == "d":
            if not rec.keys:
                return None, "option"
            try:
                term = index._convertDateTime(rec.keys[0])
            except Exception:
                return None, "option"
            if not (term is None or (isinstance(term, int) and _wire(term))):
                return None, "option"
            clauses.append({"k": "d", "i": name, "t": term})
            continue
        try:
            clause, reason = _value_clause(index, name, rec)
        except Exception:
            # A key that the conversion of the index refuses: the stock
            # search raises as it does.
            return None, "option"
        if clause is None:
            return None, reason
        clauses.append(clause)
    sort = []
    if sort_index is not None:
        indexes = sort_index if isinstance(sort_index, list) else [sort_index]
        orders = reverse if isinstance(reverse, list) else [bool(reverse)]
        for i, each in enumerate(indexes):
            if _class_of(each) not in _SORTS:
                return None, "sort"
            if len(orders) == 1:
                order = orders[0]
            else:
                order = orders[i] if i < len(orders) else None
            sort.append([each.getId(), None if order is None else bool(order)])
        if limit == 0:
            return None, "sort"
    if limit is not None and (isinstance(limit, bool) or not isinstance(limit, int)):
        return None, "option"
    try:
        b_start = int(made.get("b_start", 0))
        b_size = made.get("b_size", None)
        b_size = int(b_size) if b_size is not None else None
    except (TypeError, ValueError):
        return None, "option"
    cols = _columns(catalog)
    if cols is None:
        rn = 0
    else:
        page = max(1, _CELLS // max(1, len(cols)))
        rn = page if b_size is None else max(0, min(b_size, page))
    return {
        "c": clauses,
        "s": sort,
        "l": limit,
        "bs": b_start,
        "bz": b_size,
        "cols": cols,
        "rn": rn,
    }, None


def _class_of(index):
    """The class of an index, as `module.name`."""
    from Acquisition import aq_base

    cls = type(aq_base(index))
    return f"{cls.__module__}.{cls.__qualname__}"


def _value_clause(index, name, rec):
    """Return the clause of keys of `rec` for an index of the `UnIndex`
    family, and None; or None and the reason of the Python path."""
    if rec.get("usage", None):
        return None, "option"
    conjunction = rec.operator == "and"
    clause = {"k": "v", "i": name, "q": [], "and": conjunction, "r": None}
    range_ = rec.get("range", None)
    if range_:
        if conjunction:
            return None, "and_range"
        if not rec.keys:
            return None, "option"
        keys = [index._convert(k) for k in rec.keys]
        try:
            low = min(keys) if "min" in range_ else None
            high = max(keys) if "max" in range_ else None
        except TypeError:
            return None, "option"
        # `if hi:` of ZCatalog: a false bound is no bound.
        clause["r"] = [low, high if high else None]
    else:
        keys = [index._convert(k) for k in rec.keys if k is not None]
        if conjunction and not keys:
            return None, "option"
        clause["q"] = keys
    nots = list(rec.get("not", None) or [])
    # `UnIndex.query_index` converts the keys of `not` for a query of `not`
    # alone. With keys or a range, it looks them up as they are.
    if not rec.keys:
        nots = [index._convert(k) for k in nots]
    clause["not"] = nots
    if not all(_wire(v) for v in clause["q"] + nots + (clause["r"] or [])):
        return None, "option"
    return clause, None


# The options of a path clause, with the defaults of
# `ExtendedPathIndex.query_index`.
_PATH_DEFAULTS = {"lv": 0, "d": -1, "nt": False, "ns": 0}


def _int(value):
    """The value as an int of a plan, for a level or a depth that
    ExtendedPathIndex compares with ints; None for another value."""
    if isinstance(value, int) and _wire(value):
        return int(value)
    return None


def _path_clause(index, name, rec):
    """Return the clause of `rec` for an ExtendedPathIndex, with the options
    as `ExtendedPathIndex.query_index` reads them, and None; or None and the
    reason of the Python path."""
    level = _int(rec.get("level", 0))
    depth = _int(getattr(rec, "depth", -1))
    navtree = bool(rec.get("navtree", 0))
    start = rec.get("navtree_start", 0)
    # Without navtree, `search` does not read navtree_start.
    start = _int(start) if navtree else (_int(start) or 0)
    if level is None or depth is None or start is None:
        return None, "option"
    paths = []
    for key in rec.keys:
        if isinstance(key, str):
            paths.append([key, None])
            continue
        # `search` reads a key that is not a str as (path, level).
        path, key_level = key[0], _int(int(key[1]))
        if not isinstance(path, str) or key_level is None:
            return None, "option"
        paths.append([path, key_level])
    return {
        "k": "p",
        "i": name,
        "q": paths,
        "lv": level,
        "and": rec.operator == "and",
        "d": depth,
        "nt": navtree,
        "ns": start,
        "ip": [p for p in index.getPhysicalPath() if p],
    }, None


def _compare(plan, native, stock):
    """Log and count a difference between the native and the stock result
    of a search, with the indexes and the options of the plan and no
    value."""
    differences = []
    if list(native._seq if hasattr(native, "_seq") else []) != _rids(stock):
        differences.append("rids")
    if len(native) != len(stock):
        differences.append("len")
    actual = getattr(stock, "actual_result_count", len(stock))
    if getattr(native, "actual_result_count", len(native)) != actual:
        differences.append("actual_result_count")
    if not differences:
        return
    _count("mismatches")
    clauses = []
    for clause in plan["c"]:
        options = [
            name
            for name in ("q", "r", "not", "t")
            if clause.get(name) not in (None, [], False)
        ]
        # The options of a path clause that differ from their defaults.
        options += [
            name
            for name, default in _PATH_DEFAULTS.items()
            if name in clause and clause[name] != default
        ]
        clauses.append(f"{clause['i']}({','.join(options)})")
    sort = [f"{name}:{'desc' if r else 'asc'}" for name, r in plan["s"]]
    logger.error(
        "catalog mirror: a mismatch of %s for the clauses %s, the sort %s,"
        " sort_limit %s, b_start %s, b_size %s",
        ", ".join(differences),
        " ".join(clauses) or "none",
        " ".join(sort) or "none",
        "set" if plan["l"] is not None else "none",
        "set" if plan["bs"] else "none",
        "set" if plan["bz"] is not None else "none",
    )


def _rids(result):
    """The rids of a lazy result of ZCatalog, without making brains."""
    seq = getattr(result, "_seq", None)
    if seq is not None and getattr(result, "_func", None) is not None:
        return list(seq)
    return [brain.getRID() for brain in result]
