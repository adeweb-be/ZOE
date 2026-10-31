"""The locks of plone.locking in the ephemeral namespace of the store.

The edit form of a Plone item locks it when the form opens (plone.app.
lockingbehavior). With the stock storage of plone.locking, that GET writes
the content object: a WebDAV `LockItem` in its `_dav_writelocks`, and the
type of the lock in its annotation `plone.locking`. Two editors of one
item, or an editor and any other write of the item, then conflict.

`install()` keeps both in the ephemeral namespace `zoe.plone.locks` of the
store of the item, under the oid of the item, with the TTL of the lock. The
lock of an edit form then writes no object. It replaces two things, for an
object that provides `ITTWLockable` and whose storage has KV namespaces:

- `OFS.Lockable.LockableItem.wl_lockmapping`, the mapping of the WebDAV
  locks of the object, so every `wl_*` method of Zope reads and writes the
  namespace;
- `plone.locking.lockable.TTWLockable._locks`, the mapping of the types of
  the locks.

Every other object, and an object whose storage has no namespace, keeps the
stock behaviour. The backend does not carry locks over: the locks that an
object kept itself before `install()` do not count while it is installed.
A lock lives for minutes, so this ends with the next edit.

One row holds all the locks of one object: a pickle of the WebDAV locks and
of the types. A transaction decodes the row once, and its reads and its
changes go to that copy. Each change of a mapping stages the row in the
namespace at once, so a savepoint covers it. Each savepoint and a hook
before the commit stage the rows again, for a `LockItem` that changed in
place, as `refresh` does. A rollback to a savepoint drops the copies, and
the next read decodes the row again. The TTL of the row is the longest
time that a lock of the row stays valid, at least one second. A row
without a lock is deleted.

A lock that is no longer valid reads as absent. With `killinvalids`,
`wl_lockmapping` deletes such locks from the row, and calls
`__no_valid_write_locks__` of the object when that leaves no lock, as OFS
does. The row expires with its longest valid lock, so a read writes only
in the second after a lock ends, or for an object with several locks.

The row is the unit of a write: when two transactions change the locks of
one object, the later commit wins for all the locks of the object (ADR
0043). So two editors who open one form at the same moment can both take
the lock. With the stock storage, one of them gets a `ConflictError`
instead.

The namespace keeps no history, so the locks do not follow the snapshot
of a connection, and undo gives back no lock.
"""

import math
import pickle
import time
import weakref
from collections.abc import MutableMapping

import zoe.kv
from zoe.plone._bases import Observer, has_namespaces

__all__ = ["NAMESPACE", "install", "installed"]

# The name of the namespace of the locks.
NAMESPACE = "zoe.plone.locks"

# The longest TTL that the core takes, in seconds: the longest timeout of a
# WebDAV lock.
MAX_TTL = zoe.kv.MAX_TTL

# The stock methods while the backend is installed.
_stock = {}

# The rows that the current transaction of each connection decoded:
# connection -> _Rows.
_rows = weakref.WeakKeyDictionary()


def installed():
    """Tell if the backend is installed."""
    return bool(_stock)


def install():
    """Keep the locks of plone.locking in the ephemeral namespace of the
    store. A second call does nothing."""
    if _stock:
        return
    from OFS.Lockable import LockableItem
    from plone.locking.lockable import TTWLockable

    _stock["wl_lockmapping"] = LockableItem.wl_lockmapping
    _stock["_locks"] = TTWLockable._locks

    def wl_lockmapping(self, killinvalids=0, create=0):
        row = _row_of(self)
        if row is None:
            return _stock["wl_lockmapping"](self, killinvalids, create)
        locks = _Mapping(row, "locks")
        if killinvalids and locks.remove_invalid() and not locks:
            from Acquisition import aq_base

            if hasattr(aq_base(self), "__no_valid_write_locks__"):
                self.__no_valid_write_locks__()
        return locks

    def _locks(self, create=True):
        row = _row_of(self.context)
        if row is None:
            return _stock["_locks"](self, create)
        return _Mapping(row, "types")

    wl_lockmapping.__doc__ = _stock["wl_lockmapping"].__doc__
    LockableItem.wl_lockmapping = wl_lockmapping
    TTWLockable._locks = _locks


def _row_of(obj):
    """Return the row of the locks of `obj` for the current transaction of
    its connection, or None when `obj` keeps its locks itself."""
    from Acquisition import aq_base
    from plone.locking.interfaces import ITTWLockable

    base = aq_base(obj)
    if not ITTWLockable.providedBy(base):
        return None
    jar = getattr(base, "_p_jar", None)
    oid = getattr(base, "_p_oid", None)
    if jar is None or oid is None or not has_namespaces(jar):
        return None
    txn = jar.transaction_manager.get()
    rows = _rows.get(jar)
    if rows is None or rows.transaction is not txn:
        rows = _rows[jar] = _Rows(jar, txn)
    row = rows.get(oid)
    if row is None:
        row = rows[oid] = _Row(zoe.kv.Namespace(jar, "ephemeral", NAMESPACE), oid)
    return row


class _Rows(Observer, dict):
    """The rows that one transaction of one connection decoded, by oid.

    It joins the transaction as an `Observer`, so that a rollback to a
    savepoint drops the rows: the next read decodes them from the writes
    that the rollback gave back. The writes of the namespace stage the rows
    before each of their savepoints, so the savepoint keeps a `LockItem`
    that changed in place, whatever the order in which the two joined. A
    transaction that is not active takes no data manager: its reads decode
    the rows, and a write fails as any write after the commit does.
    """

    sort_prefix = "zoe.plone.locking"

    def __init__(self, jar, txn):
        super().__init__(jar, txn)
        if self.joined:
            self.flush_before_savepoint(zoe.kv.Namespace(jar, "ephemeral", NAMESPACE))
            txn.addBeforeCommitHook(self.write_back)

    def write_back(self):
        """Stage each row that changed: a `LockItem` can change in place."""
        for row in self.values():
            row.write()

    def rollback(self):
        self.clear()

    def end(self):
        self.clear()
        if _rows.get(self._jar) is self:
            del _rows[self._jar]


class _Row:
    """The locks of one object, decoded once in a transaction."""

    def __init__(self, namespace, oid):
        self._namespace = namespace
        self._oid = oid
        stored = namespace.get(oid)
        self.data = (
            {"locks": {}, "types": {}} if stored is None else pickle.loads(stored)
        )
        # The encoding of the row as read, so a row that a transaction only
        # read is never written.
        self._stored = None if stored is None else pickle.dumps(self.data, 3)

    def write(self):
        """Write the row when it differs from the stored one: with the TTL
        of its longest valid lock, or a delete when it holds no lock."""
        locks = self.data["locks"]
        if not locks and not self.data["types"]:
            if self._stored is not None:
                self._namespace.delete(self._oid)
                self._stored = None
            return
        encoded = pickle.dumps(self.data, 3)
        if encoded == self._stored:
            return
        self._namespace.set(self._oid, encoded, ttl=_ttl(locks.values()))
        self._stored = encoded


def _ttl(locks):
    """Return the TTL of a row of `locks`: the longest time that one of
    them stays valid, in seconds, at least 1 and at most `MAX_TTL`."""
    now = time.time()
    left = 1.0
    for lock in locks:
        try:
            left = max(left, lock.getModifiedTime() + lock.getTimeout() - now)
        except (AttributeError, TypeError):
            left = MAX_TTL
    return min(MAX_TTL, max(1, math.ceil(left)))


class _Mapping(MutableMapping):
    """One part of the row of an object: its WebDAV locks (`locks`) or the
    types of its locks (`types`). A lock that is no longer valid reads as
    absent."""

    def __init__(self, row, part):
        self._row = row
        self._part = part

    def _items(self):
        return self._row.data[self._part]

    def _valid(self, value):
        if self._part != "locks":
            return True
        is_valid = getattr(value, "isValid", None)
        return is_valid is None or is_valid()

    def __getitem__(self, key):
        value = self._items()[key]
        if not self._valid(value):
            raise KeyError(key)
        return value

    def __setitem__(self, key, value):
        self._items()[key] = value
        self._row.write()

    def __delitem__(self, key):
        del self._items()[key]
        self._row.write()

    def __iter__(self):
        return iter([key for key, value in self._items().items() if self._valid(value)])

    def __len__(self):
        return sum(1 for value in self._items().values() if self._valid(value))

    def clear(self):
        self._items().clear()
        self._row.write()

    def remove_invalid(self):
        """Delete the locks that are no longer valid. Tell if there were
        some."""
        items = self._items()
        invalid = [key for key, value in items.items() if not self._valid(value)]
        for key in invalid:
            del items[key]
        if invalid:
            self._row.write()
        return bool(invalid)
