"""Acceptance test of pack through `zoe.Storage`.

`ZODB.DB(zoe.Storage(path)).pack(days=...)` and `storage.pack(t, referencesf)`
run the online pack of the store, with garbage collection. A pack at the
time t keeps every record that a reader at t or later can reach, and
removes the other records with their blob files. The store finds the
references in the pickles itself, so the pack ignores `referencesf`. The
pack releases the GIL from start to end, and other threads read and commit
while it runs. ZODB's PackableStorage suite passes, except the tests of
undo and of a pack without garbage collection.

The tests of a running pack watch it through reads: the pack removes the
unreachable records in batches, so a read between two batches finds some of
them and not the others.

Each test imports ZODB, transaction and persistent itself, so that the
module collects before these packages are installed.
"""

import contextlib
import os
import threading
import time
import unittest

import pytest
from support import ZERO, record

import zoe

# The longest time that a test waits for another thread.
TIMEOUT = 60
# The smallest time, in seconds, between a commit and a pack time. It is
# much longer than the resolution of a tid and of the clock.
MARGIN = 0.01
# The number of unreachable records that a test of a running pack writes.
# The pack removes them in many batches.
GARBAGE = 20_000
# The number of unreachable records in one transaction.
GARBAGE_PER_TXN = 5_000
# The number of unreachable records that a test reads to follow a pack.
PROBES = 9


def after(tid):
    """Return the tid that comes after tid."""
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


def time_of(tid):
    """Return the time of tid, in seconds since the epoch."""
    from ZODB.TimeStamp import TimeStamp

    return TimeStamp(tid).timeTime()


def wait_past(instant):
    """Wait until the clock is MARGIN past instant. Return the time then."""
    while time.time() <= instant + MARGIN:
        time.sleep(MARGIN / 10)
    return time.time()


@contextlib.contextmanager
def database(path):
    """Open a ZODB database on a storage in path, and close it at the end."""
    import ZODB

    db = ZODB.DB(zoe.Storage(str(path)))
    try:
        yield db
    finally:
        db.close()


def in_thread(func):
    """Start func in a daemon thread and return the thread."""
    thread = threading.Thread(target=func, daemon=True)
    thread.start()
    return thread


def loads(storage, oid, tid):
    """Tell if the record of oid at tid is in the store."""
    from ZODB.POSException import POSKeyError

    try:
        storage.loadSerial(oid, tid)
    except POSKeyError:
        return False
    return True


def add_garbage(storage):
    """Commit GARBAGE records of new objects that no object refers to.

    Return PROBES of their oids, spread from the first to the last.
    """
    from ZODB.Connection import TransactionMetaData

    oids = []
    for _ in range(GARBAGE // GARBAGE_PER_TXN):
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        for _ in range(GARBAGE_PER_TXN):
            oid = storage.new_oid()
            storage.store(oid, ZERO, record(len(oids)), "", txn)
            oids.append(oid)
        storage.tpc_vote(txn)
        storage.tpc_finish(txn)
    step = (len(oids) - 1) // (PROBES - 1)
    return oids[::step][:PROBES]


def remaining(storage, probes):
    """Return the number of probes that the store still holds."""
    from ZODB.POSException import POSKeyError

    found = 0
    for oid in probes:
        try:
            storage.load(oid)
        except POSKeyError:
            continue
        found += 1
    return found


def test_a_database_pack_removes_unreachable_objects_and_old_revisions(tmp_path):
    from persistent.mapping import PersistentMapping

    with database(tmp_path / "store") as db:
        storage = db.storage
        initial = storage.history(ZERO, 10)[-1]["tid"]
        with db.transaction() as conn:
            root = conn.root()
            root["kept"] = PersistentMapping(value=1)
            root["chain"] = PersistentMapping(child=PersistentMapping(value=2))
            root["dropped"] = PersistentMapping(child=PersistentMapping(value=3))
        first = db.lastTransaction()
        with db.transaction() as conn:
            root = conn.root()
            kept = root["kept"]._p_oid
            chain = root["chain"]._p_oid
            leaf = root["chain"]["child"]._p_oid
            dropped = root["dropped"]._p_oid
            below = root["dropped"]["child"]._p_oid
            root["kept"]["value"] = 4
            del root["dropped"]
        second = db.lastTransaction()
        wait_past(time_of(second))

        db.pack(days=0)

        # The old revisions and the objects that the root does not reach
        # are gone.
        for oid, tid in [
            (ZERO, initial),
            (ZERO, first),
            (kept, first),
            (dropped, first),
            (below, first),
        ]:
            assert not loads(storage, oid, tid), (oid, tid)
        # The current revisions stay, also of an object that did not change
        # since it was created.
        for oid, tid in [(ZERO, second), (kept, second), (chain, first), (leaf, first)]:
            assert loads(storage, oid, tid), (oid, tid)
        assert [entry["tid"] for entry in storage.history(kept, 10)] == [second]

        with db.transaction() as conn:
            root = conn.root()
            assert root["kept"]["value"] == 4
            assert root["chain"]["child"]["value"] == 2
            assert "dropped" not in root

        # A second pack finds nothing more to remove.
        db.pack(days=0)
        for oid, tid in [(ZERO, second), (kept, second), (chain, first), (leaf, first)]:
            assert loads(storage, oid, tid), (oid, tid)


def test_a_pack_at_a_time_keeps_what_a_reader_at_that_time_sees(tmp_path):
    from persistent.mapping import PersistentMapping

    with database(tmp_path / "store") as db:
        storage = db.storage
        initial = storage.history(ZERO, 10)[-1]["tid"]
        with db.transaction() as conn:
            root = conn.root()
            root["changed"] = PersistentMapping(value=1)
            root["dropped_before"] = PersistentMapping(value=1)
            root["dropped_after"] = PersistentMapping(value=1)
        first = db.lastTransaction()
        with db.transaction() as conn:
            root = conn.root()
            changed = root["changed"]._p_oid
            dropped_before = root["dropped_before"]._p_oid
            dropped_after = root["dropped_after"]._p_oid
            root["changed"]["value"] = 2
            del root["dropped_before"]
        second = db.lastTransaction()
        # The pack time comes after the second commit and before the third.
        pack_time = wait_past(time_of(second))
        wait_past(pack_time)
        with db.transaction() as conn:
            root = conn.root()
            root["changed"]["value"] = 3
            del root["dropped_after"]
            added_object = PersistentMapping(value=1)
            root["added"] = added_object
        third = db.lastTransaction()
        added = added_object._p_oid
        assert time_of(second) < pack_time < time_of(third)

        removed = [(ZERO, initial), (ZERO, first), (changed, first)]
        removed.append((dropped_before, first))
        # A reader at the pack time reaches dropped_after through the root.
        kept = [(ZERO, second), (ZERO, third), (changed, second), (changed, third)]
        kept += [(dropped_after, first), (added, third)]
        at_pack_time = {
            oid: tuple(storage.loadBefore(oid, after(second)))
            for oid in (ZERO, changed, dropped_after)
        }

        # Every revision is younger than a day, so this pack removes
        # nothing.
        db.pack(days=1)
        for oid, tid in removed + kept:
            assert loads(storage, oid, tid), (oid, tid)

        called = []

        def references(data, oids=None):
            called.append(data)
            raise AssertionError("the pack called referencesf")

        storage.pack(pack_time, references)

        assert called == []
        for oid, tid in removed:
            assert not loads(storage, oid, tid), (oid, tid)
        for oid, tid in kept:
            assert loads(storage, oid, tid), (oid, tid)
        for oid, view in at_pack_time.items():
            assert tuple(storage.loadBefore(oid, after(second))) == view, oid
        assert [entry["tid"] for entry in storage.history(changed, 10)] == [
            third,
            second,
        ]
        with db.transaction() as conn:
            root = conn.root()
            assert root["changed"]["value"] == 3
            assert root["added"]["value"] == 1
            assert "dropped_after" not in root


def test_a_pack_removes_the_blob_files_of_the_removed_records(tmp_path):
    from ZODB.blob import Blob
    from ZODB.interfaces import IBlobStorage
    from ZODB.POSException import POSKeyError

    with database(tmp_path / "store") as db:
        storage = db.storage
        if not IBlobStorage.providedBy(storage):
            pytest.skip("zoe.Storage has no blob API yet")
        with db.transaction() as conn:
            conn.root()["file"] = Blob(b"first revision")
            conn.root()["dropped"] = Blob(b"dropped blob")
        first = db.lastTransaction()
        with db.transaction() as conn:
            root = conn.root()
            file_oid = root["file"]._p_oid
            dropped = root["dropped"]._p_oid
            with root["file"].open("w") as file:
                file.write(b"second revision")
            del root["dropped"]
        second = db.lastTransaction()
        old_files = [
            storage.loadBlob(file_oid, first),
            storage.loadBlob(dropped, first),
        ]
        assert all(os.path.exists(path) for path in old_files)
        wait_past(time_of(second))

        db.pack(days=0)

        for path in old_files:
            assert not os.path.exists(path), path
        for oid in (file_oid, dropped):
            with pytest.raises(POSKeyError):
                storage.loadBlob(oid, first)
        with open(storage.loadBlob(file_oid, second), "rb") as file:
            assert file.read() == b"second revision"
        with db.transaction() as conn:
            with conn.root()["file"].open() as file:
                assert file.read() == b"second revision"


def test_the_pack_releases_the_gil_and_reads_go_on(tmp_path):
    with database(tmp_path / "store") as db:
        storage = db.storage
        probes = add_garbage(storage)
        wait_past(time_of(storage.lastTransaction()))
        started, ended = threading.Event(), threading.Event()
        errors = []

        def pack():
            started.set()
            try:
                db.pack()
            except BaseException as error:
                errors.append(error)
            finally:
                ended.set()

        thread = in_thread(pack)
        assert started.wait(TIMEOUT)
        # This thread reads while the pack runs. With the GIL held for the
        # whole pack, it reads only before the pack or after it, and finds
        # all the probes or none.
        seen = set()
        deadline = time.monotonic() + TIMEOUT
        while not ended.is_set() and time.monotonic() < deadline:
            seen.add(remaining(storage, probes))
        thread.join(TIMEOUT)
        assert ended.is_set(), "the pack did not end"
        assert not errors, errors

        assert remaining(storage, probes) == 0
        between = sorted(n for n in seen if 0 < n < len(probes))
        assert between, f"no read ran during the pack; the reads found {sorted(seen)}"


def test_commits_go_on_while_the_pack_runs(tmp_path):
    import transaction
    from persistent.mapping import PersistentMapping

    with database(tmp_path / "store") as db:
        storage = db.storage
        with db.transaction() as conn:
            conn.root()["counter"] = PersistentMapping(value=0)
        probes = add_garbage(storage)
        wait_past(time_of(storage.lastTransaction()))
        started, ended = threading.Event(), threading.Event()
        errors, during, committed = [], [], []

        def running():
            return 0 < remaining(storage, probes) < len(probes)

        def write():
            # Commit again and again while the pack runs. Note each commit
            # that begins and ends between two batches of the pack.
            tm = transaction.TransactionManager()
            conn = db.open(tm)
            try:
                assert started.wait(TIMEOUT)
                n = 0
                while not ended.is_set():
                    began = running()
                    n += 1
                    tm.begin()
                    root = conn.root()
                    root["counter"]["value"] = n
                    root[f"added-{n}"] = PersistentMapping(value=n)
                    tm.commit()
                    committed.append(n)
                    if began and running():
                        during.append(n)
            except BaseException as error:
                errors.append(error)
            finally:
                tm.abort()
                conn.close()

        writer = in_thread(write)
        started.set()
        try:
            db.pack()
        finally:
            ended.set()
        writer.join(TIMEOUT)
        assert not writer.is_alive(), "the writer did not end"
        assert not errors, errors

        assert remaining(storage, probes) == 0
        assert during, f"no commit ran during the pack; {len(committed)} commits ran"
        with db.transaction() as conn:
            root = conn.root()
            assert root["counter"]["value"] == committed[-1]
            for n in committed:
                assert root[f"added-{n}"]["value"] == n


def test_the_zodb_pack_suite_passes(tmp_path):
    from ZODB.tests import PackableStorage, StorageTestBase

    class ZoePackableStorage(
        StorageTestBase.StorageTestBase,
        PackableStorage.PackableStorageWithOptionalGC,
        PackableStorage.PackableUndoStorage,
    ):
        def setUp(self):
            super().setUp()
            self._storage = zoe.Storage(str(tmp_path / self._testMethodName))

        def testPackAllRevisionsNoGC(self):
            self.skipTest("the pack always collects garbage")

        def testPackUnlinkedFromRoot(self):
            self.skipTest("the storage does not undo")

        def testPackUndoLog(self):
            self.skipTest("the storage does not undo")

    suite = unittest.defaultTestLoader.loadTestsFromTestCase(ZoePackableStorage)
    result = unittest.TestResult()
    suite.run(result)

    problems = [f"{test.id()}\n{trace}" for test, trace in result.failures]
    problems += [f"{test.id()}\n{trace}" for test, trace in result.errors]
    assert not problems, "\n".join(problems)
    skipped = sorted(test._testMethodName for test, _ in result.skipped)
    assert skipped == [
        "testPackAllRevisionsNoGC",
        "testPackUndoLog",
        "testPackUnlinkedFromRoot",
    ]
    assert result.testsRun == suite.countTestCases()
    assert result.testsRun - len(skipped) >= 11
