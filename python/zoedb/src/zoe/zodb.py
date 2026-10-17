"""ZODB integration: early write-intent locks (ADR 0045).

With `early-locks on`, an object of a hot class, a class that
`early-lock-classes` names, takes its intent lock when its state loads,
before the transaction sees any byte of it. The load waits for the lock,
then reads the record that was current at the grant, not the record of the
snapshot of the transaction. So a second writer of a hot object waits
before it reads, and reads the commit of the first one: its vote has no
conflict.

The costs: a transaction sees a hot object at a state newer than its
snapshot, and every transaction that reads a hot object waits for its
lock, also one that does not write it. So a hot class is one whose readers
nearly always write it, and whose invariants do not span other objects.

`install()` puts the methods of `LockingConnection` on
`ZODB.Connection.Connection`, once per process. A storage with early locks
calls it when it opens. Each patched method checks the storage of the
database of the connection first: for a storage without early locks, it does
what ZODB does. `LockingConnection` is the same methods as a subclass, for a
program that makes its `DB` with `klass` itself.

Each connection has an owner of intent locks of its own. The intent locks
come in oid order: a load of a hot object below a hot object that the
transaction holds raises `ConflictError`, which ends the attempt, and the
next attempt of the connection takes all the hot oids of the failed attempt
in oid order, at its first load of a hot object. `tpc_begin` gives the
locks to the transaction of the storage, and its end releases them. The end
of a transaction that did not commit through `tpc_begin` releases them too.
"""

import re
import threading

from ZODB.blob import Blob
from ZODB.Connection import Connection
from ZODB.POSException import ConflictError, StorageTransactionError

__all__ = ["LockingConnection", "hot_classes", "install"]

# The classes that tier 1 merges in the vote (ADR 0042): the buckets, sets,
# trees and tree sets of each family of BTrees but `fs`, and `Length`. The
# lock hand-off serves them, so none of them is a hot class.
_TIER1 = re.compile(
    r"BTrees\.(?!fs)[A-Z]{2}BTree\.[A-Z]{2}(Bucket|Set|BTree|TreeSet)|BTrees\.Length\.Length"
)


def hot_classes(early_locks, classes):
    """Return the hot classes of a storage, as a frozenset of `module.name`,
    or None when `early_locks` is false.

    `classes` names the hot classes. A class of tier 1 raises `ValueError`.
    """
    if not early_locks:
        return None
    classes = frozenset(classes or ())
    for name in sorted(classes):
        if not re.fullmatch(r"[\w.]+\.\w+", name):
            raise ValueError(f"early-lock-classes: {name!r} is not module.name")
        if _TIER1.fullmatch(name):
            raise ValueError(
                f"early-lock-classes: tier 1 merges {name}, so it is never hot"
            )
    return classes


class _Intents:
    """The intent locks of one connection."""

    def __init__(self, storage):
        self.storage = storage
        self.owner = storage.new_intent_owner()
        # The serial of each hot oid that the transaction holds, as the grant
        # gave it.
        self.held = {}
        # The hot oids that the cache holds up to date: the transaction
        # loaded them or committed them.
        self.cached = set()
        # The hot oids of an attempt that ended on `LockOrder`.
        self.retry = set()

    def lock(self, oid):
        """Take the intent lock of oid, and return its serial."""
        serial = self.held.get(oid)
        if serial is not None:
            return serial
        wanted = sorted(self.retry | {oid}) if not self.held else [oid]
        try:
            serials = self.storage.lock_intent(wanted, self.owner)
        except StorageTransactionError:
            # A hot oid below a held one: the attempt ends, and the next one
            # takes them all in oid order.
            self.retry |= set(self.held) | {oid}
            raise ConflictError(oid=oid) from None
        self.held.update(zip(wanted, serials, strict=True))
        self.retry.clear()
        return self.held[oid]

    def end(self, connection):
        """End the locks of a transaction: release the ones that no
        `tpc_begin` took, and turn the loaded hot objects into ghosts, so the
        next transaction loads them again with the lock."""
        if self.held:
            self.storage.unlock_intent(self.owner)
            self.held.clear()
        if self.cached:
            connection._cache.invalidate(list(self.cached))
            self.cached.clear()


def _hot_storage(connection):
    """Return the storage of the database of connection and its hot
    classes, or (None, None) when it has none."""
    db = connection._db
    storage = getattr(db, "storage", None) if db is not None else None
    classes = getattr(storage, "early_lock_classes", None)
    if not classes:
        return None, None
    return storage, classes


def _is_hot(obj, classes):
    """Tell if obj is of a class of classes. ZODB pickles the class of a
    record as its module and its name."""
    klass = type(obj)
    return f"{klass.__module__}.{klass.__name__}" in classes


def _hot(connection, obj):
    """Return the intents of connection when obj is of a hot class of its
    storage, else None."""
    storage, classes = _hot_storage(connection)
    if not classes or not _is_hot(obj, classes):
        return None
    return _intents(connection, storage)


def _intents(connection, storage):
    """Return the intents of connection on storage."""
    intents = connection.__dict__.get("_zoe_intents")
    if intents is None or intents.storage is not storage:
        intents = connection.__dict__["_zoe_intents"] = _Intents(storage)
    return intents


_ZODB = {
    name: getattr(Connection, name)
    for name in ("setstate", "newTransaction", "tpc_begin", "tpc_finish", "close")
}


class LockingConnection(Connection):
    """A ZODB `Connection` that takes early write-intent locks of the hot
    classes of its storage."""

    def setstate(self, obj):
        intents = _hot(self, obj) if self.opened is not None else None
        if intents is None or obj._p_oid is None:
            return _ZODB["setstate"](self, obj)
        oid = obj._p_oid
        serial = intents.lock(oid)
        # The record that was current at the grant, past the snapshot of the
        # transaction: no other transaction can commit it while the lock
        # holds.
        storage = intents.storage
        data = storage.loadSerial(oid, serial)
        self._load_count += 1
        self._reader.setGhostState(obj, data)
        obj._p_serial = serial
        self._cache.update_object_size_estimation(oid, len(data))
        obj._p_estimated_size = len(data)
        if isinstance(obj, Blob):
            obj._p_blob_uncommitted = None
            obj._p_blob_committed = storage.loadBlob(oid, serial)
        intents.cached.add(oid)
        return None

    def newTransaction(self, transaction, sync=True):
        intents = self.__dict__.get("_zoe_intents")
        if intents is not None:
            intents.end(self)
        return _ZODB["newTransaction"](self, transaction, sync)

    def tpc_begin(self, transaction):
        intents = self.__dict__.get("_zoe_intents")
        if intents is None or not intents.held:
            return _ZODB["tpc_begin"](self, transaction)
        # The transaction of the storage adopts the locks of the connection.
        bind = intents.storage.bind_intent_owner
        bind(intents.owner)
        try:
            return _ZODB["tpc_begin"](self, transaction)
        finally:
            bind(None)

    def tpc_finish(self, transaction):
        storage, classes = _hot_storage(self)
        if not classes:
            return _ZODB["tpc_finish"](self, transaction)
        oids = [*self._modified, *self._creating]
        result = _ZODB["tpc_finish"](self, transaction)
        # The cache holds the committed hot objects up to date, without a
        # lock: the next transaction loads them again with the lock.
        for oid in oids:
            obj = self._cache.get(oid)
            if obj is not None and _is_hot(obj, classes):
                _intents(self, storage).cached.add(oid)
        return result

    def close(self, primary=True):
        intents = self.__dict__.pop("_zoe_intents", None)
        if intents is not None and intents.held:
            intents.storage.unlock_intent(intents.owner)
        return _ZODB["close"](self, primary)


_installed = False
_install_lock = threading.Lock()


def install():
    """Put the methods of `LockingConnection` on `ZODB.Connection.Connection`,
    once per process."""
    global _installed
    with _install_lock:
        if _installed:
            return
        for name in _ZODB:
            setattr(Connection, name, LockingConnection.__dict__[name])
        _installed = True
