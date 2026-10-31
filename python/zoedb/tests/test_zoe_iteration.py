"""Unit tests of the iteration, `restore` and the copy of `zoe.Storage`.

The acceptance test covers the iterator bounds, `record_iternext`,
`restore`, `restoreBlob` and the copy in both directions. These tests run
the iteration and recovery suites of ZODB, and cover the rest: the refused
calls, a deletion, a closed storage and a failed copy.
"""

import os
import threading

import pytest
from support import ZERO, commit
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
    ReadOnlyError,
    StorageError,
    StorageTransactionError,
    Unsupported,
)
from ZODB.tests import IteratorStorage, RecoveryStorage, StorageTestBase

import zoe
import zoe.storage


class ZODBIterationTests(
    StorageTestBase.StorageTestBase,
    IteratorStorage.IteratorStorage,
    IteratorStorage.ExtendedIteratorStorage,
    RecoveryStorage.RecoveryStorage,
):
    """The iteration and recovery suites of ZODB."""

    # The iterator gives the extension as the store keeps it.
    use_extension_bytes = True

    def setUp(self):
        # The base class makes a temporary directory the current directory.
        StorageTestBase.StorageTestBase.setUp(self)
        self._storage = zoe.Storage("store")
        self._dst = zoe.Storage("destination")

    def tearDown(self):
        self._dst.close()
        StorageTestBase.StorageTestBase.tearDown(self)

    def new_dest(self):
        return zoe.Storage("other")

    def testUndoZombie(self):
        pytest.skip("the storage does not undo")

    def testRestoreAcrossPack(self):
        pytest.skip("the storage does not undo")

    def testRestoreWithMultipleObjectsInUndoRedo(self):
        pytest.skip("the storage does not undo")

    def testRestoreWithMultipleUndoRedo(self):
        pytest.skip("the storage does not undo")


def p64(value):
    return value.to_bytes(8, "big")


def record(value):
    """Return a small pickle that holds value."""
    return StorageTestBase.zodb_pickle(StorageTestBase.MinPO(value))


@pytest.fixture
def storage(tmp_path):
    opened = zoe.Storage(str(tmp_path / "store"))
    yield opened
    opened.close()


def rows(iterator):
    return [(tid, records) for tid, _, _, _, records in iterator]


def test_record_iternext_of_an_empty_storage_raises_value_error(storage):
    with pytest.raises(ValueError):
        storage.record_iternext()
    oid = storage.new_oid()
    commit(storage, [(oid, ZERO, record(1))])
    with pytest.raises(ValueError):
        storage.record_iternext(p64(int.from_bytes(oid, "big") + 1))


def test_an_iterator_raises_storage_error_after_close(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    for n in range(2):
        commit(storage, [(storage.new_oid(), ZERO, record(n))])
    iterator = storage.iterator()
    next(iterator)
    storage.close()
    with pytest.raises(StorageError):
        next(iterator)
    with pytest.raises(StorageError):
        storage.iterator()
    with pytest.raises(StorageError):
        storage.record_iternext()


def test_restore_needs_a_restore_transaction(storage):
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    with pytest.raises(StorageTransactionError):
        storage.restore(storage.new_oid(), ZERO, record(1), "", None, txn)
    storage.tpc_abort(txn)
    with pytest.raises(StorageTransactionError):
        storage.restore(storage.new_oid(), ZERO, record(1), "", None, txn)


def test_restore_refuses_a_version_and_another_thread(storage):
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn, p64(1000))
    with pytest.raises(Unsupported):
        storage.restore(oid, ZERO, record(1), "version", None, txn)

    raised = []

    def restore():
        try:
            storage.restore(oid, ZERO, record(1), "", None, txn)
        except zoe.WrongThread as error:
            raised.append(error)

    thread = threading.Thread(target=restore)
    thread.start()
    thread.join()
    assert len(raised) == 1

    storage.restore(oid, ZERO, record(2), "", None, txn)
    storage.tpc_vote(txn)
    assert storage.tpc_finish(txn) == p64(1000)
    assert storage.load(oid) == (record(2), p64(1000))


def test_restore_of_none_copies_a_deletion(storage):
    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record(1))])
    tid = p64(int.from_bytes(first, "big") + 1)
    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    storage.restore(oid, first, None, "", first, txn)
    storage.tpc_vote(txn)
    storage.tpc_finish(txn)

    ((_, records),) = rows(storage._iterator(tid))
    assert records == [(oid, tid, None)]
    assert storage.history(oid, 2)[0]["size"] == 0


def test_a_read_only_storage_refuses_restore(tmp_path):
    zoe.Storage(str(tmp_path / "store")).close()
    storage = zoe.Storage(str(tmp_path / "store"), read_only=True)
    try:
        with pytest.raises(ReadOnlyError):
            storage.restore(ZERO, ZERO, record(1), "", None, TransactionMetaData())
    finally:
        storage.close()


class Refusing(zoe.storage.Storage):
    """A storage whose `restoreBlob` refuses every blob."""

    def restoreBlob(self, *args):
        raise StorageTransactionError("refused")


def test_a_failed_copy_aborts_and_leaves_no_blob_copy(tmp_path):
    from ZODB import DB
    from ZODB.blob import Blob
    from ZODB.FileStorage import FileStorage

    path, blob_dir = str(tmp_path / "Data.fs"), str(tmp_path / "blobs")
    db = DB(FileStorage(path, blob_dir=blob_dir))
    try:
        with db.transaction() as conn:
            conn.root()["blob"] = Blob(b"blob")
    finally:
        db.close()

    source = FileStorage(path, blob_dir=blob_dir, read_only=True)
    storage = Refusing(str(tmp_path / "store"))
    try:
        first, _ = (txn.tid for txn in source.iterator())
        with pytest.raises(StorageTransactionError):
            storage.copyTransactionsFrom(source)
        # The transaction before the blob stays.
        assert storage.lastTransaction() == first
        assert storage._lock_table_is_empty()
        assert os.listdir(storage.temporaryDirectory()) == []
    finally:
        storage.close()
        source.close()
