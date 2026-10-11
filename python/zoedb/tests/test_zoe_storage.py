"""Unit tests of `zoe.Storage`.

ZODB's own storage test suites run first, then the tests of the parts that
ZODB does not test: the order of the `tpc_finish` callbacks, the read-only
mode, `close`, `pack` and the parallel commits without the GIL.
"""

import subprocess
import sys
import sysconfig
import textwrap
import threading
import time

import pytest
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
    ReadOnlyError,
    StorageError,
    StorageTransactionError,
    Unsupported,
)
from ZODB.tests import (
    BasicStorage,
    HistoryStorage,
    MTStorage,
    ReadOnlyStorage,
    RevisionStorage,
    StorageTestBase,
    Synchronization,
)
from ZODB.TimeStamp import TimeStamp

import zoe
import zoe.storage

ZERO = bytes(8)
FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
# The longest time that a test waits for another thread.
TIMEOUT = 30


class ZODBStorageTests(
    StorageTestBase.StorageTestBase,
    BasicStorage.BasicStorage,
    HistoryStorage.HistoryStorage,
    MTStorage.MTStorage,
    ReadOnlyStorage.ReadOnlyStorage,
    RevisionStorage.RevisionStorage,
    Synchronization.SynchronizedStorage,
):
    """The storage test suites of ZODB that apply to this storage."""

    def setUp(self):
        # The base class makes a temporary directory the current directory.
        StorageTestBase.StorageTestBase.setUp(self)
        self.open()

    def open(self, read_only=False):
        self._storage = zoe.Storage("store", read_only=read_only)

    def testLoadBeforeConsecutiveTids(self):
        pytest.skip("tpc_begin with a tid is a restore, not built yet")

    def testLoadBeforeUndo(self):
        pytest.skip("the storage does not undo")

    def testWriteMethods(self):
        pytest.skip("the storage does not undo; test_read_only covers the rest")

    def test_race_loadopen_vs_local_invalidate(self):
        if FREE_THREADED:
            # The cache of the stock persistent is not thread-safe on a
            # free-threaded build. ZODB's MappingStorage and FileStorage
            # fail this test there too, and can crash the interpreter.
            pytest.skip("the stock persistent is not thread-safe here")
        super().test_race_loadopen_vs_local_invalidate()


def record(value):
    """Return a small pickle that holds value."""
    return StorageTestBase.zodb_pickle(StorageTestBase.MinPO(value))


@pytest.fixture
def storage(tmp_path):
    opened = zoe.Storage(str(tmp_path / "store"))
    yield opened
    opened.close()


def voted(storage, oid, serial, data):
    """Begin a transaction, store one record and vote. Return the
    transaction."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return txn


def commit(storage, oid, serial, data):
    """Commit one record and return the tid."""
    return storage.tpc_finish(voted(storage, oid, serial, data))


def in_thread(func):
    """Start func in a daemon thread and return the thread."""
    thread = threading.Thread(target=func, daemon=True)
    thread.start()
    return thread


def test_wrong_thread_is_a_storage_transaction_error():
    assert issubclass(zoe.WrongThread, StorageTransactionError)
    assert zoe.WrongThread is zoe.storage.WrongThread
    assert zoe.Storage is zoe.storage.Storage


def test_pack_is_not_implemented(storage):
    with pytest.raises(NotImplementedError):
        storage.pack(time.time(), None)


def test_a_version_is_unsupported(storage):
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    with pytest.raises(Unsupported):
        storage.store(storage.new_oid(), ZERO, record(1), "version", txn)
    storage.tpc_abort(txn)


@pytest.mark.parametrize("lock_timeout", [-1.0, float("nan")])
def test_a_negative_lock_timeout_is_refused(tmp_path, lock_timeout):
    with pytest.raises(ValueError):
        zoe.Storage(str(tmp_path / "store"), lock_timeout=lock_timeout)


def test_history_gives_the_time_of_the_tid(storage):
    oid = storage.new_oid()
    tid = commit(storage, oid, ZERO, record(1))
    (entry,) = storage.history(oid)
    assert entry["time"] == TimeStamp(tid).timeTime()


def test_read_only(tmp_path):
    zoe.Storage(str(tmp_path / "store")).close()
    storage = zoe.Storage(str(tmp_path / "store"), read_only=True)
    try:
        txn = TransactionMetaData()
        for call in (
            storage.new_oid,
            lambda: storage.tpc_begin(txn),
            lambda: storage.store(ZERO, ZERO, record(1), "", txn),
        ):
            with pytest.raises(ReadOnlyError):
                call()
    finally:
        storage.close()


def test_close_aborts_the_open_transactions_and_releases_the_store(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    oid = storage.new_oid()
    voted(storage, oid, ZERO, record(1))
    storage.close()
    storage.close()
    for call in (storage.new_oid, storage.lastTransaction, lambda: storage.load(oid)):
        with pytest.raises(StorageError):
            call()

    # The directory is free, and the aborted transaction left no record.
    reopened = zoe.Storage(path)
    try:
        assert reopened.lastTransaction() == ZERO
        assert commit(reopened, oid, ZERO, record(2)) > ZERO
    finally:
        reopened.close()


def test_last_transaction_waits_for_the_callback(storage):
    oid = storage.new_oid()
    first = commit(storage, oid, ZERO, record(1))
    txn = voted(storage, oid, first, record(2))
    started, release = threading.Event(), threading.Event()
    seen = {}

    def callback(tid):
        # The callback sees the commit before it.
        seen["inside"] = storage.lastTransaction()
        seen["tid"] = tid
        started.set()
        release.wait(TIMEOUT)

    finisher = in_thread(lambda: storage.tpc_finish(txn, callback))
    assert started.wait(TIMEOUT)
    # The commit is visible, but ZODB cannot start a snapshot after it
    # before its callback ends.
    assert storage.load(oid) == (record(2), seen["tid"])
    reader = in_thread(lambda: seen.setdefault("after", storage.lastTransaction()))
    reader.join(0.2)
    assert reader.is_alive()
    release.set()
    finisher.join(TIMEOUT)
    reader.join(TIMEOUT)
    assert seen["inside"] == first
    assert seen["after"] == seen["tid"]


def test_callbacks_run_in_commit_order(storage):
    first, second = storage.new_oid(), storage.new_oid()
    one = voted(storage, first, ZERO, record(1))
    started, release = threading.Event(), threading.Event()
    order = []

    def slow(tid):
        started.set()
        release.wait(TIMEOUT)
        order.append(tid)

    finisher = in_thread(lambda: storage.tpc_finish(one, slow))
    assert started.wait(TIMEOUT)

    def commit_second():
        txn = voted(storage, second, ZERO, record(2))
        storage.tpc_finish(txn, order.append)

    # The second commit is durable, but its callback waits for the first.
    later = in_thread(commit_second)
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        try:
            storage.load(second)
            break
        except KeyError:
            time.sleep(0.01)
    later.join(0.2)
    assert order == []
    release.set()
    finisher.join(TIMEOUT)
    later.join(TIMEOUT)
    assert order == sorted(order) and len(order) == 2
    assert storage.lastTransaction() == order[1]


def test_a_failing_callback_publishes_the_commit(storage):
    oid = storage.new_oid()
    txn = voted(storage, oid, ZERO, record(1))

    def callback(tid):
        raise RuntimeError("callback")

    with pytest.raises(RuntimeError):
        storage.tpc_finish(txn, callback)
    tid = storage.lastTransaction()
    assert storage.load(oid) == (record(1), tid)
    assert commit(storage, oid, tid, record(2)) > tid


def test_parallel_commits_without_the_gil(tmp_path):
    # On a free-threaded build the child runs with the GIL off, even after
    # ZODB loads a module that did not declare that it runs without it.
    child = textwrap.dedent(
        """
        import sys
        import sysconfig
        import threading

        from ZODB.Connection import TransactionMetaData

        import zoe

        THREADS, COMMITS = 8, 25
        storage = zoe.Storage(sys.argv[1])
        oids = [[storage.new_oid() for _ in range(2)] for _ in range(THREADS)]
        barrier = threading.Barrier(THREADS)
        errors = []

        def work(mine):
            serials = [bytes(8)] * len(mine)
            barrier.wait()
            try:
                for n in range(COMMITS):
                    txn = TransactionMetaData()
                    storage.tpc_begin(txn)
                    for oid, serial in zip(mine, serials):
                        storage.store(oid, serial, b"x" + bytes([n]), "", txn)
                    storage.tpc_vote(txn)
                    tid = storage.tpc_finish(txn)
                    serials = [tid] * len(mine)
            except Exception as error:
                errors.append(error)

        threads = [threading.Thread(target=work, args=(mine,)) for mine in oids]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert not errors, errors
        for mine in oids:
            for oid in mine:
                assert storage.load(oid)[0] == b"x" + bytes([COMMITS - 1])
        storage.close()
        if sysconfig.get_config_var("Py_GIL_DISABLED"):
            assert not sys._is_gil_enabled()
        print("ok")
        """
    )
    flags = ["-X", "gil=0"] if FREE_THREADED else []
    result = subprocess.run(
        [sys.executable, *flags, "-c", child, str(tmp_path / "store")],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "ok"
