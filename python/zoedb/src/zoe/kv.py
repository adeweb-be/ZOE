"""The KV namespaces of a ZOE store on a connection of a database (ADR 0043).

`Namespace(connection, flavour, name)` is the namespace `name` of
`flavour`, `"ephemeral"` or `"durable"`, of the store of the database of
`connection`. The store is a `zoe.Storage` or a `zoe.zeo.ClientStorage`;
another storage raises `ZODB.POSException.Unsupported`.

A key is bytes, or a str, which the namespace writes as its UTF-8 bytes.
UTF-8 byte order is the code point order of a str, so a scan of str keys
gives the order of an OOBTree. A value is bytes.

`set` and `delete` keep the write in a data manager that joins the current
transaction of the connection. `get` and `scan` see the writes of that
transaction first, then the last committed values: the namespaces keep no
history, so a read does not follow the snapshot of the connection. The
data manager supports savepoints.

At the commit, the data manager stages its writes in the storage
transaction that the connection began, so they commit in one write with
the objects of the transaction. When the connection began none, because it
changed no object, the data manager begins a storage transaction of its
own and drives its vote, its finish and its abort. An abort drops the
writes.
"""

import weakref

import transaction.interfaces
import zope.interface
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import Unsupported

__all__ = ["Namespace"]

# The writes of the current transaction of each connection.
_writes = weakref.WeakKeyDictionary()


def _key(key):
    """Return `key` as bytes: a str gives its UTF-8 bytes."""
    if isinstance(key, str):
        return key.encode("utf-8")
    if isinstance(key, bytes):
        return key
    raise TypeError(f"a key is bytes or a str, not {type(key).__name__}")


def _bound(bound):
    """Return the bound of a scan as bytes, or None for an open end."""
    return None if bound is None else _key(bound)


class Namespace:
    """The namespace `name` of `flavour` of the store of `connection`."""

    def __init__(self, connection, flavour, name):
        if flavour not in ("ephemeral", "durable"):
            raise ValueError(
                'the flavour of a namespace is "ephemeral" or "durable",'
                f" not {flavour!r}"
            )
        if not isinstance(name, str):
            raise TypeError(
                f"the name of a namespace is a str, not {type(name).__name__}"
            )
        storage = connection.db().storage
        if getattr(storage, "kv_set", None) is None:
            raise Unsupported(
                f"{type(storage).__name__} has no KV namespaces: use zoe.Storage"
                " or zoe.zeo.ClientStorage"
            )
        self._connection = connection
        self._storage = storage
        self.flavour = flavour
        self.name = name

    def __repr__(self):
        return f"<Namespace {self.flavour} {self.name!r}>"

    def _staged(self, create):
        """Return the writes of the current transaction of the connection.
        With `create`, make them and join the transaction when there are
        none. Else return None when there are none."""
        txn = self._connection.transaction_manager.get()
        found = _writes.get(self._connection)
        if found is not None and found.transaction is txn:
            return found
        if not create:
            return None
        writes = _Writes(self._storage, self._connection, txn)
        _writes[self._connection] = writes
        txn.join(writes)
        return writes

    def get(self, key, default=None):
        """Return the value of `key`, or `default` when it has none or is
        expired. A write of the current transaction comes first."""
        key = _key(key)
        writes = self._staged(create=False)
        if writes is not None:
            found, value = writes.find(self.flavour, self.name, key)
            if found:
                return default if value is None else value
        value = self._storage.kv_get(self.flavour, self.name, key)
        return default if value is None else value

    def scan(self, start=None, stop=None, limit=1000):
        """Return the pairs `(key, value)` of the namespace, in the byte
        order of the keys, from `start` (included) to `stop` (excluded),
        with the writes of the current transaction. `None` is an open end.
        The list holds at most `limit` pairs, and at most 1,000. The keys
        are bytes."""
        start, stop = _bound(start), _bound(stop)
        if not isinstance(limit, int) or isinstance(limit, bool) or limit < 1:
            raise ValueError("the limit of a scan is an int of 1 or more")
        limit = min(limit, 1000)
        pairs = dict(self._storage.kv_scan(self.flavour, self.name, start, stop, limit))
        writes = self._staged(create=False)
        if writes is not None:
            for key, value in writes.of(self.flavour, self.name):
                if (start is not None and key < start) or (
                    stop is not None and key >= stop
                ):
                    continue
                if value is None:
                    pairs.pop(key, None)
                else:
                    pairs[key] = value
        return sorted(pairs.items())[:limit]

    def generation(self):
        """Return the generation of the durable namespace: the 8 bytes of
        the tid of the last commit that wrote it. A cache of the namespace
        stays valid while its generation stays the same."""
        return self._storage.kv_generation(self.name)

    def set(self, key, value, ttl=None):
        """Write `value` for `key` in the current transaction. An ephemeral
        key needs `ttl`, in seconds; a durable key takes none."""
        key = _key(key)
        if not isinstance(value, bytes):
            raise TypeError(f"a value is bytes, not {type(value).__name__}")
        self._staged(create=True).put(self.flavour, self.name, key, value, ttl)

    def delete(self, key):
        """Delete `key` in the current transaction. A key without a value
        stays without one."""
        key = _key(key)
        self._staged(create=True).put(self.flavour, self.name, key, None, None)


@zope.interface.implementer(transaction.interfaces.IDataManager)
class _Writes:
    """The namespace writes of one transaction of one connection.

    It is a data manager of the transaction. Its writes go to the storage
    transaction of the connection, or to a storage transaction of its own.
    """

    def __init__(self, storage, connection, txn):
        self.transaction_manager = connection.transaction_manager
        self.transaction = txn
        self._storage = storage
        self._connection = connection
        # (flavour, name, key) -> (value or None for a delete, ttl)
        self._writes = {}
        # The storage transaction that this data manager began, or None.
        self._meta = None

    def put(self, flavour, name, key, value, ttl):
        self._writes[(flavour, name, key)] = (value, ttl)

    def find(self, flavour, name, key):
        """Return `(True, value)` for a write of `key`, `value` None for a
        delete, else `(False, None)`."""
        write = self._writes.get((flavour, name, key))
        if write is None:
            return False, None
        return True, write[0]

    def of(self, flavour, name):
        """Return the key and the value of each write of the namespace."""
        return [
            (key, value)
            for (f, n, key), (value, _ttl) in self._writes.items()
            if f == flavour and n == name
        ]

    def _end(self):
        if _writes.get(self._connection) is self:
            del _writes[self._connection]

    # IDataManager

    def abort(self, txn):
        self._writes.clear()
        self._end()

    def tpc_begin(self, txn):
        pass

    def commit(self, txn):
        # Each resource had its `tpc_begin` before this call. So the
        # connection began its storage transaction here when it is a
        # resource of `txn`.
        try:
            meta = txn.data(self._connection)
        except KeyError:
            meta = None
        if meta is None:
            meta = TransactionMetaData(txn.user, txn.description, txn.extension)
            self._storage.tpc_begin(meta)
            self._meta = meta
        for (flavour, name, key), (value, ttl) in self._writes.items():
            if value is None:
                self._storage.kv_delete(flavour, name, key, meta)
            else:
                self._storage.kv_set(flavour, name, key, value, ttl, meta)

    def tpc_vote(self, txn):
        if self._meta is not None:
            self._storage.tpc_vote(self._meta)

    def tpc_finish(self, txn):
        try:
            if self._meta is not None:
                self._storage.tpc_finish(self._meta)
        finally:
            self._meta = None
            self._end()

    def tpc_abort(self, txn):
        try:
            if self._meta is not None:
                self._storage.tpc_abort(self._meta)
        finally:
            self._meta = None
            self._writes.clear()
            self._end()

    def sortKey(self):
        # After the connection, whose key is the key of its storage.
        return f"{self._storage.sortKey()}:zoe.kv:{id(self)}"

    def savepoint(self):
        return _Savepoint(self, dict(self._writes))


@zope.interface.implementer(transaction.interfaces.IDataManagerSavepoint)
class _Savepoint:
    """A savepoint of the writes: its rollback gives them back."""

    def __init__(self, writes, state):
        self._writes = writes
        self._state = state

    def rollback(self):
        self._writes._writes = dict(self._state)
