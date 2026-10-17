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
nearly always write it, and whose invariants do not span other objects. A
hot class must also be a leaf record: a hot object that refers to other
persistent objects can name a child that its transaction loads at its
snapshot, older than the hot object, or that the snapshot does not hold.

`install()` puts the methods of `LockingConnection` on
`ZODB.Connection.Connection`, once per process. A storage with early locks
calls it when it opens. Each patched method checks the storage of the
database of the connection first: for a storage without hot classes, and
for a historical connection (`DB.open(before=...)`), it does what the
method that it replaced does. `install` keeps the methods that it finds on
`Connection`, so a patch that another package made before stays in the
chain. `LockingConnection` is the same methods as a subclass, for a program
that makes its `DB` with `klass` itself.

Each connection has an owner of intent locks of its own. The intent locks
come in oid order. A load of a hot object below a hot object that the
transaction holds releases the locks of the transaction and takes them all
again in oid order, with the new one: when every serial that the
transaction read is unchanged, the load goes on; otherwise it raises
`ConflictError`, which ends the attempt. `tpc_begin` gives the locks to the
transaction of the storage, and its end releases them. The end of a
transaction that did not commit through `tpc_begin` releases them too, with
an implicit or an explicit transaction manager, and so does the close of
the connection, which also turns its hot objects into ghosts.
"""

import logging
import re
import threading

from ZODB.blob import Blob
from ZODB.Connection import Connection
from ZODB.POSException import ConflictError, StorageTransactionError

__all__ = ["LockingConnection", "hot_classes", "install"]

logger = logging.getLogger(__name__)

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
        # True when the owner asked for a lock since the last end. A failed
        # request can keep a connection of a pool for the owner.
        self.asked = False

    def lock(self, oid):
        """Take the intent lock of oid, and return its serial."""
        serial = self.held.get(oid)
        if serial is not None:
            return serial
        self.asked = True
        try:
            serials = self.storage.lock_intent([oid], self.owner)
        except StorageTransactionError:
            # An oid below a held one: take the whole set again in oid
            # order.
            return self._relock(oid)
        self.held[oid] = serials[0]
        return serials[0]

    def _relock(self, oid):
        """Release the locks of the transaction, and take them again in oid
        order with `oid`. Raise `ConflictError` when another transaction
        changed an object that the transaction read meanwhile."""
        read = dict(self.held)
        self.held.clear()
        self.storage.unlock_intent(self.owner)
        wanted = sorted({*read, oid})
        serials = self.storage.lock_intent(wanted, self.owner)
        self.held.update(zip(wanted, serials, strict=True))
        if any(self.held[held] != serial for held, serial in read.items()):
            raise ConflictError(oid=oid)
        return self.held[oid]

    def end(self, connection):
        """End the locks of a transaction: release the ones that no
        `tpc_begin` took, and turn the hot objects of the cache into
        ghosts, so the next transaction loads them again with the lock.

        The local state goes first: a failed release changes no state of
        the connection, and the drop of the owner on the server, or its end
        of connection, releases the locks.
        """
        self.held.clear()
        if self.cached:
            connection._cache.invalidate(list(self.cached))
            self.cached.clear()
        if not self.asked:
            return
        self.asked = False
        try:
            self.storage.unlock_intent(self.owner)
        except Exception:
            logger.warning(
                "the release of the intent locks of a connection failed",
                exc_info=True,
            )


def _hot_storage(connection):
    """Return the storage of the database of connection and its hot
    classes, or (None, None) when it has none or the connection is
    historical."""
    db = connection._db
    storage = getattr(db, "storage", None) if db is not None else None
    classes = getattr(storage, "early_lock_classes", None)
    if not classes or connection.before is not None:
        return None, None
    return storage, classes


def _is_hot(obj, classes):
    """Tell if obj is of a class of classes. ZODB pickles the class of a
    record as its module and its name."""
    klass = type(obj)
    return f"{klass.__module__}.{klass.__name__}" in classes


def _intents(connection, storage):
    """Return the intents of connection on storage."""
    intents = connection.__dict__.get("_zoe_intents")
    if intents is None or intents.storage is not storage:
        intents = connection.__dict__["_zoe_intents"] = _Intents(storage)
    return intents


# The methods of `Connection` that `install` replaced, from the time of the
# install. Until then, the methods of ZODB.
_NAMES = (
    "setstate",
    "newTransaction",
    "afterCompletion",
    "tpc_begin",
    "tpc_finish",
    "close",
)
_replaced = {name: getattr(Connection, name) for name in _NAMES}


class LockingConnection(Connection):
    """A ZODB `Connection` that takes early write-intent locks of the hot
    classes of its storage."""

    def setstate(self, obj):
        storage, classes = (
            _hot_storage(self) if self.opened is not None else (None, None)
        )
        if not classes or obj._p_oid is None or not _is_hot(obj, classes):
            return _replaced["setstate"](self, obj)
        oid = obj._p_oid
        intents = _intents(self, storage)
        serial = intents.lock(oid)
        savepoint = self._savepoint_storage
        if savepoint is not None and oid in savepoint.index:
            # A savepoint holds a change of the transaction: the stock load
            # reads it, under the lock.
            return _replaced["setstate"](self, obj)
        # The record that was current at the grant, past the snapshot of the
        # transaction: no other transaction can commit it while the lock
        # holds.
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
        return _replaced["newTransaction"](self, transaction, sync)

    def afterCompletion(self, transaction):
        # An explicit transaction manager calls no `newTransaction` here.
        intents = self.__dict__.get("_zoe_intents")
        if intents is not None:
            intents.end(self)
        return _replaced["afterCompletion"](self, transaction)

    def tpc_begin(self, transaction):
        storage, classes = _hot_storage(self)
        if not classes:
            return _replaced["tpc_begin"](self, transaction)
        # The transaction of the storage adopts the locks of the connection,
        # and never those of the default owner of the storage: a connection
        # that took no lock binds its own owner, which holds none.
        intents = _intents(self, storage)
        storage.bind_intent_owner(intents.owner)
        try:
            return _replaced["tpc_begin"](self, transaction)
        finally:
            storage.bind_intent_owner(None)

    def tpc_finish(self, transaction):
        storage, classes = _hot_storage(self)
        if not classes:
            return _replaced["tpc_finish"](self, transaction)
        oids = [*self._modified, *self._creating]
        result = _replaced["tpc_finish"](self, transaction)
        # The cache holds the committed hot objects up to date, without a
        # lock: the next transaction loads them again with the lock.
        for oid in oids:
            obj = self._cache.get(oid)
            if obj is not None and _is_hot(obj, classes):
                _intents(self, storage).cached.add(oid)
        return result

    def close(self, primary=True):
        # A connection joined to a transaction refuses the close, and keeps
        # its locks.
        intents = self.__dict__.get("_zoe_intents")
        if intents is not None and self._needs_to_join:
            intents.end(self)
        return _replaced["close"](self, primary)


_installed = False
_install_lock = threading.Lock()


def install():
    """Put the methods of `LockingConnection` on `ZODB.Connection.Connection`,
    once per process. The methods that it replaces are those that
    `Connection` has at this call."""
    global _installed
    with _install_lock:
        if _installed:
            return
        for name in _NAMES:
            _replaced[name] = getattr(Connection, name)
            setattr(Connection, name, LockingConnection.__dict__[name])
        _installed = True
