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
stock behaviour. `uninstall()` puts the stock methods back. The locks that
the namespace holds then stop counting; each one expires with its TTL.

One row holds the locks of one object: a pickle of the WebDAV locks and of
the types. A transaction decodes the row once. Its reads and its changes go
to that copy, and a hook before its commit writes the copy back when it
changed, also when a `LockItem` changed in place, as `refresh` does. The
TTL of the row is the longest time that a lock of the row stays valid, at
least one second. A row without a lock is deleted. An expired lock reads as
absent, and its row goes with its TTL, so a read never writes.

Two editors who open one form at the same moment can both take the lock:
the later commit wins (ADR 0043). With the stock storage, one of them gets
a `ConflictError` instead.

The namespace keeps no history, so the locks do not follow the snapshot
of a connection, and undo gives back no lock.
"""

import math
import pickle
import time
import weakref
from collections.abc import MutableMapping

__all__ = ["NAMESPACE", "install", "installed", "uninstall"]

# The name of the namespace of the locks.
NAMESPACE = "zoe.plone.locks"

# The longest TTL that the core takes, in seconds: the longest timeout of a
# WebDAV lock.
MAX_TTL = 2**32 - 1

# The stock methods while the backend is installed.
_stock = {}

# The row of each object that the current transaction of each connection
# decoded: connection -> (transaction, {oid: _Row}).
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
        return _Mapping(row, "locks")

    def _locks(self, create=True):
        row = _row_of(self.context)
        if row is None:
            return _stock["_locks"](self, create)
        return _Mapping(row, "types")

    wl_lockmapping.__doc__ = _stock["wl_lockmapping"].__doc__
    LockableItem.wl_lockmapping = wl_lockmapping
    TTWLockable._locks = _locks


def uninstall():
    """Put the stock methods back."""
    if not _stock:
        return
    from OFS.Lockable import LockableItem
    from plone.locking.lockable import TTWLockable

    LockableItem.wl_lockmapping = _stock.pop("wl_lockmapping")
    TTWLockable._locks = _stock.pop("_locks")


def _row_of(obj):
    """Return the row of the locks of `obj` for the current transaction of
    its connection, or None when `obj` keeps its locks itself."""
    from Acquisition import aq_base
    from plone.locking.interfaces import ITTWLockable

    import zoe.kv

    base = aq_base(obj)
    if not ITTWLockable.providedBy(base):
        return None
    jar = getattr(base, "_p_jar", None)
    oid = getattr(base, "_p_oid", None)
    if jar is None or oid is None or not zoe.kv.supported(jar):
        return None
    txn = jar.transaction_manager.get()
    found = _rows.get(jar)
    if found is None or found[0] is not txn:
        found = _rows[jar] = (txn, {})
        txn.addBeforeCommitHook(_write_back, (jar, txn))
    rows = found[1]
    row = rows.get(oid)
    if row is None:
        row = rows[oid] = _Row(zoe.kv.Namespace(jar, "ephemeral", NAMESPACE), oid)
    return row


def _write_back(jar, txn):
    """Write each row of `txn` on `jar` that changed, before the commit."""
    found = _rows.get(jar)
    if found is None or found[0] is not txn:
        return
    for row in found[1].values():
        row.write()


class _Row:
    """The locks of one object, decoded once in a transaction."""

    def __init__(self, namespace, oid):
        self._namespace = namespace
        self._oid = oid
        stored = namespace.get(oid)
        self.data = {"locks": {}, "types": {}} if stored is None else pickle.loads(stored)
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

    def __delitem__(self, key):
        del self._items()[key]

    def __iter__(self):
        return iter([key for key, value in self._items().items() if self._valid(value)])

    def __len__(self):
        return sum(1 for value in self._items().values() if self._valid(value))

    def clear(self):
        self._items().clear()


def process_starting(event):
    """Install the backend when Zope starts: the subscriber of
    `locking.zcml`."""
    install()
