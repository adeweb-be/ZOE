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
durable namespace `QUEUE` followed by the oid of the catalog in hex, in
the store of the catalog. It writes no object of the catalog. Else it
calls the stock processor.

`Indexer(site)` applies the queue of the catalog of `site`. `run()`
applies batches until the queue is empty. Each batch reads the objects in
a snapshot that holds the saves of its keys, applies one entry for each
path, and deletes its keys, in one transaction. Each path resolves
against the newest state, so the order of the paths does not change the
result, except for the UID index: the removals come first.

When the backend is installed, each Zope process has a thread for each of
its databases with KV namespaces: the root database and each mounted
database. The thread of a database runs the indexer of each site whose
catalog is in that database, while it holds the lease of the site: a key
of the ephemeral namespace `LEASE` in that database. `Indexer.run()`
itself takes no lease.

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
# The operations of the queue.
OPERATIONS = frozenset({"index", "reindex", "unindex"})

# The most keys in a batch.
BATCH_SIZE = 100
# The most batches of one run of the thread, so that one busy site does
# not hold the thread.
THREAD_BATCHES = 50
# The time between two scans of the queues of the thread, in seconds.
POLL = 0.2
# The time between two searches of the sites of the thread, in seconds.
SITES_EVERY = 60.0
# The longest pause of the thread before it serves a site again after a
# failure, in seconds. The pause doubles from `POLL` at each failure.
BACKOFF = 30.0
# The TTL of a lease, in seconds.
LEASE_TTL = 30
# The longest wait of a request for its own operations, in seconds.
WAIT = 2.0
# How far in the future the key of a cookie can be, in seconds. A request
# ignores a cookie whose key is later.
COOKIE_SKEW = 60.0
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


def parse(data):
    """Return the operation of a value as a dict. A value that is not an
    operation raises `ValueError`."""
    try:
        operation = decode(data)
    except (AttributeError, ValueError) as error:
        raise ValueError(f"the value is not JSON: {error}") from error
    if not isinstance(operation, dict):
        raise ValueError("the value is not a JSON object")
    if operation.get("op") not in OPERATIONS:
        raise ValueError(f"the operation {operation.get('op')!r} is not known")
    path = operation.get("path")
    if not isinstance(path, str) or not path.startswith("/"):
        raise ValueError(f"the path {path!r} is not a path")
    idxs = operation.get("idxs")
    if not isinstance(idxs, list) or not all(isinstance(name, str) for name in idxs):
        raise ValueError("the indexes are not a list of str")
    if not isinstance(operation.get("metadata"), bool):
        raise ValueError("the metadata flag is not a bool")
    return operation


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
# of the keys, the number of the next operation, and the transaction that
# wakes the threads after its commit, with the sites of each database.
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
    """Log one WARN event for each storage without namespaces, and one for
    the catalogs that are in no database yet."""
    storage = jar.db().storage if jar is not None else None
    with _warned_lock:
        if id(storage) in _warned:
            return
        _warned.add(id(storage))
    if storage is None:
        logger.warning(
            "queued indexing: a catalog is in no database yet, as in the"
            " transaction that adds its site, so its saves write the catalog"
        )
    else:
        logger.warning(
            "queued indexing: the storage %s has no KV namespaces, so each save"
            " writes the catalog",
            type(storage).__name__,
        )


class QueueProcessor:
    """The queue processor of `zoe.plone`.

    It writes each operation to the queue of the catalog of the object, in
    the store of the catalog and in the current transaction. When that
    storage has no namespaces, it calls the stock processor.
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
    """After a write to the queue: name the site to the thread of the
    database of the catalog, wake that thread after the commit, and set the
    cookie of the user."""
    from Acquisition import aq_inner, aq_parent
    from zope.globalrequest import getRequest

    site = aq_parent(aq_inner(catalog))
    db = jar.db()
    txn = jar.transaction_manager.get()
    hooked = getattr(_local, "hooked", None)
    if hooked is None or hooked[0] is not txn:
        # The databases of the commit, with the sites of each.
        hooked = _local.hooked = (txn, {})
        txn.addAfterCommitHook(_committed, (hooked[1],))
    hooked[1].setdefault(id(db), (db, set()))[1].add(tuple(site.getPhysicalPath()))
    request = getRequest()
    response = getattr(request, "response", None)
    if response is not None and hasattr(response, "setCookie"):
        url = site.absolute_url_path() or "/"
        name = db.database_name.encode("utf-8").hex()
        value = f"{oid.hex()}-{key.hex()}-{name}-{urllib.parse.quote(url, safe='/')}"
        # A cookie that the response already expires, as a request with the
        # cookie does at its start, must not keep that expiry.
        cookies = getattr(response, "cookies", None)
        if isinstance(cookies, dict):
            cookies.pop(COOKIE, None)
        response.setCookie(COOKIE, value, path=url, http_only=True, same_site="Lax")


def _committed(status, databases):
    """Wake the thread of each database that the commit wrote keys to, for
    each site of that database."""
    if not status:
        return
    for db, paths in databases.values():
        service = _service_for(db)
        if service is not None:
            for path in paths:
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


def _text(error):
    """Return the text of an error for the failed namespace and the log."""
    return f"{type(error).__name__}: {error}"[:500]


class Indexer:
    """Apply the queue of the catalog of `site`.

    `run()` applies batches of at most `batch_size` keys until a scan of
    the queue gives no key. After the commit of each batch it calls
    `after_batch(indexer, keys)`. The indexer works on a connection and a
    transaction manager of its own, under the system user. It opens the
    connection on the root database of `site` and finds the site by its
    path, through a mount point when the site is in a mounted database.
    The queue, the keys and the lease are in the store of the catalog.
    """

    def __init__(self, site, batch_size=BATCH_SIZE):
        from Acquisition import aq_base

        if not isinstance(batch_size, int) or batch_size < 1:
            raise ValueError("the batch size is an int of 1 or more")
        self._db = aq_base(site.getPhysicalRoot())._p_jar.db()
        self._path = site.getPhysicalPath()
        # The catalog of the site itself, not the catalog of the local site
        # of the caller.
        catalog = aq_base(aq_base(site).portal_catalog)
        self._oid = catalog._p_oid
        self._catalog_db = catalog._p_jar.db()
        self.batch_size = batch_size
        self.after_batch = _nothing
        # The token of the thread that holds the lease of the site, or
        # None. The transaction of each batch then writes the lease again.
        self._lease = None
        # The keys that failed in this run, and the paths without keys
        # whose application failed in this run.
        self._skipped = set()
        self._broken = set()
        # The most batches of one run, or None.
        self._max_batches = None

    @property
    def name(self):
        """The name of the queue."""
        return queue_name(self._oid)

    def run(self):
        """Apply batches until the queue is empty. Return the number of
        keys that the batches handled: the keys that they applied, the
        values that they moved to the failed namespace, and the keys of
        each path that failed, or 1 for a failed path without keys."""
        from Acquisition import aq_base

        tm = transaction.TransactionManager()
        connection = self._db.open(transaction_manager=tm)
        context = _Context()
        applied = 0
        try:
            app = context.enter(connection)
            site = app.unrestrictedTraverse(self._path)
            context.site(site)
            # The connection of the catalog: the connection of the root
            # database, or its connection to the mounted database.
            jar = aq_base(aq_base(site).portal_catalog)._p_jar
            batches = 0
            while self._max_batches is None or batches < self._max_batches:
                count = self._apply_batch(app, site, jar, tm)
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

    def _apply_batch(self, app, site, jar, tm):
        """Apply one batch, again after a transient error. Return the
        number of keys of the batch."""
        for attempt in range(TRIES):
            try:
                return self._batch(app, site, jar, tm)
            except TransientError:
                tm.abort()
                if attempt == TRIES - 1:
                    raise
                time.sleep(PAUSES[min(attempt, len(PAUSES) - 1)])
        return 0

    def _scan(self, jar):
        """Return the pairs of the next batch, without the skipped keys."""
        queue = zoe.kv.Namespace(jar, "durable", self.name)
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
        """Wait until the storage of the catalog saw the commit
        `generation`."""
        storage = self._catalog_db.storage
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

    def _batch(self, app, site, jar, tm):
        if self._lease is not None:
            holder = zoe.kv.Namespace(jar, "ephemeral", LEASE).get(self._oid)
            if holder is not None and holder != self._lease:
                # Another thread took the lease. The run stops.
                return 0
        pairs = self._scan(jar)
        if not pairs:
            return 0
        good = []
        bad = []
        for key, value in pairs:
            try:
                parse(value)
            except ValueError as error:
                bad.append((key, value, error))
            else:
                good.append((key, value))
        if bad:
            self._reject(jar, tm, bad)
            if not good:
                return len(bad)
        queue = zoe.kv.Namespace(jar, "durable", self.name)
        # Each key of the scan committed at or before the generation.
        self._wait_for(queue.generation())
        self._begin(tm, jar)
        began = time.monotonic()
        entries = coalesce(good)
        try:
            self._apply(app, site, entries)
        except _PathFailure as failure:
            tm.abort()
            entry = entries.get(failure.path)
            self._fail(jar, tm, entry, failure)
            return len(entry.keys) if entry is not None and entry.keys else 1
        keys = [key for entry in entries.values() for key in entry.keys]
        queue = zoe.kv.Namespace(jar, "durable", self.name)
        for key in keys:
            queue.delete(key)
        if self._lease is not None:
            lease = zoe.kv.Namespace(jar, "ephemeral", LEASE)
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
        catalog = site.portal_catalog
        self._expand_security(catalog, entries)
        inner = catalog._catalog
        uids = inner.uids
        uid_index = inner.indexes.get("UID")
        removals = []
        updates = []
        for path, entry in entries.items():
            if not entry.keys and path in self._broken:
                continue
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
                    if (
                        stale is not None
                        and stale != path
                        and stale not in self._broken
                    ):
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
        indexes. Search only below the highest such paths. A search that
        fails is a failure of its highest path, so the keys of that path
        stay in the queue and count the failure."""
        tops = highest(path for path, entry in entries.items() if entry.security)
        for top in tops:
            idxs = set(entries[top].idxs & SECURITY) or set(SECURITY)
            found = self._guarded(
                top,
                lambda top=top: [
                    brain.getPath()
                    for brain in catalog.unrestrictedSearchResults(path=top)
                ],
            )
            for path in found:
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

    def _reject(self, jar, tm, bad):
        """Move each value that is not an operation to the failed
        namespace, with its error, in a transaction of its own."""
        queue = zoe.kv.Namespace(jar, "durable", self.name)
        failed = zoe.kv.Namespace(jar, "durable", failed_name(self._oid))
        self._skipped.update(key for key, _value, _error in bad)
        tm.begin()
        try:
            for key, value, error in bad:
                record = {"error": _text(error), "value": value.hex()}
                failed.set(key, json.dumps(record, sort_keys=True).encode("utf-8"))
                queue.delete(key)
                logger.error(
                    "queued indexing: a value of the queue %s is not an operation"
                    " and left the queue: %s",
                    self.name,
                    _text(error),
                )
            tm.get().note(NOTE)
            tm.commit()
        except TransientError:
            tm.abort()

    def _fail(self, jar, tm, entry, failure):
        """Count a failure for each key of the path that failed, in a
        transaction of its own. A key with `FAILURES` failures goes to the
        failed namespace. The run skips the keys of the path. A path
        without keys, as a stale path of the UID index or a path of a
        security subtree, logs an ERROR event, and the run skips it."""
        text = _text(failure.error)
        if entry is None or not entry.keys:
            self._broken.add(failure.path)
            logger.error(
                "queued indexing: %s failed and the run skips it: %s",
                failure.path,
                text,
            )
            return
        self._skipped.update(entry.keys)
        queue = zoe.kv.Namespace(jar, "durable", self.name)
        failed = zoe.kv.Namespace(jar, "durable", failed_name(self._oid))
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
    path. Only the errors of a missing path give None. A read error of the
    store, `POSKeyError` included, stays an error."""
    from Acquisition import aq_base
    from zExceptions import NotFound
    from ZODB.POSException import POSKeyError

    try:
        obj = app.unrestrictedTraverse(path)
    except (TransientError, POSKeyError):
        raise
    except (KeyError, AttributeError, NotFound):
        # `LocationError` of zope.location is a `KeyError`.
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


def status(site, since=None):
    """Return the status of the queue of `site` as a dict: the keys in the
    queue, the age of the oldest key, the failed keys, whether a thread
    holds the lease, and the delays of the keys that the indexers of this
    process applied. With `since`, a time in seconds since the Unix epoch,
    the delays are those of the batches that committed at or after it."""
    from Acquisition import aq_base

    result = {"installed": installed(), "namespaces": False}
    catalog = aq_base(aq_base(site).portal_catalog)
    jar = getattr(catalog, "_p_jar", None)
    storage = _storage(jar.db()) if jar is not None else None
    if storage is None:
        return result
    oid = catalog._p_oid
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
    """Return `(oid, key, database name, path)` of the value of the
    cookie, or None."""
    try:
        oid, key, name, path = value.split("-", 3)
        oid, key = bytes.fromhex(oid), bytes.fromhex(key)
        name = bytes.fromhex(name).decode("utf-8")
    except (AttributeError, ValueError):
        return None
    if len(oid) != 8 or len(key) != _KEY.size or not name:
        return None
    return oid, key, name, urllib.parse.unquote(path) or "/"


def _cookie_path(value, request):
    """Return the path of a cookie whose value does not parse: the path in
    the value when it has one, else the first segment of the path of the
    request, where a site at the top of the Zope root lives."""
    if isinstance(value, str):
        # The other fields are hex, so the first "-/" starts the path. The
        # path itself can hold a dash.
        start = value.find("-/")
        if start >= 0:
            return urllib.parse.unquote(value[start + 1 :])
    path = request.get("PATH_INFO", "") or ""
    first = path.strip("/").split("/", 1)[0]
    return f"/{first}" if first else "/"


def request_started(event):
    """Wait, at the start of a request with the cookie of the user, until
    the indexer applied the operations of the save of the user. Then
    begin the transaction of the request again, so that its snapshot holds
    them. The wait lasts at most `WAIT`. The request expires the cookie,
    also after a wait that ran out."""
    if not _installed:
        return
    request = event.request
    value = getattr(request, "cookies", {}).get(COOKIE)
    if not value:
        return
    parsed = _parse_cookie(value)
    if parsed is None:
        request.response.expireCookie(COOKIE, path=_cookie_path(value, request))
        return
    oid, key, name, path = parsed
    if key_time(key) > time.time() + COOKIE_SKEW:
        # The time of the key is too far in the future. The clock of the
        # instance that set the cookie is wrong, or no save set it.
        request.response.expireCookie(COOKIE, path=path)
        return
    import Zope2

    root = getattr(Zope2, "DB", None)
    db = root.databases.get(name) if root is not None else None
    storage = _storage(db) if db is not None else None
    if storage is None:
        request.response.expireCookie(COOKIE, path=path)
        return
    queue = queue_name(oid)
    stop = key + b"\x00"
    deadline = time.monotonic() + WAIT
    late = False
    while storage.kv_scan("durable", queue, None, stop, 1):
        if time.monotonic() > deadline:
            late = True
            break
        time.sleep(0.02)
    if not late and not isinstance(db.storage, DemoStorage):
        generation = storage.kv_generation(queue)
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


# The root database of the process, from `IDatabaseOpenedWithRoot`.
_root = {}

# The threads of this process, by the id of their database.
_services = {}
_services_lock = threading.Lock()


def database_opened(event):
    """Start a thread for the root database and for each database that
    Zope opened with it, when the backend is installed. Zope opens each
    database of a mount point before this event."""
    if not _installed:
        return
    _root["db"] = event.database
    for db in list(event.database.databases.values()):
        _service_for(db)


def _service_for(db):
    """Return the thread of `db`, and start it when the database is part of
    the databases of the root database and has KV namespaces. Return None
    for another database, or before the root database opened."""
    with _services_lock:
        service = _services.get(id(db))
        if service is not None:
            return service
        root = _root.get("db")
        if root is None or not _installed:
            return None
        if root.databases.get(db.database_name) is not db:
            return None
        if _storage(db) is None:
            return None
        service = _services[id(db)] = _Service(db, root)
    service.start()
    return service


class _Service(threading.Thread):
    """The thread of a database: it runs the indexer of each site whose
    catalog is in the database, while it holds the lease of the site in
    the database. It finds the site through the root database `root`.
    Each thread searches the sites of its own database: the thread of the
    root database at the top of the Zope root, the thread of a mounted
    database at the mount points of that database. It gives each site to
    the thread of the database of its catalog."""

    def __init__(self, db, root):
        super().__init__(name=f"zoe.plone.indexing {db.database_name}", daemon=True)
        self._db = db
        self._root = root
        self._token = os.urandom(16)
        self._event = threading.Event()
        self._sites = set()
        self._held = set()
        # The failures of each site: the count and the time of the next
        # try, from `time.monotonic()`.
        self._failures = {}
        self._stopping = False
        atexit.register(self.stop)

    def add(self, path):
        """Serve the site at `path`."""
        self._sites.add(tuple(path))

    def wake(self, path=None):
        if path is not None:
            self.add(path)
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
            if found is None or time.monotonic() - found > SITES_EVERY:
                found = time.monotonic()
                try:
                    self._find_sites()
                except Exception:
                    logger.exception("queued indexing: the search of the sites failed")
            for path in list(self._sites):
                failed = self._failures.get(path)
                if failed is not None and time.monotonic() < failed[1]:
                    continue
                try:
                    self._serve(path)
                except Exception:
                    count = failed[0] + 1 if failed is not None else 1
                    pause = min(POLL * 2 ** min(count, 16), BACKOFF)
                    self._failures[path] = (count, time.monotonic() + pause)
                    logger.exception(
                        "queued indexing: the queue of %s failed %d times;"
                        " the next try is in %.1f s",
                        "/".join(path),
                        count,
                        pause,
                    )
                else:
                    self._failures.pop(path, None)

    def _find_sites(self):
        """Search the sites of the database of the thread: at the top of
        the Zope root for the root database, at or just below each mount
        point of the database for a mounted database. Give each site to
        the thread of the database of its catalog."""
        from Acquisition import aq_base
        from Products.CMFCore.interfaces import ISiteRoot

        tm = transaction.TransactionManager()
        connection = self._root.open(transaction_manager=tm)
        try:
            app = connection.root()["Application"]
            candidates = []
            if self._db is self._root:
                candidates.extend(app.objectValues())
            for path, name in _mount_paths():
                if name != self._db.database_name:
                    continue
                try:
                    mounted = app.unrestrictedTraverse(path)
                except (KeyError, AttributeError):
                    continue
                candidates.append(mounted)
                values = getattr(mounted, "objectValues", None)
                if values is not None:
                    candidates.extend(values())
            for obj in candidates:
                base = aq_base(obj)
                if not ISiteRoot.providedBy(base) or not hasattr(
                    base, "portal_catalog"
                ):
                    continue
                catalog = aq_base(base.portal_catalog)
                jar = getattr(catalog, "_p_jar", None)
                if jar is None:
                    continue
                service = _service_for(jar.db())
                if service is not None:
                    service.add(obj.getPhysicalPath())
        finally:
            tm.abort()
            connection.close()

    def _serve(self, path):
        """Run the indexer of the site at `path` when its queue holds keys
        and the thread holds the lease."""
        from Acquisition import aq_base

        tm = transaction.TransactionManager()
        connection = self._root.open(transaction_manager=tm)
        try:
            try:
                site = connection.root()["Application"].unrestrictedTraverse(path)
                catalog = aq_base(aq_base(site).portal_catalog)
            except (KeyError, AttributeError):
                self._sites.discard(path)
                return
            jar = getattr(catalog, "_p_jar", None)
            if jar is None or jar.db() is not self._db or not zoe.kv._supported(jar):
                self._sites.discard(path)
                return
            oid = catalog._p_oid
            queue = zoe.kv.Namespace(jar, "durable", queue_name(oid))
            if not queue.scan(limit=1):
                return
            if not self._take(jar, tm, oid):
                return
            indexer = Indexer(site)
            indexer._lease = self._token
            indexer._max_batches = THREAD_BATCHES
            indexer.run()
            if queue.scan(limit=1):
                # The site has more keys: serve it again at once.
                self._event.set()
        finally:
            tm.abort()
            connection.close()

    def _take(self, jar, tm, oid):
        """Take the lease of the catalog `oid`, in the store of the
        connection `jar`, or keep it. Return True when the thread holds
        it."""
        lease = zoe.kv.Namespace(jar, "ephemeral", LEASE)
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


def _mount_paths():
    """Return `(path, database name)` of each mount point of the
    configuration of Zope, or nothing outside Zope."""
    try:
        from App.config import getConfiguration

        dbtab = getattr(getConfiguration(), "dbtab", None)
    except Exception:
        return []
    if dbtab is None:
        return []
    return [(path, name) for path, name in dbtab.listMountPaths() if path != "/"]
