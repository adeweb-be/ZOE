"""Unit tests of conflict resolution in the native client.

`just test` runs this module, against `zoe serve`. The acceptance test is
`test_native_conflicts.py`.
"""

import threading

import pytest
from support import commit_resolving as commit
from support import counter, native, value_of
from ZODB.utils import z64


@pytest.fixture
def storage(server):
    storage = native(server)
    yield storage
    storage.close()


def stale(storage, count=1):
    """Commit `count` counters twice. Return each oid with its first and its
    second serial."""
    oids = [storage.new_oid() for _ in range(count)]
    first, _ = commit(storage, [(oid, z64, counter(1)) for oid in oids])
    second, _ = commit(storage, [(oid, first, counter(2)) for oid in oids])
    return [(oid, first, second) for oid in oids]


def test_the_resolution_runs_on_the_thread_of_the_transaction(storage):
    [(oid, first, second)] = stale(storage)
    calls = []
    resolve = storage.tryToResolveConflict

    def recording(*args):
        calls.append((threading.get_ident(), args))
        return resolve(*args)

    storage.tryToResolveConflict = recording
    tid, resolved = commit(storage, [(oid, first, counter(5))])
    assert calls == [(threading.get_ident(), (oid, second, first, counter(5)))]
    assert resolved == [oid]
    # The counter adds both increments.
    data, serial = storage.load(oid)
    assert (value_of(data), serial) == (6, tid)


def test_the_vote_returns_each_resolved_oid_once(storage):
    stales = stale(storage, 2)
    stores = [(oid, first, counter(5)) for oid, first, _ in stales]
    _, resolved = commit(storage, stores)
    assert sorted(resolved) == sorted(oid for oid, _, _ in stales)


def test_an_error_of_the_resolution_leaves_the_transaction_to_the_abort(storage):
    from ZODB.Connection import TransactionMetaData

    [(oid, first, second)] = stale(storage)

    def failing(*args):
        raise ValueError("no resolution")

    storage.tryToResolveConflict = failing
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, first, counter(5), "", txn)
    with pytest.raises(ValueError):
        storage.tpc_vote(txn)
    # The transaction is still open: the abort releases its locks.
    assert storage.tpc_transaction() is txn
    storage.tpc_abort(txn)
    assert storage.tpc_transaction() is None
    del storage.tryToResolveConflict
    tid, _ = commit(storage, [(oid, second, counter(3))])
    assert storage.load(oid) == (counter(3), tid)


def test_a_resolution_that_is_not_bytes_raises_type_error(storage):
    [(oid, first, _)] = stale(storage)
    storage.tryToResolveConflict = lambda *args: "not bytes"
    with pytest.raises(TypeError):
        commit(storage, [(oid, first, counter(5))])
