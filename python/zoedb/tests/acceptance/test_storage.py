"""Acceptance test of the in-process storage: `zoe.Storage` implements IStorage.

ZODB opens the storage through its MVCCAdapter, commits and reloads. Only
the thread that began a transaction stores and votes in it. Any thread can
finish or abort it, as ZODB's own tests do. A vote that waits for a lock
lets the other threads run. The errors of the core arrive as the exceptions
of ZODB.

Each test imports ZODB, transaction and persistent itself, so that the
module collects before these packages are installed.
"""

import contextlib
import pickle
import threading
import time

import pytest

import zoe

# The tid and the serial of an object that has no record yet.
ZERO = bytes(8)
# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for another thread.
TIMEOUT = 30
# The lock timeout, in seconds, of the test that waits for a lock.
LOCK_TIMEOUT = 2


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def after(tid):
    """Return the tid that comes after tid."""
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


@contextlib.contextmanager
def opened(path, **options):
    """Open a storage in the directory path, and close it at the end."""
    storage = zoe.Storage(str(path), **options)
    try:
        yield storage
    finally:
        storage.close()


@contextlib.contextmanager
def database(path, **options):
    """Open a ZODB database on a storage in path, and close it at the end."""
    import ZODB

    db = ZODB.DB(zoe.Storage(str(path), **options))
    try:
        yield db
    finally:
        db.close()


def commit(storage, oid, serial, data, **metadata):
    """Store one record in one transaction. Return the tid."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(**metadata)
    storage.tpc_begin(txn)
    try:
        storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except Exception:
        storage.tpc_abort(txn)
        raise


def raised_in_another_thread(func):
    """Call func in a new thread and wait for it.

    Return the exception that func raised, or None.
    """
    raised = []

    def target():
        try:
            func()
        except Exception as error:
            raised.append(error)

    thread = threading.Thread(target=target, daemon=True)
    thread.start()
    thread.join(TIMEOUT)
    assert not thread.is_alive(), "the other thread did not end"
    return raised[0] if raised else None


def test_a_database_commits_and_reloads_after_a_reopen(tmp_path):
    from persistent.mapping import PersistentMapping

    with database(tmp_path / "store") as db:
        assert isinstance(db.storage, zoe.Storage)
        with db.transaction() as conn:
            conn.root()["item"] = PersistentMapping(value=1)
        last = db.lastTransaction()

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            assert conn.root()["item"]["value"] == 1
        assert db.lastTransaction() == last


def test_a_connection_reads_its_snapshot_until_its_next_transaction(tmp_path):
    import transaction
    from persistent.mapping import PersistentMapping

    with database(tmp_path / "store") as db:
        writer_tm = transaction.TransactionManager()
        reader_tm = transaction.TransactionManager()
        writer = db.open(writer_tm)
        reader = db.open(reader_tm)
        writer.root()["loaded"] = PersistentMapping(value=1)
        writer.root()["ghost"] = PersistentMapping(value=1)
        writer_tm.commit()

        reader_tm.begin()
        assert reader.root()["loaded"]["value"] == 1

        writer.root()["loaded"]["value"] = 2
        writer.root()["ghost"]["value"] = 2
        writer_tm.commit()

        # The reader keeps the snapshot of its transaction. This is true for
        # an object in its cache and for an object that it loads now.
        assert reader.root()["loaded"]["value"] == 1
        assert reader.root()["ghost"]["value"] == 1
        reader_tm.begin()
        assert reader.root()["loaded"]["value"] == 2
        assert reader.root()["ghost"]["value"] == 2
        reader.close()
        writer.close()


def test_the_storage_provides_istorage(tmp_path):
    from ZODB.interfaces import IStorage
    from zope.interface.verify import verifyObject

    with opened(tmp_path / "store") as storage:
        assert verifyObject(IStorage, storage)


def test_finish_returns_the_tid_and_gives_it_to_the_callback(tmp_path):
    from ZODB.Connection import TransactionMetaData

    with opened(tmp_path / "store") as storage:
        assert storage.lastTransaction() == ZERO
        oid = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, ZERO, record(1), "", txn)
        storage.tpc_vote(txn)
        called = []
        tid = storage.tpc_finish(txn, called.append)

        assert type(tid) is bytes and len(tid) == 8
        assert called == [tid]
        assert storage.lastTransaction() == tid
        assert commit(storage, oid, tid, record(2)) > tid


def test_each_revision_loads_by_tid(tmp_path):
    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, record(1))
        second = commit(storage, oid, first, record(2))

        assert first < second
        assert tuple(storage.loadBefore(oid, MAX_TID)) == (record(2), second, None)
        assert tuple(storage.loadBefore(oid, second)) == (record(1), first, second)
        assert storage.loadBefore(oid, first) is None
        assert storage.loadSerial(oid, first) == record(1)
        assert storage.loadSerial(oid, second) == record(2)
        assert tuple(storage.load(oid)) == (record(2), second)


def test_history_lists_the_revisions_newest_first(tmp_path):
    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, record(1), user="editor", description="one")
        second = commit(storage, oid, first, record(22), description="two")

        entries = storage.history(oid, 5)
        assert [entry["tid"] for entry in entries] == [second, first]
        assert [entry["serial"] for entry in entries] == [second, first]
        assert [entry["description"] for entry in entries] == [b"two", b"one"]
        assert entries[1]["user_name"] == b"editor"
        assert [entry["size"] for entry in entries] == [
            len(record(22)),
            len(record(1)),
        ]
        assert all(isinstance(entry["time"], float) for entry in entries)
        assert len(storage.history(oid, 1)) == 1


def test_new_oids_are_unique_and_not_given_again_after_a_reopen(tmp_path):
    from ZODB.Connection import TransactionMetaData

    with opened(tmp_path / "store") as storage:
        oids = [storage.new_oid() for _ in range(50)]
        assert all(type(oid) is bytes and len(oid) == 8 for oid in oids)
        assert len(set(oids)) == len(oids)
        # ZODB gives the zero oid to the root object.
        assert ZERO not in oids
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        for oid in oids:
            storage.store(oid, ZERO, record(1), "", txn)
        storage.tpc_vote(txn)
        storage.tpc_finish(txn)

    with opened(tmp_path / "store") as storage:
        later = {storage.new_oid() for _ in range(50)}
    assert not later & set(oids)


def test_the_informational_methods_answer(tmp_path):
    with opened(tmp_path / "one") as one, opened(tmp_path / "two") as two:
        commit(one, one.new_oid(), ZERO, record(1))

        assert isinstance(one.getName(), str)
        assert isinstance(one.getSize(), int)
        assert isinstance(len(one), int)
        assert isinstance(one.sortKey(), str)
        assert one.sortKey() != two.sortKey()
        assert one.isReadOnly() is False


def test_a_missing_object_raises_poskeyerror(tmp_path):
    from ZODB.POSException import POSKeyError

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        with pytest.raises(POSKeyError) as error:
            storage.loadBefore(oid, MAX_TID)
        assert error.value.args[0] == oid

        tid = commit(storage, oid, ZERO, record(1))
        with pytest.raises(POSKeyError):
            storage.loadSerial(oid, after(tid))

    with database(tmp_path / "store") as db:
        conn = db.open()
        with pytest.raises(POSKeyError):
            conn.get(after(oid))
        conn.close()


def test_a_stale_store_raises_conflict_error_with_oid_and_serials(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ConflictError, ReadConflictError

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = commit(storage, oid, ZERO, record(1))
        second = commit(storage, oid, first, record(2))

        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        with pytest.raises(ConflictError) as error:
            storage.store(oid, first, record(3), "", txn)
            storage.tpc_vote(txn)
        storage.tpc_abort(txn)

        assert not isinstance(error.value, ReadConflictError)
        assert error.value.oid == oid
        assert error.value.serials == (second, first)
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(2), second)


def test_a_stale_read_current_check_raises_read_conflict_error(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import POSKeyError, ReadConflictError

    with opened(tmp_path / "store") as storage:
        read = storage.new_oid()
        first = commit(storage, read, ZERO, record(1))
        second = commit(storage, read, first, record(2))
        written = storage.new_oid()

        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        with pytest.raises(ReadConflictError) as error:
            storage.store(written, ZERO, record(3), "", txn)
            storage.checkCurrentSerialInTransaction(read, first, txn)
            storage.tpc_vote(txn)
        storage.tpc_abort(txn)

        assert error.value.oid == read
        assert error.value.serials == (second, first)
        with pytest.raises(POSKeyError):
            storage.loadBefore(written, MAX_TID)


def test_read_current_through_a_connection_raises_read_conflict_error(tmp_path):
    import transaction
    from persistent.mapping import PersistentMapping
    from ZODB.POSException import ReadConflictError

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            conn.root()["read"] = PersistentMapping(value=0)
            conn.root()["written"] = PersistentMapping(value=0)

        reader_tm = transaction.TransactionManager()
        writer_tm = transaction.TransactionManager()
        reader = db.open(reader_tm)
        writer = db.open(writer_tm)
        reader_tm.begin()
        writer_tm.begin()

        read = reader.root()["read"]
        assert read["value"] == 0
        reader.readCurrent(read)
        reader.root()["written"]["value"] = 1

        writer.root()["read"]["value"] = 2
        writer_tm.commit()

        with pytest.raises(ReadConflictError) as error:
            reader_tm.commit()
        reader_tm.abort()
        assert error.value.oid == read._p_oid
        reader.close()
        writer.close()


def test_a_transaction_that_is_not_begun_raises_storage_transaction_error(
    tmp_path,
):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import StorageTransactionError

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        begun = TransactionMetaData()
        storage.tpc_begin(begun)
        with pytest.raises(StorageTransactionError):
            storage.tpc_begin(begun)

        stranger = TransactionMetaData()
        with pytest.raises(StorageTransactionError):
            storage.store(oid, ZERO, record(1), "", stranger)
        with pytest.raises(StorageTransactionError):
            storage.tpc_vote(stranger)
        with pytest.raises(StorageTransactionError):
            storage.tpc_finish(stranger)

        # IStorage tells the storage to ignore an abort of a transaction
        # that it does not know.
        storage.tpc_abort(stranger)
        storage.tpc_abort(begun)
        commit(storage, oid, ZERO, record(1))


def test_store_and_vote_need_the_beginning_thread_but_finish_does_not(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import POSKeyError

    with opened(tmp_path / "store") as storage:
        read = storage.new_oid()
        stale = commit(storage, read, ZERO, record(0))
        commit(storage, read, stale, record(1))
        mine = storage.new_oid()
        theirs = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(mine, ZERO, record(2), "", txn)

        def store_theirs():
            storage.store(theirs, ZERO, record(3), "", txn)

        def check_stale_read():
            storage.checkCurrentSerialInTransaction(read, stale, txn)

        def vote():
            storage.tpc_vote(txn)

        assert isinstance(raised_in_another_thread(store_theirs), zoe.WrongThread)
        assert isinstance(raised_in_another_thread(check_stale_read), zoe.WrongThread)
        assert isinstance(raised_in_another_thread(vote), zoe.WrongThread)
        # The refused read check changed nothing, so this vote does not
        # raise ReadConflictError.
        storage.tpc_vote(txn)

        # ZODB lets another thread finish a transaction.
        tids = []
        assert (
            raised_in_another_thread(lambda: tids.append(storage.tpc_finish(txn)))
            is None
        )
        assert storage.lastTransaction() == tids[0]

        # The refused store changed nothing.
        assert tuple(storage.loadBefore(mine, MAX_TID))[:2] == (record(2), tids[0])
        with pytest.raises(POSKeyError):
            storage.loadBefore(theirs, MAX_TID)


def test_another_thread_can_abort_a_transaction(tmp_path):
    from ZODB.Connection import TransactionMetaData

    with opened(tmp_path / "store", lock_timeout=LOCK_TIMEOUT) as storage:
        oid = storage.new_oid()
        serial = commit(storage, oid, ZERO, record(0))
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, serial, record(1), "", txn)
        storage.tpc_vote(txn)

        assert raised_in_another_thread(lambda: storage.tpc_abort(txn)) is None

        # The abort released the lock and wrote nothing.
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(0), serial)
        tid = commit(storage, oid, serial, record(2))
        assert tuple(storage.loadBefore(oid, MAX_TID))[:2] == (record(2), tid)


def test_a_disjoint_transaction_commits_while_another_holds_its_locks(tmp_path):
    from ZODB.Connection import TransactionMetaData

    with opened(tmp_path / "store") as storage:
        held = storage.new_oid()
        free = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        try:
            storage.store(held, ZERO, record(1), "", txn)
            # The vote takes the lock on held, until finish or abort.
            storage.tpc_vote(txn)

            def commit_free():
                commit(storage, free, ZERO, record(2))

            assert raised_in_another_thread(commit_free) is None
            tid = storage.tpc_finish(txn)
        except Exception:
            storage.tpc_abort(txn)
            raise

        free_tid = tuple(storage.loadBefore(free, MAX_TID))[1]
        assert free_tid < tid


def test_a_vote_that_waits_for_a_lock_lets_other_threads_run(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ConflictError

    outcome = {}
    with opened(tmp_path / "store", lock_timeout=LOCK_TIMEOUT) as storage:
        oid = storage.new_oid()
        serial = commit(storage, oid, ZERO, record(0))
        holder = TransactionMetaData()
        storage.tpc_begin(holder)
        try:
            storage.store(oid, serial, record(1), "", holder)
            storage.tpc_vote(holder)

            def contend():
                txn = TransactionMetaData()
                storage.tpc_begin(txn)
                try:
                    storage.store(oid, serial, record(2), "", txn)
                    start = time.monotonic()
                    try:
                        storage.tpc_vote(txn)
                    except ConflictError as error:
                        outcome["error"] = error
                    outcome["waited"] = time.monotonic() - start
                finally:
                    storage.tpc_abort(txn)

            # The contender waits in its vote for the lock that the holder
            # keeps. Meanwhile this thread counts the longest pause between
            # two of its own steps.
            thread = threading.Thread(target=contend, daemon=True)
            longest = 0.0
            last = time.monotonic()
            deadline = last + TIMEOUT
            thread.start()
            while thread.is_alive() and last < deadline:
                now = time.monotonic()
                longest = max(longest, now - last)
                last = now
            assert not thread.is_alive(), "the vote did not end"
        finally:
            storage.tpc_abort(holder)

    # The wait ends at the lock timeout with a conflict on the oid.
    assert isinstance(outcome.get("error"), ConflictError)
    assert outcome["error"].oid == oid
    assert LOCK_TIMEOUT / 2 <= outcome["waited"] < 3 * LOCK_TIMEOUT
    # The waiting vote does not hold the GIL, so this thread never stops.
    assert longest < LOCK_TIMEOUT / 2


def test_a_read_only_storage_refuses_tpc_begin(tmp_path):
    from persistent.mapping import PersistentMapping
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ReadOnlyError

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            conn.root()["item"] = PersistentMapping(value=1)

    with database(tmp_path / "store", read_only=True) as db:
        storage = db.storage
        assert storage.isReadOnly() is True
        with pytest.raises(ReadOnlyError):
            storage.tpc_begin(TransactionMetaData())
        with db.transaction() as conn:
            assert conn.root()["item"]["value"] == 1


def test_undo_is_not_supported(tmp_path):
    from ZODB.interfaces import IStorageUndoable

    with database(tmp_path / "store") as db:
        assert db.storage.supportsUndo() is False
        assert db.supportsUndo() is False
        assert not IStorageUndoable.providedBy(db.storage)
