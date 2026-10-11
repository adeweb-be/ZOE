"""Acceptance test of in-process conflict resolution.

At vote, `zoe.Storage` resolves a conflict on a class with
`_p_resolveConflict` in the same process, through ZODB's
ConflictResolvingStorage, and keeps its locks while it does so. BTree
buckets and `Length` merge as on FileStorage, and ZODB's ConflictResolution
suite passes. A conflict that no method resolves raises ConflictError with
the oid, the class and the serials.

Each test imports ZODB, BTrees, transaction and persistent itself, so that
the module collects before these packages are installed. The counters come
from ZODB's own tests, so that their class is importable by name.
"""

import contextlib
import threading
import unittest

import pytest

import zoe

# The tid and the serial of an object that has no record yet.
ZERO = bytes(8)
# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for another thread.
TIMEOUT = 30
# The time that the test gives a blocked commit to end, which it must not.
BLOCKED = 0.5


@contextlib.contextmanager
def opened(path, **options):
    """Open a storage in the directory path, and close it at the end."""
    storage = zoe.Storage(str(path), **options)
    try:
        yield storage
    finally:
        storage.close()


def counter(cls, amount):
    """Return the record of a counter of class cls that holds amount."""
    from ZODB.tests.StorageTestBase import zodb_pickle

    obj = cls()
    obj._value = amount
    return zodb_pickle(obj)


def value(data):
    """Return the value of the counter in a record."""
    from ZODB.tests.StorageTestBase import zodb_unpickle

    return zodb_unpickle(data)._value


def commit(storage, oid, serial, data):
    """Store one record in one transaction. Return the tid."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except Exception:
        storage.tpc_abort(txn)
        raise


def test_the_storage_is_a_conflict_resolving_storage(tmp_path):
    from ZODB.ConflictResolution import ConflictResolvingStorage

    with opened(tmp_path / "store") as storage:
        assert isinstance(storage, ConflictResolvingStorage)


def test_the_zodb_conflict_resolution_suite_passes(tmp_path):
    from ZODB.tests import ConflictResolution, StorageTestBase

    class ZoeConflictResolution(
        StorageTestBase.StorageTestBase,
        ConflictResolution.ConflictResolvingStorage,
    ):
        def setUp(self):
            super().setUp()
            self._storage = zoe.Storage(str(tmp_path / self._testMethodName))

        def test_resolve(self):
            # The suite runs only the unresolvable case of this check.
            self.checkResolve()

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ZoeConflictResolution)
    result = unittest.TestResult()
    suite.run(result)

    problems = [f"{test.id()}\n{trace}" for test, trace in result.failures]
    problems += [f"{test.id()}\n{trace}" for test, trace in result.errors]
    assert not problems, "\n".join(problems)
    assert result.testsRun == suite.countTestCases()
    assert result.testsRun >= 5


def test_a_vote_resolves_the_conflict_and_returns_the_oid(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.tests.ConflictResolution import PCounter

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, counter(PCounter, 0))
        commit(storage, oid, first, counter(PCounter, 1))

        # This transaction read the first revision and adds 2 to it.
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, first, counter(PCounter, 2), "", txn)
        resolved = storage.tpc_vote(txn)
        tid = storage.tpc_finish(txn)

        assert oid in (resolved or ())
        data, serial = tuple(storage.loadBefore(oid, MAX_TID))[:2]
        assert serial == tid
        assert value(data) == 3


def test_an_unresolved_conflict_raises_with_oid_class_and_serials(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ConflictError
    from ZODB.tests.ConflictResolution import PCounter2

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, counter(PCounter2, 0))
        second = commit(storage, oid, first, counter(PCounter2, 1))

        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        with pytest.raises(ConflictError) as error:
            storage.store(oid, first, counter(PCounter2, 2), "", txn)
            storage.tpc_vote(txn)
        storage.tpc_abort(txn)

        assert error.value.oid == oid
        assert error.value.class_name == "ZODB.tests.ConflictResolution.PCounter2"
        assert error.value.serials == (second, first)
        data, serial = tuple(storage.loadBefore(oid, MAX_TID))[:2]
        assert (value(data), serial) == (1, second)
        # The failed vote released its locks.
        commit(storage, oid, second, counter(PCounter2, 5))


def test_a_conflict_is_resolved_with_the_locks_held(tmp_path, monkeypatch):
    from ZODB.tests.ConflictResolution import PCounter

    # Stop each resolution until the test lets it go on.
    entered = threading.Event()
    go_on = threading.Event()
    resolve = PCounter._p_resolveConflict

    def stopped(self, old, committed, new):
        entered.set()
        assert go_on.wait(TIMEOUT)
        return resolve(self, old, committed, new)

    monkeypatch.setattr(PCounter, "_p_resolveConflict", stopped)

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, counter(PCounter, 0))
        second = commit(storage, oid, first, counter(PCounter, 1))
        tids = {}
        errors = []
        later_done = threading.Event()

        def stale():
            # Read the first revision and add 2: a conflict to resolve.
            try:
                tids["stale"] = commit(storage, oid, first, counter(PCounter, 2))
            except Exception as error:
                errors.append(error)

        def later():
            # Read the current revision and add 4: no conflict yet.
            try:
                tids["later"] = commit(storage, oid, second, counter(PCounter, 5))
            except Exception as error:
                errors.append(error)
            finally:
                later_done.set()

        stale_thread = threading.Thread(target=stale, daemon=True)
        stale_thread.start()
        assert entered.wait(TIMEOUT), "the vote did not resolve the conflict"

        later_thread = threading.Thread(target=later, daemon=True)
        later_thread.start()
        # The resolving vote keeps the lock on oid, so the later commit
        # cannot end.
        assert not later_done.wait(BLOCKED)

        go_on.set()
        stale_thread.join(TIMEOUT)
        later_thread.join(TIMEOUT)
        assert not stale_thread.is_alive() and not later_thread.is_alive()
        assert not errors

        assert tids["stale"] < tids["later"]
        data, serial = tuple(storage.loadBefore(oid, MAX_TID))[:2]
        assert serial == tids["later"]
        assert value(data) == 0 + 1 + 2 + 4


def bucket_and_length(storage):
    """Make two connections change one bucket and one Length at once.

    Return the outcome of each second commit, and the state at the end.
    """
    import transaction
    import ZODB
    from BTrees.Length import Length
    from BTrees.OOBTree import OOBTree
    from ZODB.POSException import ConflictError

    db = ZODB.DB(storage)
    try:
        with db.transaction() as conn:
            conn.root()["tree"] = OOBTree({"base": 0})
            conn.root()["length"] = Length(1)

        first_tm = transaction.TransactionManager()
        second_tm = transaction.TransactionManager()
        first = db.open(first_tm)
        second = db.open(second_tm)
        outcomes = []

        def race(first_change, second_change):
            first_tm.begin()
            second_tm.begin()
            first_change(first.root())
            second_change(second.root())
            first_tm.commit()
            try:
                second_tm.commit()
            except ConflictError:
                second_tm.abort()
                outcomes.append("conflict")
            else:
                outcomes.append("merged")

        def add(key, number):
            def change(root):
                root["tree"][key] = number
                root["length"].change(1)

            return change

        # Different keys in one bucket, and two changes of one Length.
        race(add("one", 1), add("two", 2))
        # Different values for one key.
        race(add("same", 1), add("same", 2))

        first_tm.begin()
        state = (dict(first.root()["tree"].items()), first.root()["length"]())
        first_tm.abort()
        first.close()
        second.close()
        return outcomes, state
    finally:
        db.close()


def test_buckets_and_length_merge_as_on_filestorage(tmp_path):
    from ZODB.FileStorage import FileStorage

    reference = bucket_and_length(FileStorage(str(tmp_path / "Data.fs")))
    assert reference == (
        ["merged", "conflict"],
        ({"base": 0, "one": 1, "two": 2, "same": 1}, 4),
    )
    assert bucket_and_length(zoe.Storage(str(tmp_path / "store"))) == reference
