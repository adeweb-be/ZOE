"""The catalog backend of `zoe.plone` (ADR 0059): `Catalog.search` answered
by the catalog mirror of the store.

`install()` hooks `Products.ZCatalog.Catalog.Catalog.search`, as
collective.solr does. The hook reads `catalog_supported()` of the storage
of the catalog with `getattr`, as `zoe.kv` reads `kv_supported()`. A
`zoe.Storage` answers from the mirror of the process; a
`zoe.zeo.ClientStorage` with the native client inside answers from the
mirror of `zoe serve`, when its server accepted `catalog_query`. A stock
`ZEO.ClientStorage`, the asyncio mode of `zoe.zeo.ClientStorage` and a
test storage have no `catalog_supported`, so their searches take the
Python path (`unsupported`). It
builds the plan of the query with ZCatalog's own `IndexQuery` and the
conversion of each index, sends it with the start of the snapshot of the
Connection to `catalog_query` of the storage, and gives a `LazyMap` of
brains in the order of the reply. For a refusal, or for a query that it
cannot type, the hook runs the stock search, whole.

The transaction overlay. A search in a transaction that changed records of
the database of the catalog sends these records with the query (ADR 0059,
section 8): the pickles of the objects that the Connection registered, the
records of its savepoint storage that no registered object replaces, and
the new objects that they reach. A new object that no savepoint stored has
no oid, so the hook names it with a provisional oid: the high bit set, then
a counter of the overlay. The hook pickles copies of the states with
ZODB's `ObjectWriter` and changes no object: a new object, and a weak
reference to a new object, get no oid and no jar; the stock writer sees
only the objects that it does not change. It sends every record, with no
filter, and pickles the overlay again for each search. A plan and an
overlay that pass the cap of a call, 50,000 msgpack values or 16 MiB, take
the Python path (`too_large`): the hook counts the records before any
pickle, and stops as soon as the overlay passes the cap. An object that the
hook cannot name without a change to it (a weak reference to an object of
another database, a persistent class without an oid), and an overlay that
does not pickle, take the Python path (`overlay`); the second goes to the
log once for each class. A `ConflictError` and a `POSKeyError` of a pickle
go on to the caller.

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
Connection sees it then. A search with an overlay sends no columns: a
change of a registered object leaves no trace, so each of its brains comes
from `Catalog.__getitem__`. A schema that does not give each position from
0 once sends no columns.

This version types the clauses of FieldIndex, KeywordIndex, DateIndex,
UUIDIndex and the `ZoeSecurityIndex` of `zoe.plone.security` (`"k": "v"`),
of BooleanIndex (`"k": "b"`, the truth of the first key), of
DateRangeIndex (`"k": "d"`, the date in minutes of `_convertDateTime`) and
of ExtendedPathIndex (`"k": "p"`, each path with
its level, the options of `ExtendedPathIndex.query_index` and the
physical path of the index), and a sort on FieldIndexes, DateIndexes and
UUIDIndexes. A sort on an ExtendedPathIndex takes the Python path
(`sort`). The hook decides the reasons `unsupported`, `index`, `option`,
`and_range`, `sort`, `too_large` and the `overlay` that it cannot pickle;
the store decides the others. An error of `catalog_query` that the hook
catches (`Unsupported`, `StorageError`) goes to the log once for each
class, at WARNING, and the search takes the Python path (`unsupported`).

The modes: `on`, the default, and `compare`, which also runs the stock
search, logs each difference at ERROR with the indexes and the options of
the query and no value, counts it, and returns the stock result.
`ZOE_PLONE_CATALOG_MODE` gives the mode of a process. ZCatalog keeps the
interim sets of an index in a cache of the request, and a hit of that cache
can give another answer than a miss. So a difference runs the stock search
again with an empty cache for the catalog: when that answer equals the
native answer, the difference comes from the cache of ZCatalog. It logs at
WARNING and counts apart, and it is no mismatch. The comparison also
takes the brains of the first rids that the reply gives rows for: each
brain of the mirror must hold the values of the stock brain in each column
and give its path. A test can replace a method of `Catalog` with a mock, so
the stock search gives no reference: a search while a method of `Catalog`
that a search calls (`_SEARCH_METHODS`) differs from the method at
`install()` is not compared, and counts as unchecked (`patched`); so is a
search of an instance of a subclass that overrides one of these methods
(`subclass`).

The throwaway mirror (ADR 0059, section 9). A test storage, a `DemoStorage`
whose chain of bases ends at a ZOE storage, has no mirror. In the mode
`compare`, a search there builds a throwaway mirror with the private
function `zoe._zoe._catalog_throwaway`: the build reads the test storage
at the snapshot of the Connection, then the query reads the transaction
overlay. The mirror answers the one search and is dropped. In the mode
`on`, the search takes the Python path (`unsupported`).

The counts. `_counts_now()` gives the queries by path, the reasons of the
Python path with their side (`client` for the reasons of the hook,
`server` for the reasons of a reply), the comparisons of the mode
`compare` and the throwaway mirrors among them, the mismatches, and the
differences of the cache of the request. With `ZOE_PLONE_CATALOG_COUNTS`,
the process adds its counts and the indexes and the options of each
mismatch, with no value, as one JSON line to the end of that file when it
exits.

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

import atexit
import collections
import functools
import json
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
    # The security index of `zoe.plone` (ADR 0061): a clause of tokens.
    "zoe.plone.security.ZoeSecurityIndex": "v",
}
_SORTS = {
    f"{_PLUGIN_INDEXES}.FieldIndex.FieldIndex.FieldIndex",
    f"{_PLUGIN_INDEXES}.DateIndex.DateIndex.DateIndex",
    f"{_PLUGIN_INDEXES}.UUIDIndex.UUIDIndex.UUIDIndex",
}

# The stock `Catalog.search` and the mode, while the backend is installed.
_installed = {}
# The stock `Catalog.search` and the methods of `_SEARCH_METHODS` at the
# last `install()`, kept after `uninstall()`: a search that runs while
# `uninstall()` runs reads them.
_originals = {}
# The lock of `install()` and `uninstall()`.
_install_lock = threading.Lock()

# The root database of the process, from `IDatabaseOpenedWithRoot`.
_root = {}

# The counts of this process: `("queries", path)`, `("fallbacks", reason)`
# for the reasons that the hook decides, `("refusals", reason)` for the
# reasons of a reply, `("compared",)`, `("throwaways",)`, `("mismatches",)`,
# `("cache_differences",)`, `("unchecked", why)`, `("stock_errors",)` for
# a native answer whose stock search raised, `("overlay_bound",)` for
# an overlay that does not fit `catalog-memory`, and `("refused", why)` for
# the text of each refusal of a throwaway build.
_counts = collections.Counter()
_counts_lock = threading.Lock()

# The indexes and the options of each mismatch of the mode `compare`, as
# the log gives them, at most `_KEPT`.
_mismatched = []
_KEPT = 1000

# The file of `ZOE_PLONE_CATALOG_COUNTS`, once its write is registered.
_counts_file = {}

# The last search of the thread: `outcome` is "native", or the reason of the
# Python path with its side, or None; `mismatch` is the shape of its
# mismatch, or None. The replay reads it.
_last = threading.local()

# A function that the tests give to change a native result before the
# comparison of the mode `compare`. None outside the tests.
_plant = None

# The largest int of a plan: msgpack carries 64 bits.
_INT = 2**63

# The cells of the rows that a reply gives for a query without `b_size`.
_CELLS = 50_000

# The cap of a call (ADR 0059, section 8): the msgpack values and the bytes
# of the plan and of the overlay.
_CAP_VALUES = 50_000
_CAP_BYTES = 16 * 1024 * 1024

# The high bit of a provisional oid.
_PROVISIONAL = 1 << 63

# The classes of the errors of `catalog_query` that went to the log.
_logged = set()


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
    from Products.ZCatalog.Catalog import Catalog

    with _install_lock:
        if _installed:
            return
        current = mode()
        stock = {"stock": Catalog.search, "code": _code(Catalog)}
        _originals.update(stock)
        _installed.update(stock, mode=current)
        Catalog.search = _search
        path = os.environ.get("ZOE_PLONE_CATALOG_COUNTS", "").strip()
        if path and not _counts_file:
            _counts_file["path"] = path
            atexit.register(_write_counts, path)
    db = _root.get("db")
    if db is not None:
        register(db)


# The methods of `Catalog` that the stock search and the hook call.
_SEARCH_METHODS = (
    "__getitem__",
    "__len__",
    "_get_index_query_names",
    "_limit_sequence",
    "_maintain_zodb_cache",
    "_search_index",
    "_sort_iterate_index",
    "_sort_iterate_resultset",
    "_sort_limit_arguments",
    "_sort_nbest",
    "_sort_nbest_reverse",
    "_sorted_search_indexes",
    "getCatalogPlan",
    "getIndex",
    "instantiate",
    "make_query",
    "sortResults",
)


def _code(cls):
    """The methods of `_SEARCH_METHODS` that the class `cls` defines, by
    name."""
    found = vars(cls)
    return {name: found[name] for name in _SEARCH_METHODS if name in found}


def _unchecked(catalog):
    """Why the stock search of `catalog` gives no reference, or None: a
    method that a search calls differs from the method at `install()`
    (`patched`), or the class of `catalog` overrides one (`subclass`)."""
    from Acquisition import aq_base
    from Products.ZCatalog.Catalog import Catalog

    if _code(Catalog) != _originals["code"]:
        return "patched"
    mro = type(aq_base(catalog)).__mro__
    below = mro[: mro.index(Catalog)] if Catalog in mro else mro
    if any(_code(cls) for cls in below):
        return "subclass"
    return None


def uninstall():
    """Give the stock search back, then remove, in a transaction of its own,
    the registration keys of each database that the process has open. A
    second call does nothing. A search that runs meanwhile ends with the
    stock search."""
    from Products.ZCatalog.Catalog import Catalog

    with _install_lock:
        if not _installed:
            return
        Catalog.search = _installed["stock"]
        # `_originals` keeps the stock search for the searches in flight.
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
    the queries by path, the fallbacks by reason and side (`client` for the
    reasons that the hook decided, `server` for the reasons of a reply),
    the comparisons of the mode `compare`, the throwaway mirrors, the
    mismatches and the differences of the cache of the request."""
    with _counts_lock:
        counts = dict(_counts)
    out = {
        "queries": {},
        "fallbacks": {},
        "compared": counts.get(("compared",), 0),
        "throwaways": counts.get(("throwaways",), 0),
        "mismatches": counts.get(("mismatches",), 0),
        "cache_differences": counts.get(("cache_differences",), 0),
        "unchecked": {},
        "overlay_bound": counts.get(("overlay_bound",), 0),
        "stock_errors": counts.get(("stock_errors",), 0),
        "refused": {},
    }
    for key, n in counts.items():
        if key[0] == "queries":
            out["queries"][key[1]] = n
        elif key[0] == "unchecked":
            out["unchecked"][key[1]] = n
        elif key[0] == "refused":
            out["refused"][key[1]] = n
        elif key[0] == "fallbacks":
            out["fallbacks"][(key[1], "client")] = n
        elif key[0] == "refusals":
            out["fallbacks"][(key[1], "server")] = n
    return out


def _write_counts(path):
    """Add the counts of this process and the indexes and the options of
    each mismatch, with no value, to the end of the file `path`, as one
    JSON line."""
    counts = _counts_now()
    with _counts_lock:
        mismatched = list(_mismatched)
    line = {
        "pid": os.getpid(),
        "queries": counts["queries"],
        "fallbacks": [
            [reason, side, n]
            for (reason, side), n in sorted(counts["fallbacks"].items())
        ],
        "compared": counts["compared"],
        "throwaways": counts["throwaways"],
        "mismatches": counts["mismatches"],
        "cache_differences": counts["cache_differences"],
        "unchecked": counts["unchecked"],
        "overlay_bound": counts["overlay_bound"],
        "stock_errors": counts["stock_errors"],
        "refused": counts["refused"],
        "mismatched": mismatched,
    }
    try:
        with open(path, "a") as out:
            out.write(json.dumps(line, sort_keys=True) + "\n")
    except OSError:
        logger.exception("catalog mirror: the counts were not written to %s", path)


def _search(self, query, sort_index=None, reverse=False, limit=None, merge=True):
    """`Catalog.search` with the mirror."""
    stock = _installed.get("stock")
    if stock is None:
        from Products.ZCatalog.Catalog import Catalog

        return Catalog.search(self, query, sort_index, reverse, limit, merge)
    _last.outcome = None
    _last.mismatch = None
    native, reason, plan = _native(self, query, sort_index, reverse, limit, merge)
    if native is None:
        _count("queries", "python")
        if reason is not None:
            _count("fallbacks", reason)
            _last.outcome = (reason, "client")
    else:
        _count("queries", "native")
        _last.outcome = "native"
    if _installed.get("mode") == "compare":
        try:
            result = stock(self, query, sort_index, reverse, limit, merge)
        except Exception:
            # The stock search raises for this query: there is nothing to
            # compare the native answer with.
            if native is not None:
                _count("stock_errors")
            raise
        why = _unchecked(self) if native is not None else None
        if why is not None:
            _count("unchecked", why)
        elif native is not None:
            _count("compared")
            if _plant is not None:
                native = _plant(native)
            _compare(
                plan,
                native,
                result,
                lambda: _uncached(
                    self, lambda: stock(self, query, sort_index, reverse, limit, merge)
                ),
            )
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
    throwaway = False
    if supported is None or not supported():
        if _installed.get("mode") != "compare" or not _test_storage(storage):
            return None, "unsupported", None
        throwaway = True
    start = _start_of(jar)
    if start is None:
        return None, "unsupported", None
    plan, reason = plan_of(catalog, query, sort_index, reverse, limit, merge)
    if plan is None:
        return None, reason, None
    from ZODB.POSException import ConflictError, POSKeyError, StorageError, Unsupported

    overlay = None
    if jar._registered_objects or _savepoint_records(jar):
        # Each brain of a result over an overlay comes from
        # `Catalog.__getitem__`.
        cols, rn = plan["cols"], plan["rn"]
        plan["cols"], plan["rn"] = None, 0
        try:
            overlay = _overlay(jar)
        except _TooLarge:
            return None, "too_large", plan
        except _Unsure:
            return None, "overlay", plan
        except (ConflictError, POSKeyError):
            raise
        except Exception as e:
            _log_once(e, "the transaction overlay does not pickle")
            return None, "overlay", plan
        if not overlay:
            overlay = None
            plan["cols"], plan["rn"] = cols, rn
    if _too_large(plan, overlay):
        return None, "too_large", plan

    if throwaway:
        _count("throwaways")
        reply = _throwaway(storage, oid, start, plan, overlay)
    else:
        try:
            reply = storage.catalog_query(oid, start, plan, overlay)
        except (Unsupported, StorageError) as e:
            # The server stopped serving the extension, as an older server
            # after a failover, or the connection is lost. The stock search
            # answers. A plan of another form raises `ValueError`, a fault
            # of the hook, which goes on.
            _log_once(e, "catalog_query failed")
            return None, "unsupported", plan
    if "f" in reply:
        _count("refusals", reply["f"])
        _last.outcome = (reply["f"], "server")
        why = str(reply.get("why", ""))
        if reply["f"] == "overlay" and why.startswith("the overlay passes"):
            _count("overlay_bound")
        if throwaway and reply["f"] == "refused":
            # The text names oids and records, never a value of a query.
            _count("refused", why[:200])
        return None, None, plan
    return _result(catalog, reply, jar, start, overlay is None), None, plan


def _test_storage(storage):
    """Tell if `storage` is a test storage: a `DemoStorage` whose chain of
    bases ends at a storage with KV namespaces, a ZOE storage."""
    from ZODB.DemoStorage import DemoStorage

    if not isinstance(storage, DemoStorage):
        return False
    base = storage
    while isinstance(base, DemoStorage):
        base = base.base
    return zoe.kv._has_namespaces(base)


def _throwaway(storage, oid, start, plan, overlay):
    """Answer the plan from a throwaway mirror of the catalog `oid` of the
    test storage `storage`, as of the snapshot `start`, with the overlay."""
    from ZODB.POSException import POSKeyError

    from zoe import _zoe

    def load(oid):
        try:
            found = storage.loadBefore(oid, start)
        except POSKeyError:
            return None
        return None if found is None else found[0]

    return _zoe._catalog_throwaway(load, oid, start, plan, overlay)


def _log_once(error, what):
    """Log an error that the hook caught, with `what` it was doing, once for
    each class and each `what` in the process."""
    cls = type(error)
    name = f"{cls.__module__}.{cls.__qualname__}"
    with _counts_lock:
        if (what, name) in _logged:
            return
        _logged.add((what, name))
    logger.warning(
        "catalog mirror: %s with %s: %s; the search takes the Python path",
        what,
        name,
        error,
    )


class _TooLarge(Exception):
    """The overlay passes the cap of a call."""


class _Unsure(Exception):
    """The overlay holds an object that the hook cannot name without a
    change to it."""


def _overlay(jar):
    """The transaction overlay of the Connection `jar`: a list of `(oid,
    pickle)` pairs of bytes. It holds the pickles of the objects that the
    Connection registered and that changed, the records of its savepoint
    storage that no registered object replaces, and the new objects that
    they reach, with provisional oids. It changes no object.

    The count of the records comes before any pickle, and the pickles stop
    as soon as the overlay alone passes the cap of a call: then it raises
    `_TooLarge`. So a search does at most the work of one cap. An object
    that the hook cannot name without a change to it raises `_Unsure`."""
    changed = {}
    for obj in list(jar._registered_objects):
        oid = obj._p_oid
        # As `Connection._commit`: an object of another jar fails the commit,
        # and an object that is no longer changed stores nothing.
        if obj._p_jar is not jar or oid in changed:
            continue
        if oid not in jar._added and not obj._p_changed:
            continue
        changed[oid] = obj
    savepoint = getattr(jar, "_savepoint_storage", None)
    kept = [oid for oid in savepoint.index if oid not in changed] if savepoint else []
    # The array of the overlay, then three values for each record: its
    # array, its oid and its pickle.
    values = _CAP_VALUES - 1 - 3 * (len(changed) + len(kept))
    size = _CAP_BYTES - 5
    if values < 0 or size < 0:
        raise _TooLarge()
    records = []

    def take(oid, data):
        nonlocal size
        size -= 16 + len(data)
        if size < 0:
            raise _TooLarge()
        records.append((oid, data))

    writer = _overlay_writer()(jar)
    for oid, obj in changed.items():
        take(bytes(oid), writer.serialize(obj))
        while writer.pending:
            values -= 3
            if values < 0:
                raise _TooLarge()
            new, obj = writer.pending.pop()
            take(new, writer.serialize(obj))
    for oid in kept:
        data, _ = savepoint.load(oid)
        take(bytes(oid), bytes(data))
    return records


@functools.cache
def _overlay_writer():
    """The class of the writer of an overlay, made at its first use, so the
    import of the module imports no ZODB."""
    from persistent import Persistent
    from persistent.wref import WeakRef
    from ZODB.serialize import ObjectWriter, binary
    from ZODB.utils import p64

    class OverlayWriter(ObjectWriter):
        """An `ObjectWriter` that names a new object with a provisional oid,
        and gives it no oid and no jar."""

        def __init__(self, jar):
            super().__init__()
            self._jar = jar
            self.pending = []
            self._new = {}

        def provisional(self, obj):
            """The provisional oid of the new object `obj`."""
            found = self._new.get(id(obj))
            if found is not None:
                return found[0]
            oid = binary(p64(_PROVISIONAL | (len(self._new) + 1)))
            # The entry keeps the object alive, so no other object takes
            # its id.
            self._new[id(obj)] = (oid, obj)
            self.pending.append((bytes(oid), obj))
            return oid

        def persistent_id(self, obj):
            # The stock `persistent_id` gives an oid and a jar to an object
            # without an oid, and an oid to a weak reference without one.
            # So it sees only the objects that it does not change.
            if isinstance(obj, WeakRef):
                if obj.oid is not None:
                    return super().persistent_id(obj)
                target = obj()
                if target is None:
                    raise _Unsure()
                if getattr(target, "_p_oid", None) is None:
                    return ["w", (self.provisional(target),)]
                if target._p_jar is self._jar:
                    return ["w", (binary(target._p_oid),)]
                raise _Unsure()
            if isinstance(obj, type):
                if getattr(obj, "_p_oid", 0) is None:
                    raise _Unsure()
                return super().persistent_id(obj)
            if isinstance(obj, Persistent) and obj._p_oid is None:
                oid = self.provisional(obj)
                if hasattr(type(obj), "__getnewargs__"):
                    return oid
                return oid, type(obj)
            return super().persistent_id(obj)

    return OverlayWriter


def _size(value):
    """The msgpack values of `value` and a bound of its bytes."""
    if value is None or isinstance(value, bool):
        return 1, 1
    if isinstance(value, (int, float)):
        return 1, 9
    if isinstance(value, str):
        return 1, 5 + len(value.encode("utf-8", "surrogatepass"))
    if isinstance(value, bytes):
        return 1, 5 + len(value)
    if isinstance(value, dict):
        values, size = 1, 5
        for k, v in value.items():
            for item in (k, v):
                n, b = _size(item)
                values, size = values + n, size + b
        return values, size
    values, size = 1, 5
    for item in value:
        n, b = _size(item)
        values, size = values + n, size + b
    return values, size


def _too_large(plan, overlay):
    """Tell if the plan and the overlay pass the cap of a call."""
    values, size = _size(plan)
    if overlay:
        values += 1 + 3 * len(overlay)
        size += 5 + sum(16 + len(pickle) for _, pickle in overlay)
    return values > _CAP_VALUES or size > _CAP_BYTES


def _start_of(jar):
    """The start of the snapshot of the Connection `jar`, or None. After a
    savepoint, `_storage` is the savepoint storage, so the start comes from
    the storage under it."""
    # The MVCC adapter takes the `__len__` of its storage, so an empty
    # storage gives a false adapter: the test is on None.
    storage = getattr(jar, "_normal_storage", None)
    if storage is None:
        storage = getattr(jar, "_storage", None)
    return getattr(storage, "_start", None)


def _savepoint_records(jar):
    """Tell if the savepoint storage of the connection holds a record."""
    savepoint = getattr(jar, "_savepoint_storage", None)
    if savepoint is None:
        return False
    try:
        return len(savepoint) > 0
    except TypeError:
        return True


def _result(catalog, reply, jar, start, rows=True):
    """The lazy result of a reply, as ZCatalog gives it. Without `rows`,
    each brain comes from `Catalog.__getitem__`."""
    from ZTUtils.Lazy import LazyCat, LazyMap

    if not reply["r"] and not reply["a"]:
        return LazyCat([])
    return LazyMap(
        _brains(catalog, reply, jar, start) if rows else catalog.__getitem__,
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
            or _start_of(jar) != start
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
    # Keys that are all None: ZCatalog looks none up and gives no row,
    # whatever `not` holds. With `not`, the plan would read as a query of
    # `not` alone, which gives rows.
    if rec.keys and not clause["q"] and not range_ and nots:
        return None, "option"
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


def _differences(native, stock, rows=0):
    """The names of the parts of the answer where the native and the stock
    result of a search differ: the rids, `len()`, `actual_result_count`,
    and the brains of the first `rows` rids."""
    differences = []
    if list(native._seq if hasattr(native, "_seq") else []) != _rids(stock):
        differences.append("rids")
    if len(native) != len(stock):
        differences.append("len")
    actual = getattr(stock, "actual_result_count", len(stock))
    if getattr(native, "actual_result_count", len(native)) != actual:
        differences.append("actual_result_count")
    if not differences and rows and not _same_brains(native, stock, rows):
        differences.append("rows")
    return differences


def _same_brains(native, stock, rows):
    """Tell if the first `rows` brains of the native result hold the values
    of the stock brains in each column, and give their paths."""
    seq = getattr(native, "_seq", None) or []
    func = getattr(native, "_func", None)
    if func is None:
        return True
    stock_func = getattr(stock, "_func", None)
    stock_seq = getattr(stock, "_seq", None)
    for k in range(min(rows, len(seq))):
        try:
            mine = func(seq[k])
            theirs = (
                stock_func(stock_seq[k])
                if stock_func is not None and stock_seq is not None
                else stock[k]
            )
            if tuple(mine) != tuple(theirs) or mine.getPath() != theirs.getPath():
                return False
        except Exception:
            return False
    return True


def _uncached(catalog, run):
    """Return `run()` with an empty cache of the request for `catalog`, and
    keep the cache of the request as it was. Without a cache of the
    request, return None: the stock search had no cache."""
    from Acquisition import aq_base, aq_get
    from Products.PluginIndexes.cache import RequestCache

    request = aq_get(catalog, "REQUEST", None)
    container = request.get("__catalog_cache__") if hasattr(request, "get") else None
    if container is None:
        return None
    key = aq_base(catalog)
    kept = container.get(key)
    container[key] = RequestCache()
    try:
        return run()
    finally:
        if kept is None:
            container.pop(key, None)
        else:
            container[key] = kept


def _compare(plan, native, stock, uncached=None):
    """Log and count a difference between the native and the stock result
    of a search, with the indexes and the options of the plan and no
    value. `uncached()` gives the stock result with an empty cache of the
    request, or None when the search had no such cache: a difference that
    it does not show comes from the cache of ZCatalog."""
    rows = plan["rn"] if plan.get("cols") else 0
    differences = _differences(native, stock, rows)
    if not differences:
        return
    again = uncached() if uncached is not None else None
    cache = again is not None and not _differences(native, again, rows)
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
    shape = (
        f"{', '.join(differences)} for the clauses {' '.join(clauses) or 'none'},"
        f" the sort {' '.join(sort) or 'none'},"
        f" sort_limit {'set' if plan['l'] is not None else 'none'},"
        f" b_start {'set' if plan['bs'] else 'none'},"
        f" b_size {'set' if plan['bz'] is not None else 'none'}"
    )
    if cache:
        _count("cache_differences")
        logger.warning(
            "catalog mirror: the cache of the request of ZCatalog gives a"
            " difference of %s; the search without the cache equals the mirror",
            shape,
        )
        return
    _count("mismatches")
    _last.mismatch = shape
    with _counts_lock:
        if len(_mismatched) < _KEPT:
            _mismatched.append(shape)
    logger.error("catalog mirror: a mismatch of %s", shape)


def _rids(result):
    """The rids of a lazy result of ZCatalog, without making brains."""
    seq = getattr(result, "_seq", None)
    if seq is not None and getattr(result, "_func", None) is not None:
        return list(seq)
    return [brain.getRID() for brain in result]
