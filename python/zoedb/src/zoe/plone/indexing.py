"""Queued indexing: a save writes its catalog operations to a durable
namespace of the store, and one indexer applies them (ADR 0047).

The indexing queue of Products.CMFCore collects the catalog operations of
a transaction (index, reindex, unindex) and gives them to each
`IIndexQueueProcessor` utility before the commit. The utility
`portal-catalog` writes the catalog, in the transaction of the save. So
two saves of different objects write the same buckets of the catalog, and
one of them gets a `ConflictError`.

`install()` puts the processor of this module in the place of
`portal-catalog`. When the storage of the catalog has KV namespaces
(`zoe.kv`), the processor writes one key for each operation in the
durable namespace `QUEUE` followed by the oid of the catalog in hex. It
writes no object of the catalog. Else it calls the stock processor.

`Indexer(site)` applies the queue of the catalog of `site`. `run()`
applies batches until the queue is empty. Each batch reads the objects in
a snapshot that holds the saves of its keys, applies one entry for each
path, and deletes its keys, in one transaction. Each path resolves
against the newest state, so the order of the paths does not change the
result, except for the UID index: the removals come first.

When the backend is installed, a thread of each Zope process runs the
indexer of each site while it holds the lease of the site: a key of the
ephemeral namespace `LEASE`. `Indexer.run()` itself takes no lease.

The response of a request that queued operations sets the cookie
`COOKIE`. A later request with the cookie waits at `PubStart` until the
indexer applied those operations, then begins its transaction again.

Only `install()` and the functions that Plone calls import Plone.
"""

import atexit
import collections
import json
import logging
import os
import struct
import threading
import time
import urllib.parse

import transaction
from transaction.interfaces import TransientError
from ZODB.DemoStorage import DemoStorage

import zoe.kv

__all__ = [
    "BATCH_SIZE",
    "COOKIE",
    "FAILED",
    "LEASE",
    "QUEUE",
    "Indexer",
    "install",
    "installed",
    "status",
    "uninstall",
]

logger = logging.getLogger("zoe.plone.indexing")

# The durable namespace of the queue of a catalog: this prefix, then the
# oid of the catalog in 16 hex digits.
QUEUE = "zoe.plone.indexing:"
# The durable namespace of the operations that failed too often.
FAILED = "zoe.plone.indexing.failed:"
# The ephemeral namespace of the leases, one key for each catalog.
LEASE = "zoe.plone.indexing.lease"
# The name of the utility of CMFCore that the processor replaces.
UTILITY = "portal-catalog"
# The cookie of the user who saved.
COOKIE = "zoe.indexing"
# The note of the transaction of each batch.
NOTE = "zoe.plone: indexing queue"
# The security indexes of CMFCore (`CMFCatalogAware._cmf_security_indexes`).
SECURITY = frozenset({"allowedRolesAndUsers"})

# The most keys in a batch.
BATCH_SIZE = 100
# The time between two scans of the queues of the thread, in seconds.
POLL = 0.2
# The time between two searches of the sites of the thread, in seconds.
SITES_EVERY = 60.0
# The TTL of a lease, in seconds.
LEASE_TTL = 30
# The longest wait of a request for its own operations, in seconds.
WAIT = 2.0
# The longest wait of a batch for its storage to see the queue, in seconds.
VIEW_WAIT = 10.0
# The tries of a batch that meets a transient error, and the pauses
# between them, in seconds.
TRIES = 5
PAUSES = (0.1, 0.2, 0.4, 0.8)
# The failures after which an operation leaves the queue.
FAILURES = 5
# The most delays that the process keeps for the status.
DELAYS = 100_000

# The key of an operation: the time in microseconds since the Unix epoch,
# 8 random bytes of the processing of the queue, and the number of the
# operation in that processing. Big-endian, so the byte order of the keys
# is their order in time.
_KEY = struct.Struct(">Q8sI")


def make_key(micros, salt, number):
    """Return the key of an operation."""
    return _KEY.pack(micros, salt, number)


def key_time(key):
    """Return the time in a key, in seconds since the Unix epoch."""
    return _KEY.unpack(key)[0] / 1_000_000


def queue_name(oid):
    """Return the name of the queue of the catalog whose oid is `oid`."""
    return QUEUE + oid.hex()


def failed_name(oid):
    """Return the name of the failed operations of the catalog `oid`."""
    return FAILED + oid.hex()


def encode(op, path, idxs, metadata, failures=0, error=None):
    """Return the value of an operation: JSON in UTF-8."""
    value = {"op": op, "path": path, "idxs": list(idxs), "metadata": bool(metadata)}
    if failures:
        value["failures"] = failures
        value["error"] = error
    return json.dumps(value, sort_keys=True, separators=(",", ":")).encode("utf-8")


def decode(data):
    """Return the operation of a value as a dict."""
    return json.loads(data.decode("utf-8"))


class Entry:
    """The operations of one path in a batch.

    `full` is true when one operation asks for all indexes: an index, an
    unindex, or a reindex with no index. Else `idxs` holds the union of
    the indexes. `security` is true when one operation asks only for the
    security indexes: the indexer then reindexes the subtree of the path
    too.
    """

    __slots__ = ("full", "idxs", "keys", "metadata", "security")

    def __init__(self):
        self.keys = []
        self.full = False
        self.idxs = set()
        self.metadata = False
        self.security = False

    def add(self, op, idxs, metadata):
        idxs = set(idxs)
        if op in ("index", "unindex") or not idxs:
            self.full = True
        else:
            self.idxs |= idxs
            if idxs <= SECURITY:
                self.security = True
        if op in ("index", "unindex") or metadata:
            self.metadata = True


def coalesce(pairs):
    """Return the entries of `pairs`, `(key, value)` in key order, as a
    dict of path to `Entry`, in the order of the first key of each path."""
    entries = {}
    for key, value in pairs:
        operation = decode(value)
        entry = entries.get(operation["path"])
        if entry is None:
            entry = entries[operation["path"]] = Entry()
        entry.keys.append(key)
        entry.add(operation["op"], operation.get("idxs") or (), operation["metadata"])
    return entries


def highest(paths):
    """Return the paths of `paths` that are below no other path of it."""
    tops = []
    for path in sorted(paths):
        if not any(path.startswith(top + "/") for top in tops):
            tops.append(path)
    return tops


# The processor while the backend is installed: the registry, the stock
# utility, whether the registry held the stock utility itself, and the
# processor.
_installed = {}

# The state of the processing of the queue in each thread: the random part
# of the keys and the number of the next operation.
_local = threading.local()

# The storages without namespaces that the processor warned about.
_warned = set()
_warned_lock = threading.Lock()

# The keys that the indexers of this process applied: the time of the
# commit of each batch and the delay of each key.
_delays = collections.deque(maxlen=DELAYS)
_delays_lock = threading.Lock()


def installed():
    """Tell if the backend is installed."""
    return bool(_installed)


def install():
    """Put the processor of this module in the place of the utility
    `portal-catalog` of the global registry. Start no thread. A second
    call does nothing."""
    if _installed:
        return
    from Products.CMFCore.indexing import PortalCatalogProcessor
    from Products.CMFCore.interfaces import IPortalCatalogQueueProcessor
    from zope.component import getGlobalSiteManager
    from zope.interface import classImplements

    classImplements(QueueProcessor, IPortalCatalogQueueProcessor)
    registry = getGlobalSiteManager()
    stock = registry.queryUtility(IPortalCatalogQueueProcessor, name=UTILITY)
    if stock is None:
        stock = PortalCatalogProcessor()
    own = any(
        registration.provided is IPortalCatalogQueueProcessor
        and registration.name == UTILITY
        for registration in registry.registeredUtilities()
    )
    processor = QueueProcessor(stock)
    registry.registerUtility(processor, IPortalCatalogQueueProcessor, UTILITY)
    _installed.update(registry=registry, stock=stock, own=own, processor=processor)


def uninstall():
    """Put the stock utility back. A second call does nothing."""
    if not _installed:
        return
    from Products.CMFCore.interfaces import IPortalCatalogQueueProcessor

    registry = _installed["registry"]
    registry.unregisterUtility(
        _installed["processor"], IPortalCatalogQueueProcessor, UTILITY
    )
    if _installed["own"]:
        registry.registerUtility(
            _installed["stock"], IPortalCatalogQueueProcessor, UTILITY
        )
    _installed.clear()


def _warn(jar):
    """Log one WARN event for each storage without namespaces."""
    storage = jar.db().storage if jar is not None else None
    with _warned_lock:
        if id(storage) in _warned:
            return
        _warned.add(id(storage))
    logger.warning(
        "queued indexing: the storage %s has no KV namespaces, so each save"
        " writes the catalog",
        type(storage).__name__,
    )


class QueueProcessor:
    """The queue processor of `zoe.plone`.

    It writes each operation to the queue of the catalog of the object, in
    the current transaction. When the storage of the catalog has no
    namespaces, it calls the stock processor.
    """

    def __init__(self, stock):
        self._stock = stock

    def begin(self):
        _local.salt = os.urandom(8)
        _local.number = 0
        self._stock.begin()

    def commit(self):
        self._stock.commit()

    def abort(self):
        self._stock.abort()

    def index(self, obj, attributes=None):
        if not self._queue(obj, "index", (), True):
            self._stock.index(obj, attributes)

    def reindex(self, obj, attributes=None, update_metadata=1):
        if not self._queue(obj, "reindex", attributes or (), update_metadata):
            self._stock.reindex(obj, attributes, update_metadata=update_metadata)

    def unindex(self, obj):
        if not self._queue(obj, "unindex", (), True):
            self._stock.unindex(obj)

    def _queue(self, obj, op, idxs, metadata):
        """Write the operation to the queue. Return False when the storage
        of the catalog has no namespaces."""
        from Acquisition import aq_base
        from Products.CMFCore.utils import getToolByName

        catalog = getToolByName(obj, "portal_catalog", None)
        if catalog is None:
            # The stock processor does nothing then.
            return True
        base = aq_base(catalog)
        jar = getattr(base, "_p_jar", None)
        oid = getattr(base, "_p_oid", None)
        if jar is None or oid is None or not zoe.kv._supported(jar):
            _warn(jar)
            return False
        salt = getattr(_local, "salt", None)
        if salt is None:
            self.begin()
            salt = _local.salt
        number = _local.number
        _local.number = number + 1
        key = make_key(int(time.time() * 1_000_000), salt, number)
        path = "/".join(obj.getPhysicalPath())
        value = encode(op, path, [str(name) for name in idxs], metadata)
        zoe.kv.Namespace(jar, "durable", queue_name(oid)).set(key, value)
        _written(jar, catalog, oid, key)
        return True


def _written(jar, catalog, oid, key):
    """After a write to the queue: wake the thread of the process after
    the commit, name the site to the thread, and set the cookie of the
    user."""
    from Acquisition import aq_inner, aq_parent
    from zope.globalrequest import getRequest

    site = aq_parent(aq_inner(catalog))
    path = site.getPhysicalPath()
    txn = jar.transaction_manager.get()
    if getattr(_local, "hooked", None) is not txn:
        _local.hooked = txn
        txn.addAfterCommitHook(_committed, (jar.db(), path))
    request = getRequest()
    response = getattr(request, "response", None)
    if response is not None and hasattr(response, "setCookie"):
        url = site.absolute_url_path() or "/"
        value = f"{oid.hex()}-{key.hex()}-{urllib.parse.quote(url, safe='/')}"
        response.setCookie(COOKIE, value, path=url, http_only=True, same_site="Lax")


def _committed(status, db, path):
    """Wake the thread of the database after a commit that wrote keys."""
    if not status:
        return
    service = _services.get(id(db))
    if service is not None:
        service.wake(path)


class StaleView(TransientError):
    """The storage did not see the commit of a key of the batch in time."""


class _PathFailure(Exception):
    """An error while the indexer applied one path."""

    def __init__(self, path, error):
        super().__init__(path, error)
        self.path = path
        self.error = error


def _nothing(indexer, keys):
    """The default `after_batch`: do nothing."""


class Indexer:
    """Apply the queue of the catalog of `site`.

    `run()` applies batches of at most `batch_size` keys until a scan of
    the queue gives no key. After the commit of each batch it calls
    `after_batch(indexer, keys)`. The indexer works on a connection and a
    transaction manager of its own, under the system user.
    """

    def __init__(self, site, batch_size=BATCH_SIZE):
        from Acquisition import aq_base
        from Products.CMFCore.utils import getToolByName

        if not isinstance(batch_size, int) or batch_size < 1:
            raise ValueError("the batch size is an int of 1 or more")
        self._db = aq_base(site)._p_jar.db()
        self._path = site.getPhysicalPath()
        catalog = aq_base(getToolByName(site, "portal_catalog"))
        self._oid = catalog._p_oid
        self.batch_size = batch_size
        self.after_batch = _nothing
        # The token of the thread that holds the lease of the site, or
        # None. The transaction of each batch then writes the lease again.
        self._lease = None
        # The keys that failed in this run, and the paths of a security
        # subtree whose reindex failed in this run.
        self._skipped = set()
        self._broken = set()
        # The most batches of one run, or None. The thread of the process
        # sets it, so that one busy site does not hold the thread.
        self._max_batches = None

    @property
    def name(self):
        """The name of the queue."""
        return queue_name(self._oid)

    def run(self):
        """Apply batches until the queue is empty. Return the number of
        keys that the batches applied."""
        tm = transaction.TransactionManager()
        connection = self._db.open(transaction_manager=tm)
        context = _Context()
        applied = 0
        try:
            app = context.enter(connection)
            site = app.unrestrictedTraverse(self._path)
            context.site(site)
            batches = 0
            while self._max_batches is None or batches < self._max_batches:
                count = self._apply_batch(app, site, connection, tm)
                if not count:
                    break
                applied += count
                batches += 1
        except BaseException:
            tm.abort()
            raise
        finally:
            tm.abort()
            context.exit()
            connection.close()
        return applied

    def _apply_batch(self, app, site, connection, tm):
        """Apply one batch, again after a transient error. Return the
        number of keys of the batch."""
        for attempt in range(TRIES):
            try:
                return self._batch(app, site, connection, tm)
            except TransientError:
                tm.abort()
                if attempt == TRIES - 1:
                    raise
                time.sleep(PAUSES[min(attempt, len(PAUSES) - 1)])
        return 0

    def _scan(self, connection):
        """Return the pairs of the next batch, without the skipped keys."""
        queue = zoe.kv.Namespace(connection, "durable", self.name)
        pairs = []
        start = None
        while len(pairs) < self.batch_size:
            page = queue.scan(start=start, limit=1000)
            if not page:
                break
            pairs.extend(pair for pair in page if pair[0] not in self._skipped)
            start = page[-1][0] + b"\x00"
        return pairs[: self.batch_size]

    def _wait_for(self, generation):
        """Wait until the storage saw the commit `generation`."""
        storage = self._db.storage
        if isinstance(storage, DemoStorage):
            # The overlays of zoe.kv give generations that are not tids,
            # and the commits are in this process.
            return
        deadline = time.monotonic() + VIEW_WAIT
        while storage.lastTransaction() < generation:
            if time.monotonic() > deadline:
                raise StaleView("the storage did not see the commit of the queue")
            time.sleep(0.01)

    def _begin(self, tm, connection):
        """Begin the transaction of a batch: a new snapshot."""
        tm.begin()

    def _batch(self, app, site, connection, tm):
        pairs = self._scan(connection)
        if not pairs:
            return 0
        queue = zoe.kv.Namespace(connection, "durable", self.name)
        # Each key of the scan committed at or before the generation.
        self._wait_for(queue.generation())
        self._begin(tm, connection)
        began = time.monotonic()
        entries = coalesce(pairs)
        try:
            self._apply(app, site, entries)
        except _PathFailure as failure:
            tm.abort()
            self._fail(connection, tm, entries, failure)
            return len(entries[failure.path].keys) or 1
        keys = [key for entry in entries.values() for key in entry.keys]
        queue = zoe.kv.Namespace(connection, "durable", self.name)
        for key in keys:
            queue.delete(key)
        if self._lease is not None:
            lease = zoe.kv.Namespace(connection, "ephemeral", LEASE)
            lease.set(self._oid, self._lease, ttl=LEASE_TTL)
        txn = tm.get()
        txn.note(NOTE)
        txn.setUser("System Processes", "")
        tm.commit()
        now = time.time()
        delays = [now - key_time(key) for key in keys]
        with _delays_lock:
            _delays.extend((now, delay) for delay in delays)
        logger.debug(
            "queued indexing: %d keys, %d paths, %.3f s, largest delay %.3f s",
            len(keys),
            len(entries),
            time.monotonic() - began,
            max(delays),
        )
        self.after_batch(self, keys)
        return len(keys)

    def _apply(self, app, site, entries):
        """Apply the entries to the catalog: the removals first, then the
        index or the reindex of each path that has an object."""
        from Products.CMFCore.utils import getToolByName

        catalog = getToolByName(site, "portal_catalog")
        self._expand_security(catalog, entries)
        inner = catalog._catalog
        uids = inner.uids
        uid_index = inner.indexes.get("UID")
        removals = []
        updates = []
        for path, entry in entries.items():
            obj = self._guarded(path, _resolve, app, path)
            rid = uids.get(path)
            if obj is None:
                if rid is not None:
                    removals.append(path)
                continue
            uid = _uid(obj)
            if uid is not None and uid_index is not None:
                if rid is not None:
                    held = uid_index.getEntryForObject(rid, None)
                    if held is not None and held != uid:
                        # Another object took the path.
                        removals.append(path)
                        rid = None
                other = uid_index._index.get(uid)
                if other is not None and other != rid:
                    stale = inner.paths.get(other)
                    if stale is not None and stale != path:
                        found = self._guarded(stale, _resolve, app, stale)
                        if found is None or _uid(found) != uid:
                            removals.append(stale)
            updates.append((path, obj, entry))
        done = set()
        for path in removals:
            if path not in done and uids.get(path) is not None:
                self._guarded(path, catalog.uncatalog_object, path)
            done.add(path)
        for path, obj, entry in updates:
            if uids.get(path) is None:
                self._guarded(path, catalog._indexObject, obj)
            else:
                idxs = [] if entry.full else sorted(entry.idxs)
                self._guarded(
                    path,
                    catalog._reindexObject,
                    obj,
                    idxs=idxs,
                    update_metadata=int(entry.metadata),
                )

    def _expand_security(self, catalog, entries):
        """Add to `entries` each path that the catalog holds below a path
        whose operations reindex only the security indexes, with those
        indexes. Search only below the highest such paths."""
        tops = highest(path for path, entry in entries.items() if entry.security)
        for top in tops:
            idxs = set(entries[top].idxs & SECURITY) or set(SECURITY)
            for brain in catalog.unrestrictedSearchResults(path=top):
                path = brain.getPath()
                if path == top or path in self._broken:
                    continue
                entry = entries.get(path)
                if entry is None:
                    entry = entries[path] = Entry()
                    entry.idxs = set(idxs)
                elif not entry.full:
                    entry.idxs |= idxs

    def _guarded(self, path, call, *args, **kwargs):
        """Return `call(*args, **kwargs)`, which works on `path`. A
        transient error stays as it is; another error becomes a
        `_PathFailure` of the path."""
        try:
            return call(*args, **kwargs)
        except TransientError:
            raise
        except Exception as error:
            raise _PathFailure(path, error) from error

    def _fail(self, connection, tm, entries, failure):
        """Count a failure for each key of the path that failed, in a
        transaction of its own. A key with `FAILURES` failures goes to the
        failed namespace. The run skips the keys of the path."""
        entry = entries[failure.path]
        text = f"{type(failure.error).__name__}: {failure.error}"[:500]
        if not entry.keys:
            # A path that only the security subtree named: skip it.
            self._broken.add(failure.path)
            logger.error(
                "queued indexing: the security reindex of %s failed: %s",
                failure.path,
                text,
            )
            return
        self._skipped.update(entry.keys)
        queue = zoe.kv.Namespace(connection, "durable", self.name)
        failed = zoe.kv.Namespace(connection, "durable", failed_name(self._oid))
        tm.begin()
        try:
            for key in entry.keys:
                data = queue.get(key)
                if data is None:
                    continue
                operation = decode(data)
                failures = operation.get("failures", 0) + 1
                value = encode(
                    operation["op"],
                    operation["path"],
                    operation.get("idxs") or (),
                    operation["metadata"],
                    failures,
                    text,
                )
                if failures >= FAILURES:
                    failed.set(key, value)
                    queue.delete(key)
                    logger.error(
                        "queued indexing: %s failed %d times and left the queue: %s",
                        failure.path,
                        failures,
                        text,
                    )
                else:
                    queue.set(key, value)
                    logger.warning(
                        "queued indexing: %s failed (%d): %s",
                        failure.path,
                        failures,
                        text,
                    )
            tm.get().note(NOTE)
            tm.commit()
        except TransientError:
            tm.abort()


def _resolve(app, path):
    """Return the object at `path` that the catalog can index, or None.
    The object is found without acquisition: its physical path is the
    path."""
    from Acquisition import aq_base

    try:
        obj = app.unrestrictedTraverse(path)
    except TransientError:
        raise
    except Exception:
        return None
    try:
        if "/".join(obj.getPhysicalPath()) != path:
            return None
    except AttributeError:
        return None
    if getattr(aq_base(obj), "reindexObject", None) is None:
        return None
    return obj


def _uid(obj):
    """Return the UID of `obj`, or None."""
    from plone.uuid.interfaces import IUUID

    return IUUID(obj, None)


class _Context:
    """The site, the request and the security manager of a run of the
    indexer. `exit()` gives back those of the caller."""

    def enter(self, connection):
        from AccessControl.SecurityManagement import (
            getSecurityManager,
            newSecurityManager,
        )
        from AccessControl.SpecialUsers import system
        from Testing.makerequest import makerequest
        from zope.component.hooks import getSite
        from zope.globalrequest import getRequest, setRequest

        self._site = getSite()
        self._request = getRequest()
        self._security = getSecurityManager()
        app = makerequest(connection.root()["Application"])
        setRequest(app.REQUEST)
        newSecurityManager(None, system)
        return app

    def site(self, site):
        from zope.component.hooks import setSite

        setSite(site)

    def exit(self):
        from AccessControl.SecurityManagement import setSecurityManager
        from zope.component.hooks import setSite
        from zope.globalrequest import clearRequest, setRequest

        setSite(self._site)
        if self._request is None:
            clearRequest()
        else:
            setRequest(self._request)
        setSecurityManager(self._security)


class _Jar:
    """A stand-in for a connection, for the reads of `zoe.kv` on a
    database."""

    def __init__(self, db):
        self._db = db

    def db(self):
        return self._db


def _storage(db):
    """Return the namespaces of the storage of `db`, or None."""
    try:
        return zoe.kv._storage_of(_Jar(db))
    except Exception:
        return None


def _site_storage(site):
    """Return the namespaces of the storage of `site`, or None."""
    from Acquisition import aq_base

    jar = getattr(aq_base(site), "_p_jar", None)
    if jar is None:
        return None
    return _storage(jar.db())


def status(site, since=None):
    """Return the status of the queue of `site` as a dict: the keys in the
    queue, the age of the oldest key, the failed keys, whether a thread
    holds the lease, and the delays of the keys that the indexers of this
    process applied. With `since`, a time in seconds since the Unix epoch,
    the delays are those of the batches that committed at or after it."""
    from Acquisition import aq_base
    from Products.CMFCore.utils import getToolByName

    result = {"installed": installed(), "namespaces": False}
    storage = _site_storage(site)
    if storage is None:
        return result
    oid = aq_base(getToolByName(site, "portal_catalog"))._p_oid
    result["namespaces"] = True
    pending, oldest = _count(storage, queue_name(oid))
    failed, _ = _count(storage, failed_name(oid))
    result["pending"] = pending
    result["oldest_age"] = None if oldest is None else max(0.0, time.time() - oldest)
    result["failed"] = failed
    result["lease"] = storage.kv_get("ephemeral", LEASE, oid) is not None
    with _delays_lock:
        delays = sorted(
            delay for committed, delay in _delays if since is None or committed >= since
        )
    result["delays"] = _summary(delays)
    return result


def _count(storage, name):
    """Return the number of keys of a durable namespace and the time in
    its first key, or None."""
    count = 0
    first = None
    start = None
    while True:
        page = storage.kv_scan("durable", name, start, None, 1000)
        if not page:
            break
        if first is None:
            first = key_time(page[0][0])
        count += len(page)
        start = page[-1][0] + b"\x00"
    return count, first


def _summary(delays):
    """Return the count, p50, p95 and maximum of sorted `delays`."""
    if not delays:
        return {"count": 0, "p50": None, "p95": None, "max": None}

    def at(share):
        return delays[min(len(delays) - 1, int(share * len(delays)))]

    return {"count": len(delays), "p50": at(0.5), "p95": at(0.95), "max": delays[-1]}


class StatusView:
    """`@@zoe-indexing`: the status of the queue of the site, as JSON. The
    form value `since`, a time in seconds since the Unix epoch, limits the
    delays to the batches that committed at or after it."""

    def __init__(self, context, request):
        self.context = context
        self.request = request

    def __call__(self):
        since = self.request.form.get("since")
        try:
            since = None if since in (None, "") else float(since)
        except (TypeError, ValueError):
            since = None
        self.request.response.setHeader("Content-Type", "application/json")
        self.request.response.setHeader("Cache-Control", "no-store")
        return json.dumps(status(self.context, since=since))


def _parse_cookie(value):
    """Return `(oid, key, path)` of the value of the cookie, or None."""
    try:
        oid, key, path = value.split("-", 2)
        oid, key = bytes.fromhex(oid), bytes.fromhex(key)
    except (AttributeError, ValueError):
        return None
    if len(oid) != 8 or len(key) != _KEY.size:
        return None
    return oid, key, urllib.parse.unquote(path) or "/"


def request_started(event):
    """Wait, at the start of a request with the cookie of the user, until
    the indexer applied the operations of the save of the user. Then
    begin the transaction of the request again, so that its snapshot holds
    them. The wait lasts at most `WAIT`."""
    if not _installed:
        return
    request = event.request
    value = getattr(request, "cookies", {}).get(COOKIE)
    if not value:
        return
    parsed = _parse_cookie(value)
    if parsed is None:
        request.response.expireCookie(COOKIE, path="/")
        return
    oid, key, path = parsed
    import Zope2

    db = getattr(Zope2, "DB", None)
    storage = _storage(db) if db is not None else None
    if storage is None:
        request.response.expireCookie(COOKIE, path=path)
        return
    name = queue_name(oid)
    stop = key + b"\x00"
    deadline = time.monotonic() + WAIT
    late = False
    while storage.kv_scan("durable", name, None, stop, 1):
        if time.monotonic() > deadline:
            late = True
            break
        time.sleep(0.02)
    if not late and not isinstance(db.storage, DemoStorage):
        generation = storage.kv_generation(name)
        while db.storage.lastTransaction() < generation:
            if time.monotonic() > deadline:
                late = True
                break
            time.sleep(0.005)
    if late:
        logger.warning(
            "queued indexing: a request waited %.1f s for the indexer and goes on",
            WAIT,
        )
    request.response.expireCookie(COOKIE, path=path)
    # A new snapshot, and the mark of the writes-on-GET report for it.
    transaction.manager.begin()
    from zoe.plone import report

    report.request_started(event)


# The threads of this process, by the id of their database.
_services = {}
_services_lock = threading.Lock()


def database_opened(event):
    """Start the thread of the database, when the backend is installed."""
    if not _installed:
        return
    db = event.database
    with _services_lock:
        if id(db) in _services:
            return
        service = _services[id(db)] = _Service(db)
    service.start()


class _Service(threading.Thread):
    """The thread of a database: it runs the indexer of each site of the
    database while it holds the lease of the site."""

    def __init__(self, db):
        super().__init__(name="zoe.plone.indexing", daemon=True)
        self._db = db
        self._token = os.urandom(16)
        self._event = threading.Event()
        self._sites = set()
        self._held = set()
        self._stopping = False
        atexit.register(self.stop)

    def wake(self, path=None):
        if path is not None:
            self._sites.add(tuple(path))
        self._event.set()

    def stop(self):
        """Stop the thread and give back its leases."""
        self._stopping = True
        self._event.set()
        if not self._held:
            return
        try:
            tm = transaction.TransactionManager()
            connection = self._db.open(transaction_manager=tm)
            try:
                lease = zoe.kv.Namespace(connection, "ephemeral", LEASE)
                for oid in list(self._held):
                    if lease.get(oid) == self._token:
                        lease.delete(oid)
                tm.commit()
            finally:
                tm.abort()
                connection.close()
        except Exception:
            logger.debug("queued indexing: no lease given back", exc_info=True)
        self._held.clear()

    def run(self):
        found = None
        while not self._stopping:
            self._event.wait(POLL)
            self._event.clear()
            if self._stopping:
                break
            try:
                if found is None or time.monotonic() - found > SITES_EVERY:
                    self._find_sites()
                    found = time.monotonic()
                for path in list(self._sites):
                    self._serve(path)
            except Exception:
                logger.exception("queued indexing: the thread failed")
                time.sleep(1)

    def _find_sites(self):
        """Add the sites at the top of the Zope root."""
        from Acquisition import aq_base
        from Products.CMFCore.interfaces import ISiteRoot

        tm = transaction.TransactionManager()
        connection = self._db.open(transaction_manager=tm)
        try:
            app = connection.root()["Application"]
            for obj in app.objectValues():
                base = aq_base(obj)
                if ISiteRoot.providedBy(base) and hasattr(base, "portal_catalog"):
                    self._sites.add(tuple(obj.getPhysicalPath()))
        finally:
            tm.abort()
            connection.close()

    def _serve(self, path):
        """Run the indexer of the site at `path` when its queue holds keys
        and the thread holds the lease."""
        from Acquisition import aq_base
        from Products.CMFCore.utils import getToolByName

        tm = transaction.TransactionManager()
        connection = self._db.open(transaction_manager=tm)
        try:
            try:
                site = connection.root()["Application"].unrestrictedTraverse(path)
                catalog = aq_base(getToolByName(site, "portal_catalog"))
            except (KeyError, AttributeError):
                self._sites.discard(path)
                return
            if not zoe.kv._supported(connection):
                self._sites.discard(path)
                return
            oid = catalog._p_oid
            queue = zoe.kv.Namespace(connection, "durable", queue_name(oid))
            if not queue.scan(limit=1):
                return
            if not self._take(connection, tm, oid):
                return
            indexer = Indexer(site)
            indexer._lease = self._token
            indexer._max_batches = 50
            indexer.run()
            if queue.scan(limit=1):
                # The site has more keys: serve it again at once.
                self._event.set()
        finally:
            tm.abort()
            connection.close()

    def _take(self, connection, tm, oid):
        """Take the lease of the catalog `oid`, or keep it. Return True
        when the thread holds it."""
        lease = zoe.kv.Namespace(connection, "ephemeral", LEASE)
        current = lease.get(oid)
        if current == self._token:
            return True
        if current is not None:
            self._held.discard(oid)
            return False
        tm.begin()
        lease.set(oid, self._token, ttl=LEASE_TTL)
        tm.commit()
        # The last writer wins: read the lease again.
        held = lease.get(oid) == self._token
        if held:
            self._held.add(oid)
        return held
