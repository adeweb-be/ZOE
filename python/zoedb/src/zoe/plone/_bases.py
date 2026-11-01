"""The bases that the modules of `zoe.plone` share.

- `EntryMapping` is the base of the persistent mappings whose fields merge
  at the vote: `history.WorkflowHistory` and
  `properties.PrincipalProperties`. Each class that the store keeps stays
  in its own module, with its own name.
- `Observer` is the base of the data managers that write nothing
  themselves: the rows of `locking` and the moved registries of
  `registry_ops`.
- `has_namespaces(jar)` tells if the storage of a connection has KV
  namespaces.
- `private_connection(db)` opens a connection with a transaction manager
  of its own.

This module needs no Plone.
"""

import contextlib
import sys
from collections.abc import MutableMapping

import transaction
import transaction.interfaces
import zope.interface
from persistent import Persistent
from ZODB.POSException import Unsupported

import zoe.kv


class EntryMapping(Persistent, MutableMapping):
    """A persistent mapping whose state is the mapping itself.

    So each key is a field of the record, and the merge policy can give a
    strategy to each one. `__setstate__` interns the keys, as
    `Persistent.__setstate__` does, so the record of a client that loads
    the mapping and writes it again is the record of the store. A change
    of an item marks the object as changed.
    """

    def __init__(self, items=()):
        self._entries = dict(items)

    def __getstate__(self):
        return dict(self._entries)

    def __setstate__(self, state):
        entries = {
            (sys.intern(key) if type(key) is str else key): value
            for key, value in state.items()
        }
        self.__dict__.clear()
        self.__dict__["_entries"] = entries

    def __getitem__(self, key):
        return self._entries[key]

    def __setitem__(self, key, value):
        self._entries[key] = value
        self._p_changed = True

    def __delitem__(self, key):
        del self._entries[key]
        self._p_changed = True

    def __iter__(self):
        return iter(self._entries)

    def __len__(self):
        return len(self._entries)

    def __contains__(self, key):
        return key in self._entries

    def __repr__(self):
        return f"<{type(self).__name__} of {sorted(map(repr, self._entries))}>"


def has_namespaces(jar):
    """Tell if the storage of the database of the connection `jar` has KV
    namespaces."""
    try:
        zoe.kv._storage_of(jar.db())
    except Unsupported:
        return False
    return True


@contextlib.contextmanager
def private_connection(db):
    """Open a connection of `db` with a transaction manager of its own, so
    that its transactions are not those of the thread. Yield the connection
    and the manager. At the end, abort the transaction of the manager and
    close the connection."""
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        yield connection, manager
    finally:
        manager.abort()
        connection.close()


@zope.interface.implementer(transaction.interfaces.IDataManager)
class Observer:
    """A data manager of one transaction of one connection that writes
    nothing itself.

    It joins the transaction, so that a rollback to a savepoint and the end
    of the transaction reach it. A transaction that is not active, as in an
    after-commit hook, takes no data manager: `joined` is then false, and
    the end of the transaction does not reach it.

    Before each savepoint, it calls `write_back`, which stages what changed
    in place. A rollback to a savepoint calls `rollback`, and the end of the
    transaction calls `end`. A subclass gives the three, and `sort_prefix`,
    the start of its sort key.
    """

    sort_prefix = "zoe.plone"

    def __init__(self, jar, txn):
        self.transaction_manager = jar.transaction_manager
        self.transaction = txn
        self._jar = jar
        try:
            txn.join(self)
        except ValueError:
            self.joined = False
        else:
            self.joined = True

    def flush_before_savepoint(self, namespace):
        """Call `write_back` before each savepoint of the writes of the
        `zoe.kv.Namespace` `namespace`, so that the savepoint keeps what it
        stages, whatever the order in which the two joined the
        transaction."""
        writes = namespace._staged(create=True)
        writes.flush_before_savepoint(self.write_back)

    def write_back(self):
        """Stage what changed in place."""

    def rollback(self):
        """Drop what the transaction decoded."""

    def end(self):
        """Forget the transaction."""

    # IDataManager

    def abort(self, txn):
        self.end()

    def tpc_begin(self, txn):
        pass

    def commit(self, txn):
        pass

    def tpc_vote(self, txn):
        pass

    def tpc_finish(self, txn):
        self.end()

    def tpc_abort(self, txn):
        self.end()

    def sortKey(self):
        return f"{self.sort_prefix}:{id(self)}"

    def savepoint(self):
        self.write_back()
        return _Savepoint(self)


@zope.interface.implementer(transaction.interfaces.IDataManagerSavepoint)
class _Savepoint:
    """A savepoint of an `Observer`: its rollback calls `rollback` of the
    observer."""

    def __init__(self, observer):
        self._observer = observer

    def rollback(self):
        self._observer.rollback()
