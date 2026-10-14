"""Acceptance test of several transactions at once in one client process.

The threads of one process commit at the same time through one
`zoe.zeo.ClientStorage` with the native client inside. Each transaction has
its own transaction on the server. The DB keeps the contract of ZODB for
invalidations. `just test` runs this module.

- Thread A begins a transaction, stores an object and votes, then waits.
  Thread B commits another object through the same storage. The
  `tpc_finish` of B returns while A waits. With one transaction at a time
  per client, B cannot begin before A ends.
- 16 threads commit 200 transactions each through one DB, each on its own
  two objects. A 17th thread checks its snapshot after each new
  transaction, with the model idea of `ZODB.tests.racetest`: the two
  objects of a writer hold the same value, and the sum of the counters is
  the number of commits that the snapshot can see. The checker counts these
  commits from the tids that the writers record.
- `lastTransaction()` never gives a tid before the DB got its
  invalidations: the invalidations of the other clients, and the callback
  of `tpc_finish` for the commits of the process. The DB gets the
  invalidations of the other client once each, in tid order, and never
  gets a commit of the process as an invalidation.

The tests open the native client with `zoe.testing.native_client`. Each
test imports ZEO and ZODB's test modules itself.
"""

import pickle
import threading
import time

import pytest

import zoe.testing
from zoe.testing import ZoeServer

# The longest time that a test waits for the server or for one step of a
# thread.
TIMEOUT = 30
# The longest time that a test waits for all its threads to end.
RUN_TIMEOUT = 300
# The serial of an object that has no record yet.
ZERO = bytes(8)
# The writers of the model test, and the commits of each.
WRITERS = 16
COMMITS = 200
# The threads of the process that commit in the lastTransaction test, and
# the commits of each thread and of the other client.
OWN_WRITERS = 4
ROUNDS = 50


def native(address, **kw):
    """Open the native client on the storage 1 of address."""
    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.testing.native_client(address, storage="1", **kw)


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def commit(storage, stores, f=None):
    """Commit (oid, serial, data) items through storage. Return the tid.

    `f` is the callback of `tpc_finish`.
    """
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        if f is None:
            return storage.tpc_finish(txn)
        return storage.tpc_finish(txn, f)
    except BaseException:
        storage.tpc_abort(txn)
        raise


def wait_for(condition, what):
    """Poll condition until it is true. Fail after TIMEOUT seconds."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} did not happen in {TIMEOUT} seconds")
        time.sleep(0.01)


def start(target, errors, *args):
    """Start a thread that runs target. Its exception goes to errors."""

    def run():
        try:
            target(*args)
        except BaseException as error:
            errors.append(error)

    thread = threading.Thread(target=run, daemon=True)
    thread.start()
    return thread


def join(threads, timeout):
    """Join threads. Fail when one runs after timeout seconds."""
    deadline = time.monotonic() + timeout
    for thread in threads:
        thread.join(max(0.0, deadline - time.monotonic()))
    alive = [thread.name for thread in threads if thread.is_alive()]
    assert not alive, f"threads still run after {timeout} seconds: {alive}"


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


def test_a_transaction_commits_while_another_holds_its_vote(server):
    storage = native(server.address)
    try:
        first, second = storage.new_oid(), storage.new_oid()
        voted = threading.Event()
        release = threading.Event()
        finished = threading.Event()
        errors = []

        def hold_the_vote():
            from ZODB.Connection import TransactionMetaData

            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            try:
                storage.store(first, ZERO, record("a"), "", txn)
                storage.tpc_vote(txn)
                voted.set()
                release.wait(TIMEOUT)
                storage.tpc_finish(txn)
            except BaseException:
                storage.tpc_abort(txn)
                raise

        def commit_another():
            commit(storage, [(second, ZERO, record("b"))])
            finished.set()

        holder = start(hold_the_vote, errors)
        try:
            assert voted.wait(TIMEOUT), errors
            committer = start(commit_another, errors)
            # Thread A waits for release, so it cannot finish before this.
            finished_first = finished.wait(TIMEOUT)
        finally:
            release.set()
        join([holder, committer], RUN_TIMEOUT)
        assert not errors
        assert finished_first, "the second transaction waited for the first"
        assert storage.load(first)[0] == record("a")
        assert storage.load(second)[0] == record("b")
    finally:
        storage.close()


class Progress:
    """The tids that each writer committed, in commit order.

    A writer records each tid after its commit. Its condition wakes the
    checker.
    """

    def __init__(self, writers):
        self.condition = threading.Condition()
        self.tids = [[] for _ in range(writers)]
        self.done = [False] * writers

    def committed(self, index, tid):
        with self.condition:
            self.tids[index].append(tid)
            self.condition.notify_all()

    def finished(self, index):
        with self.condition:
            self.done[index] = True
            self.condition.notify_all()

    def _count(self):
        return sum(len(tids) for tids in self.tids)

    def wait_for_more(self, seen):
        """Wait for a commit after the first `seen` ones, or for the end.

        Return the number of commits and whether every writer ended.
        """
        with self.condition:
            moved = self.condition.wait_for(
                lambda: self._count() > seen or all(self.done), TIMEOUT
            )
            assert moved, f"no commit in {TIMEOUT} seconds"
            return self._count(), all(self.done)

    def commits_up_to(self, at):
        """Return the number of commits with a tid up to at.

        The writers commit one after the other, so their tids increase. The
        method waits until each writer recorded a tid after at, or ended.
        Then the count holds every commit up to at.
        """

        def complete():
            return all(
                done or (tids and tids[-1] > at)
                for tids, done in zip(self.tids, self.done, strict=True)
            )

        with self.condition:
            assert self.condition.wait_for(complete, TIMEOUT), "a writer stopped"
            return sum(tid <= at for tids in self.tids for tid in tids)


def check_snapshot(root, progress):
    """Check the invariant of the model in the snapshot of root.

    As `ZODB.tests.racetest` does, the check loads half of the objects from
    the storage and takes the other half from the cache of the connection.
    """
    keys = sorted(root.keys())
    for key in keys[: len(keys) // 2]:
        root[key]._p_invalidate()
    lefts = [root[f"left-{index}"] for index in range(WRITERS)]
    rights = [root[f"right-{index}"] for index in range(WRITERS)]
    for index, (left, right) in enumerate(zip(lefts, rights, strict=True)):
        assert left.value == right.value, (
            f"writer {index}: left {left.value}, right {right.value}"
        )
        assert left._p_serial == right._p_serial, f"writer {index}"
    # The newest commit that the snapshot sees wrote one of the objects.
    at = max(obj._p_serial for obj in lefts + rights)
    visible = progress.commits_up_to(at)
    total = sum(left.value for left in lefts)
    assert total == visible, (
        f"the snapshot at {at.hex()} holds {total} of {visible} commits"
    )


def test_threads_commit_while_a_checker_sees_whole_snapshots(server):
    import transaction
    import ZODB
    from ZODB.tests.MinPO import MinPO

    db = ZODB.DB(native(server.address), pool_size=WRITERS + 2)
    try:
        with db.transaction() as connection:
            root = connection.root()
            for index in range(WRITERS):
                root[f"left-{index}"] = MinPO(0)
                root[f"right-{index}"] = MinPO(0)

        progress = Progress(WRITERS)
        barrier = threading.Barrier(WRITERS, timeout=TIMEOUT)
        errors = []
        checks = []

        def write(index):
            tm = transaction.TransactionManager()
            connection = db.open(tm)
            try:
                barrier.wait()
                for _ in range(COMMITS):
                    tm.begin()
                    root = connection.root()
                    left, right = root[f"left-{index}"], root[f"right-{index}"]
                    left.value += 1
                    right.value += 1
                    tm.commit()
                    progress.committed(index, left._p_serial)
            finally:
                tm.abort()
                connection.close()
                progress.finished(index)

        def check():
            tm = transaction.TransactionManager()
            connection = db.open(tm)
            try:
                seen, ended = -1, False
                while not ended:
                    seen, ended = progress.wait_for_more(seen)
                    tm.begin()
                    check_snapshot(connection.root(), progress)
                    tm.abort()
                    checks.append(seen)
            finally:
                tm.abort()
                connection.close()

        threads = [start(write, errors, index) for index in range(WRITERS)]
        threads.append(start(check, errors))
        join(threads, RUN_TIMEOUT)
        assert not errors, errors
        assert checks

        tm = transaction.TransactionManager()
        connection = db.open(tm)
        try:
            tm.begin()
            root = connection.root()
            for index in range(WRITERS):
                assert root[f"left-{index}"].value == COMMITS
                assert root[f"right-{index}"].value == COMMITS
        finally:
            tm.abort()
            connection.close()
    finally:
        db.close()


class RecordingDB:
    """A DB for `registerDB`. It records the tids that reach it."""

    def __init__(self):
        self._lock = threading.Lock()
        self.invalidated = []
        self.finished = []
        self._tids = set()

    def invalidate(self, tid, oids, *args, **kw):
        with self._lock:
            self.invalidated.append(tid)
            self._tids.add(tid)

    def invalidateCache(self):
        pass

    def finish(self, tid):
        """Record a commit of the process: the callback of `tpc_finish`."""
        with self._lock:
            self.finished.append(tid)
            self._tids.add(tid)

    def got(self, tid):
        with self._lock:
            return tid in self._tids

    @staticmethod
    def transform_record_data(data):
        return data

    @staticmethod
    def untransform_record_data(data):
        return data


def test_last_transaction_never_gives_a_tid_before_the_db_got_it(server):
    storage = native(server.address)
    try:
        other = native(server.address)
        try:
            db = RecordingDB()
            storage.registerDB(db)
            opened = storage.lastTransaction()
            own_tids, other_tids, errors = [], [], []

            def write(client, tids, f):
                oid = client.new_oid()
                serial = ZERO
                for round_ in range(ROUNDS):
                    serial = commit(client, [(oid, serial, record(round_))], f)
                    tids.append(serial)

            threads = [
                start(write, errors, storage, own_tids, db.finish)
                for _ in range(OWN_WRITERS)
            ]
            threads += [start(write, errors, other, other_tids, None) for _ in range(2)]
            early = []
            while any(thread.is_alive() for thread in threads):
                tid = storage.lastTransaction()
                if tid > opened and not db.got(tid):
                    early.append(tid)
                time.sleep(0)
            join(threads, RUN_TIMEOUT)
            assert not errors, errors
            assert not early, [tid.hex() for tid in early]

            last = max(other_tids)
            wait_for(
                lambda: storage.lastTransaction() >= last,
                "the client got the last commit of the other client",
            )
            # Each commit of the other client reaches the DB once, in tid
            # order. No commit of the process reaches it as an invalidation.
            assert db.invalidated == sorted(other_tids)
            assert sorted(db.finished) == sorted(own_tids)
            assert len(own_tids) == OWN_WRITERS * ROUNDS
        finally:
            other.close()
    finally:
        storage.close()
