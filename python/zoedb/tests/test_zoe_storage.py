"""Unit tests of `zoe.Storage`.

ZODB's own storage test suites run first, then the tests of the parts that
ZODB does not test: the order of the `tpc_finish` callbacks, the read-only
mode, `close`, `pack`, the conflict resolution and the parallel commits
without the GIL.
"""

import io
import pickle
import subprocess
import sys
import sysconfig
import textwrap
import threading
import time

import pytest
from persistent.mapping import PersistentMapping
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
    ConflictError,
    POSKeyError,
    ReadConflictError,
    ReadOnlyError,
    StorageError,
    StorageSystemError,
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
from ZODB.tests.ConflictResolution import PCounter, PCounter2
from ZODB.TimeStamp import TimeStamp
from ZODB.utils import u64

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
            # free-threaded build. A heavier stress of this pattern fails
            # with a SystemError in ZODB.serialize or a spurious
            # AttributeError, on zoe.Storage and on FileStorage, and
            # MappingStorage can crash the interpreter.
            pytest.skip("the stock persistent is not thread-safe here")
        super().test_race_loadopen_vs_local_invalidate()


def record(value):
    """Return a small pickle that holds value."""
    return StorageTestBase.zodb_pickle(StorageTestBase.MinPO(value))


def counter(amount, cls=PCounter):
    """Return the pickle of a counter of class cls that holds amount."""
    obj = cls()
    obj._value = amount
    return StorageTestBase.zodb_pickle(obj)


def amount(data):
    """Return the amount of the counter in a pickle."""
    return StorageTestBase.zodb_unpickle(data)._value


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


def test_a_read_only_storage_creates_no_directory(tmp_path):
    with pytest.raises(FileNotFoundError):
        zoe.Storage(str(tmp_path / "missing"), read_only=True)
    assert not (tmp_path / "missing").exists()


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


def test_the_lock_table_is_empty_after_finish_and_abort(storage):
    assert storage._lock_table_is_empty()
    oid = storage.new_oid()
    # The vote holds the lock of the object until the end of the transaction.
    txn = voted(storage, oid, ZERO, record(1))
    assert not storage._lock_table_is_empty()
    tid = storage.tpc_finish(txn)
    assert storage._lock_table_is_empty()
    txn = voted(storage, oid, tid, record(2))
    assert not storage._lock_table_is_empty()
    storage.tpc_abort(txn)
    assert storage._lock_table_is_empty()
    storage.close()
    with pytest.raises(StorageError):
        storage._lock_table_is_empty()


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


def test_a_conflict_names_the_class_of_the_object(storage):
    oid = storage.new_oid()
    first = commit(storage, oid, ZERO, record(1))
    second = commit(storage, oid, first, record(2))
    for data, class_name in [
        (record(3), "ZODB.tests.MinPO.MinPO"),
        # ZODB cannot read a class from this pickle. The error comes all the
        # same, without the class name.
        (b"c\x00", None),
    ]:
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, first, data, "", txn)
        with pytest.raises(ConflictError) as error:
            storage.tpc_vote(txn)
        storage.tpc_abort(txn)
        assert error.value.oid == oid
        assert error.value.serials == (second, first)
        assert error.value.class_name == class_name


def test_a_callback_cannot_commit_in_its_own_storage(storage):
    first, second = storage.new_oid(), storage.new_oid()
    txn = voted(storage, first, ZERO, record(1))
    raised = []

    def callback(tid):
        inner = voted(storage, second, ZERO, record(2))
        try:
            storage.tpc_finish(inner)
        except StorageTransactionError as error:
            raised.append(error)
        storage.tpc_abort(inner)

    tid = storage.tpc_finish(txn, callback)
    assert len(raised) == 1
    assert storage.lastTransaction() == tid
    with pytest.raises(KeyError):
        storage.load(second)
    assert commit(storage, second, ZERO, record(2)) > tid


def test_a_transaction_that_begins_during_close_does_not_keep_the_store(tmp_path):
    path = str(tmp_path / "store")
    for _ in range(20):
        storage = zoe.Storage(path)
        started = threading.Event()
        outcome = []

        def begin_until_closed(storage=storage, started=started, outcome=outcome):
            try:
                while True:
                    storage.tpc_begin(TransactionMetaData())
                    started.set()
            except StorageError as error:
                outcome.append(error)

        thread = in_thread(begin_until_closed)
        assert started.wait(TIMEOUT)
        storage.close()
        thread.join(TIMEOUT)
        assert len(outcome) == 1
        # No transaction keeps the store open, so it opens again at once.
        zoe.Storage(path).close()


def test_close_does_not_wait_for_a_vote_on_an_open_transaction(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"), lock_timeout=TIMEOUT)
    oid = storage.new_oid()
    voted(storage, oid, ZERO, record(1))
    waiting = threading.Event()
    outcome = []

    def vote_behind():
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, ZERO, record(2), "", txn)
        waiting.set()
        try:
            storage.tpc_vote(txn)
            outcome.append("voted")
        except Exception as error:
            outcome.append(error)

    thread = in_thread(vote_behind)
    assert waiting.wait(TIMEOUT)
    time.sleep(0.2)
    start = time.monotonic()
    # The close aborts the transaction that holds the lock first, so the
    # waiting vote ends at once.
    storage.close()
    assert time.monotonic() - start < TIMEOUT / 3
    thread.join(TIMEOUT)
    assert outcome == ["voted"]
    zoe.Storage(str(tmp_path / "store")).close()


def test_publication_under_stress(storage):
    # Writers commit, abort after the vote or fail in their callback.
    # Readers check that lastTransaction never goes back and gives only a
    # tid whose callback ran.
    writers, rounds = 4, 30
    called, lock = [], threading.Lock()
    stop = threading.Event()
    problems = []
    first = storage.lastTransaction()

    def write(n):
        oid = storage.new_oid()
        serial = ZERO
        for i in range(rounds):
            txn = voted(storage, oid, serial, record(i))
            if (n + i) % 5 == 0:
                storage.tpc_abort(txn)
                continue

            def callback(tid, fail=(n + i) % 3 == 0):
                with lock:
                    called.append(tid)
                if fail:
                    raise RuntimeError("callback")

            try:
                serial = storage.tpc_finish(txn, callback)
            except RuntimeError:
                serial = storage.load(oid)[1]

    def read():
        last = first
        while not stop.is_set():
            tid = storage.lastTransaction()
            with lock:
                seen = tid == first or tid in called
            if tid < last or not seen:
                problems.append((last, tid))
            last = tid

    readers = [in_thread(read) for _ in range(2)]
    threads = [in_thread(lambda n=n: write(n)) for n in range(writers)]
    for thread in threads:
        thread.join(TIMEOUT * 4)
    stop.set()
    for thread in readers:
        thread.join(TIMEOUT)
    assert not problems
    # The callbacks ran in commit order, and the last one is published.
    assert called == sorted(called) and len(set(called)) == len(called)
    assert storage.lastTransaction() == called[-1]


def test_no_lock_is_held_while_an_error_is_built(tmp_path):
    # The first error of a fresh interpreter imports ZODB. Threads that
    # meet errors at once must not wait for each other forever.
    child = textwrap.dedent(
        """
        import sys
        import threading

        from zoe import _zoe

        storage = _zoe.Storage(sys.argv[1])
        assert "ZODB" not in sys.modules

        def fail(call):
            for _ in range(20):
                try:
                    call()
                except Exception:
                    pass

        def run(call):
            threads = [
                threading.Thread(target=fail, args=(call,)) for _ in range(8)
            ]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()

        run(lambda: storage.tpc_vote(object()))
        storage.close()
        run(storage.new_oid)
        print("ok")
        """
    )
    result = subprocess.run(
        [sys.executable, "-c", child, str(tmp_path / "store")],
        capture_output=True,
        text=True,
        timeout=120,
    )
    assert result.returncode == 0, result.stderr
    assert result.stdout.strip() == "ok"


def two_revisions(storage, data=counter):
    """Commit two revisions of a new object. Return the oid and the tids."""
    oid = storage.new_oid()
    first = commit(storage, oid, ZERO, data(0))
    second = commit(storage, oid, first, data(1))
    return oid, first, second


def test_a_vote_without_conflict_resolves_nothing(storage):
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, ZERO, counter(0), "", txn)
    assert storage.tpc_vote(txn) == []
    storage.tpc_finish(txn)


def test_a_read_current_conflict_is_never_resolved(storage, monkeypatch):
    def resolve(self, old, committed, new):
        raise AssertionError("a read-current object is never resolved")

    monkeypatch.setattr(PCounter, "_p_resolveConflict", resolve)
    oid, first, second = two_revisions(storage)
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.checkCurrentSerialInTransaction(oid, first, txn)
    storage.store(storage.new_oid(), ZERO, counter(5), "", txn)
    with pytest.raises(ReadConflictError) as error:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert error.value.oid == oid
    assert error.value.serials == (second, first)


def test_a_class_without_a_resolve_method_raises_with_its_name(storage):
    oid, first, second = two_revisions(storage, record)
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, first, record(2), "", txn)
    with pytest.raises(ConflictError) as error:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert error.value.oid == oid
    assert error.value.class_name == "ZODB.tests.MinPO.MinPO"
    assert error.value.serials == (second, first)


def test_a_new_object_over_a_committed_one_is_not_resolved(storage, monkeypatch):
    def resolve(self, old, committed, new):
        raise AssertionError("an object without an old state is never resolved")

    monkeypatch.setattr(PCounter, "_p_resolveConflict", resolve)
    oid, first, second = two_revisions(storage)
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, ZERO, counter(2), "", txn)
    with pytest.raises(ConflictError) as error:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert error.value.oid == oid
    assert error.value.serials == (second, ZERO)
    assert error.value.class_name == "ZODB.tests.ConflictResolution.PCounter"


def test_a_serial_of_a_missing_record_is_not_resolved(storage, monkeypatch):
    def resolve(self, old, committed, new):
        raise AssertionError("an object without a committed state is never resolved")

    monkeypatch.setattr(PCounter, "_p_resolveConflict", resolve)
    oid, first, second = two_revisions(storage)
    missing = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(missing, first, counter(2), "", txn)
    with pytest.raises(ConflictError) as error:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert error.value.oid == missing
    assert error.value.serials == (ZERO, first)
    assert error.value.class_name == "ZODB.tests.ConflictResolution.PCounter"


def test_an_unresolved_conflict_releases_the_locks_before_the_abort(tmp_path):
    # With no wait for a lock, a lock that the failed vote kept fails the
    # next commit.
    storage = zoe.Storage(str(tmp_path / "store"), lock_timeout=0)
    try:
        oid, first, second = two_revisions(
            storage, lambda value: counter(value, PCounter2)
        )
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, first, counter(2, PCounter2), "", txn)
        with pytest.raises(ConflictError):
            storage.tpc_vote(txn)
        assert commit(storage, oid, second, counter(3, PCounter2)) > second
        storage.tpc_abort(txn)
    finally:
        storage.close()


class Marked:
    """A database that marks each record with a prefix."""

    def transform_record_data(self, data):
        return b"marked:" + data

    def untransform_record_data(self, data):
        assert data.startswith(b"marked:")
        return data.removeprefix(b"marked:")


def test_a_conflict_names_the_class_through_the_record_transforms(storage):
    storage.registerDB(Marked())
    mark = Marked().transform_record_data
    oid, first, second = two_revisions(storage, lambda value: mark(counter(value)))

    def vote_error(serial, data):
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, serial, data, "", txn)
        with pytest.raises(ConflictError) as error:
            storage.tpc_vote(txn)
        storage.tpc_abort(txn)
        return error.value

    # The core does not resolve a new object over a committed one. A
    # PCounter2 does not resolve.
    for serial in (ZERO, first):
        error = vote_error(serial, mark(counter(2, PCounter2)))
        assert error.class_name == "ZODB.tests.ConflictResolution.PCounter2"
    # A record that the database cannot untransform gives no class name.
    assert vote_error(ZERO, counter(2, PCounter2)).class_name is None


def test_the_resolution_uses_the_record_transforms_of_the_database(storage):
    storage.registerDB(Marked())
    mark = Marked().transform_record_data
    oid, first, second = two_revisions(storage, lambda value: mark(counter(value)))
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, first, mark(counter(2)), "", txn)
    assert storage.tpc_vote(txn) == [oid]
    tid = storage.tpc_finish(txn)
    data, serial = storage.load(oid)
    assert serial == tid
    assert amount(Marked().untransform_record_data(data)) == 3


@pytest.fixture
def stopped(monkeypatch):
    """Stop each resolution of a PCounter until the test lets it go on.

    Return the event that a resolution sets when it starts, and the event
    that lets it go on.
    """
    entered, go_on = threading.Event(), threading.Event()
    resolve = PCounter._p_resolveConflict

    def stopped_resolve(self, old, committed, new):
        entered.set()
        assert go_on.wait(TIMEOUT)
        return resolve(self, old, committed, new)

    monkeypatch.setattr(PCounter, "_p_resolveConflict", stopped_resolve)
    yield entered, go_on
    go_on.set()


def resolving(storage, writes, read_current=()):
    """Commit in a new thread a transaction that stores writes.

    Each write is an oid, the serial that the transaction read and a pickle.
    Each read-current entry is an oid and its serial. Return the thread, the
    transaction and a list that gets the tid of the commit, or the error of
    the vote.
    """
    txn = TransactionMetaData()
    outcome = []

    def run():
        storage.tpc_begin(txn)
        for oid, serial in read_current:
            storage.checkCurrentSerialInTransaction(oid, serial, txn)
        for oid, serial, data in writes:
            storage.store(oid, serial, data, "", txn)
        try:
            storage.tpc_vote(txn)
            outcome.append(storage.tpc_finish(txn))
        except Exception as error:
            outcome.append(error)
            storage.tpc_abort(txn)

    return in_thread(run), txn, outcome


def test_a_partial_resolution_commits_nothing(tmp_path):
    # With no wait for a lock, a lock that the failed vote kept fails the
    # next commit.
    storage = zoe.Storage(str(tmp_path / "store"), lock_timeout=0)
    try:
        low, first_low, second_low = two_revisions(storage)
        high, first_high, second_high = two_revisions(
            storage, lambda value: counter(value, PCounter2)
        )
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(low, first_low, counter(2), "", txn)
        storage.store(high, first_high, counter(2, PCounter2), "", txn)
        with pytest.raises(ConflictError) as error:
            storage.tpc_vote(txn)
        assert error.value.oid == high
        assert error.value.class_name == "ZODB.tests.ConflictResolution.PCounter2"
        # The resolved state of low is not committed, and both locks are
        # free before the abort.
        assert storage.load(low) == (counter(1), second_low)
        assert storage.load(high) == (counter(1, PCounter2), second_high)
        assert commit(storage, low, second_low, counter(3)) > second_high
        assert commit(storage, high, second_high, counter(3, PCounter2)) > second_high
        storage.tpc_abort(txn)
    finally:
        storage.close()


def test_an_abort_from_another_thread_during_the_resolution(tmp_path, stopped):
    entered, go_on = stopped
    storage = zoe.Storage(str(tmp_path / "store"), lock_timeout=0)
    try:
        oid, first, second = two_revisions(storage)
        thread, txn, outcome = resolving(storage, [(oid, first, counter(2))])
        assert entered.wait(TIMEOUT)
        storage.tpc_abort(txn)
        # The abort released the lock while the resolution runs.
        later = commit(storage, oid, second, counter(5))
        go_on.set()
        thread.join(TIMEOUT)
        assert isinstance(outcome[0], StorageTransactionError)
        assert storage.load(oid) == (counter(5), later)
    finally:
        storage.close()


def test_a_close_during_the_resolution(tmp_path, stopped):
    entered, go_on = stopped
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    try:
        oid, first, second = two_revisions(storage)
        thread, _, outcome = resolving(storage, [(oid, first, counter(2))])
        assert entered.wait(TIMEOUT)
        storage.close()
        # The close released the store while the resolution runs.
        reopened = zoe.Storage(path)
        try:
            assert reopened.load(oid) == (counter(1), second)
        finally:
            reopened.close()
        go_on.set()
        thread.join(TIMEOUT)
        assert isinstance(outcome[0], StorageError)
    finally:
        storage.close()


def test_a_read_current_lock_stays_held_during_the_resolution(tmp_path, stopped):
    entered, go_on = stopped
    storage = zoe.Storage(str(tmp_path / "store"), lock_timeout=0)
    try:
        oid, first, second = two_revisions(storage)
        read = storage.new_oid()
        seen = commit(storage, read, ZERO, counter(0))
        thread, _, outcome = resolving(
            storage, [(oid, first, counter(2))], read_current=[(read, seen)]
        )
        assert entered.wait(TIMEOUT)
        # The resolving vote holds a shared lock on the read-current oid, so
        # a writer cannot take it.
        with pytest.raises(ConflictError) as error:
            commit(storage, read, seen, counter(1))
        assert error.value.oid == read
        assert error.value.serials is None
        go_on.set()
        thread.join(TIMEOUT)
        (tid,) = outcome
        assert amount(storage.load(oid)[0]) == 3
        assert commit(storage, read, seen, counter(1)) > tid
    finally:
        storage.close()


class Interrupt(BaseException):
    """An exception that can come at any time, as KeyboardInterrupt does."""


class Interrupted(zoe.storage.Storage):
    """A storage whose store raises Interrupt after it stores, on request."""

    interrupt = False

    def store(self, *args):
        super().store(*args)
        if self.interrupt:
            raise Interrupt


def test_an_error_after_the_last_resolved_store_releases_the_locks(tmp_path):
    storage = Interrupted(str(tmp_path / "store"), lock_timeout=0)
    try:
        oid, first, second = two_revisions(storage)
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, first, counter(2), "", txn)
        storage.interrupt = True
        with pytest.raises(Interrupt):
            storage.tpc_vote(txn)
        storage.interrupt = False
        # The transaction is not voted: it cannot finish, and its lock is
        # free.
        with pytest.raises(StorageTransactionError):
            storage.tpc_finish(txn)
        assert commit(storage, oid, second, counter(5)) > second
    finally:
        storage.close()


# The default staging-memory of the core. A transaction keeps this many
# bytes of pickles in memory and spills the others to `staging/`.
STAGING_MEMORY = 16 << 20


def test_a_damaged_spill_file_fails_the_resolution(tmp_path):
    path = tmp_path / "store"
    storage = zoe.Storage(str(path), lock_timeout=0)
    try:
        oid, first, second = two_revisions(storage)
        filler = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        # The filler fills the memory part, so the next pickle spills.
        storage.store(filler, ZERO, b"x" * STAGING_MEMORY, "", txn)
        storage.store(oid, first, counter(2), "", txn)
        (spill,) = (path / "staging").iterdir()
        with spill.open("r+b") as file:
            file.write(b"\xff")
        # The error is not a conflict, so ZODB does not retry it. Its cause
        # is the error of the read.
        with pytest.raises(StorageError, match="does not match its hash") as raised:
            storage.tpc_vote(txn)
        assert isinstance(raised.value.__cause__, OSError)
        # The transaction is aborted: its lock is free, and its spill file
        # is gone.
        assert commit(storage, oid, second, counter(3)) > second
        assert list((path / "staging").iterdir()) == []
        storage.tpc_abort(txn)
    finally:
        storage.close()


class Ref:
    """A reference to the object `oid` in a record."""

    def __init__(self, oid):
        self.oid = oid


class RecordPickler(pickle.Pickler):
    """A pickler that writes each Ref as a persistent reference."""

    def persistent_id(self, obj):
        return (obj.oid, None) if isinstance(obj, Ref) else None


def linking(*oids):
    """Return a record of a persistent mapping that refers to each oid."""
    file = io.BytesIO()
    pickler = RecordPickler(file, 3)
    pickler.dump((PersistentMapping, None))
    pickler.dump({"data": {str(n): Ref(oid) for n, oid in enumerate(oids)}})
    return file.getvalue()


def commit_all(storage, writes):
    """Commit writes in one transaction and return the tid.

    Each write is an oid, the serial that the transaction read and a pickle.
    """
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    for oid, serial, data in writes:
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def past(tid):
    """Wait until the clock is past the time of tid. Return the time then."""
    instant = TimeStamp(tid).timeTime()
    while time.time() <= instant + 0.01:
        time.sleep(0.001)
    return time.time()


def present(storage, oid, tid):
    """Tell if the store holds the record of oid at tid."""
    try:
        storage.loadSerial(oid, tid)
    except POSKeyError:
        return False
    return True


def test_a_closed_or_read_only_storage_refuses_to_pack(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    # No object refers to oid, so a pack removes it.
    oid = storage.new_oid()
    tid = commit(storage, oid, ZERO, record(1))
    storage.close()
    pack_time = past(tid)
    with pytest.raises(StorageError, match="closed"):
        storage.pack(pack_time, None)
    read_only = zoe.Storage(path, read_only=True)
    try:
        with pytest.raises(ReadOnlyError):
            read_only.pack(pack_time, None)
        assert present(read_only, oid, tid)
    finally:
        read_only.close()

    storage = zoe.Storage(path)
    try:
        storage.pack(pack_time, None)
        assert not present(storage, oid, tid)
    finally:
        storage.close()


def test_a_pack_before_the_first_transaction_removes_nothing(storage):
    oid = storage.new_oid()
    first = commit_all(storage, [(ZERO, ZERO, linking(oid)), (oid, ZERO, record(1))])
    second = commit_all(storage, [(ZERO, first, linking()), (oid, first, record(2))])
    records = [(ZERO, first), (ZERO, second), (oid, first), (oid, second)]
    for pack_time in (0, TimeStamp(first).timeTime() - 1):
        storage.pack(pack_time, None)
        assert all(present(storage, *key) for key in records), pack_time

    # Such a pack does not stop a later pack. The root no longer refers to
    # oid at the second commit, so oid goes.
    storage.pack(past(second), None)
    assert [present(storage, *key) for key in records] == [False, True, False, False]


def test_a_pack_while_another_thread_commits(storage):
    kept, dropped = storage.new_oid(), storage.new_oid()
    first = commit_all(
        storage,
        [
            (ZERO, ZERO, linking(kept)),
            (kept, ZERO, record(1)),
            (dropped, ZERO, record(1)),
        ],
    )
    is_voted, finish, called, release = (threading.Event() for _ in range(4))
    # The writer notes if each of its waits ended before the timeout. A
    # pack that waits for the writer makes a wait time out.
    tids, waits = [], []

    def callback(tid):
        tids.append(tid)
        called.set()
        waits.append(release.wait(TIMEOUT))

    def write():
        txn = voted(storage, kept, first, record(2))
        is_voted.set()
        waits.append(finish.wait(TIMEOUT))
        storage.tpc_finish(txn, callback)

    writer = in_thread(write)
    try:
        assert is_voted.wait(TIMEOUT)
        # The vote holds the lock of kept. The pack takes no lock of an
        # object, so it does not wait for the commit.
        storage.pack(past(first), None)
        assert not present(storage, dropped, first)
        assert present(storage, kept, first)

        finish.set()
        assert called.wait(TIMEOUT)
        # The commit is durable and its callback runs. The pack does not
        # wait for the callback.
        (second,) = tids
        storage.pack(past(second), None)
        assert not present(storage, kept, first)
        assert storage.loadSerial(kept, second) == record(2)
    finally:
        finish.set()
        release.set()
        writer.join(TIMEOUT)
    assert not writer.is_alive()
    assert waits == [True, True]
    assert storage.lastTransaction() == second
    assert storage.load(kept) == (record(2), second)


def test_an_unreadable_record_stops_the_pack(storage):
    bad = storage.new_oid()
    first = commit(storage, ZERO, ZERO, linking())
    second = commit_all(
        storage, [(ZERO, first, linking(bad)), (bad, ZERO, b"\xffnot a pickle")]
    )
    with pytest.raises(StorageSystemError) as error:
        storage.pack(past(second), None)
    text = str(error.value)
    assert text.startswith("pack stopped: ")
    assert f"oid Oid({u64(bad)}), tid Tid({u64(second)})" in text
    assert "opcode 0xff" in text
    # The pack stopped before it removed anything.
    assert present(storage, ZERO, first)
