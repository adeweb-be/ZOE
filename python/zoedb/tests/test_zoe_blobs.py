"""Unit tests of the blobs of `zoe.Storage`.

The acceptance test covers the commit of a blob, the rename from the
temporary directory, the read-only files and the layout. These tests cover
the rest: the refused calls, the copy of a file from elsewhere, a blob of a
savepoint, `blob_dir`, `restoreBlob` and the calls on a read-only or a
closed storage.
"""

import os
import pickle
import shutil
import tempfile
import threading

import pytest
import transaction
from ZODB import DB
from ZODB.blob import Blob
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
    ConflictError,
    POSKeyError,
    ReadOnlyError,
    StorageError,
    StorageTransactionError,
)

import zoe

ZERO = bytes(8)
DATA = bytes(range(256)) * 64


def blob_record():
    """Return the record that ZODB writes for a blob."""
    return pickle.dumps((("ZODB.blob", "Blob"), None), 3) + pickle.dumps(None, 3)


def p64(value):
    return value.to_bytes(8, "big")


def u64(value):
    return int.from_bytes(value, "big")


def write_temporary(storage, data):
    """Write data to a new file in the temporary directory of storage."""
    fd, path = tempfile.mkstemp(dir=storage.temporaryDirectory(), suffix=".tmp")
    with os.fdopen(fd, "wb") as file:
        file.write(data)
    return path


def read(path):
    with open(path, "rb") as file:
        return file.read()


@pytest.fixture
def storage(tmp_path):
    opened = zoe.Storage(str(tmp_path / "store"))
    yield opened
    opened.close()


def store_blob(storage, oid, serial, path):
    """Store the blob file path in one transaction. Return the tid."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.storeBlob(oid, serial, blob_record(), path, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def test_a_refused_store_blob_leaves_the_file(storage):
    oid = storage.new_oid()
    path = write_temporary(storage, DATA)
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    raised = []

    def other_thread():
        try:
            storage.storeBlob(oid, ZERO, blob_record(), path, "", txn)
        except Exception as error:
            raised.append(error)

    thread = threading.Thread(target=other_thread)
    thread.start()
    thread.join()
    assert len(raised) == 1 and isinstance(raised[0], zoe.WrongThread)
    assert read(path) == DATA

    # A store after the vote is refused too.
    storage.tpc_vote(txn)
    with pytest.raises(StorageTransactionError):
        storage.storeBlob(oid, ZERO, blob_record(), path, "", txn)
    assert read(path) == DATA
    storage.tpc_abort(txn)

    # The file is still free to commit.
    tid = store_blob(storage, oid, ZERO, path)
    assert read(storage.loadBlob(oid, tid)) == DATA


def test_a_file_from_elsewhere_is_copied_then_removed(storage, tmp_path):
    oid = storage.new_oid()
    outside = tmp_path / "outside.blob"
    outside.write_bytes(DATA)
    inode = os.stat(outside).st_ino
    tid = store_blob(storage, oid, ZERO, str(outside))

    committed = storage.loadBlob(oid, tid)
    assert read(committed) == DATA
    assert os.stat(committed).st_ino != inode
    assert not outside.exists()


@pytest.mark.parametrize("blob_dir", [None, "elsewhere"], ids=["default", "blob_dir"])
def test_a_file_of_the_store_is_refused(tmp_path, blob_dir):
    store = tmp_path / "store"
    options = {} if blob_dir is None else {"blob_dir": str(tmp_path / blob_dir)}
    storage = zoe.Storage(str(store), **options)
    try:
        oid = storage.new_oid()
        first = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
        committed = storage.loadBlob(oid, first)
        layout = os.path.join(os.path.dirname(storage.temporaryDirectory()), ".layout")
        database = store / "zoe.redb"
        size = database.stat().st_size
        for path in (
            str(database),
            committed,
            layout,
            str(store / ".." / "store" / "lock"),
        ):
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            with pytest.raises(StorageTransactionError):
                storage.storeBlob(oid, first, blob_record(), path, "", txn)
            storage.tpc_abort(txn)
            assert os.path.exists(path), path
        assert database.stat().st_size == size
        assert read(committed) == DATA
        second = store_blob(storage, oid, first, write_temporary(storage, b"second"))
        assert read(storage.loadBlob(oid, second)) == b"second"
    finally:
        storage.close()


def test_a_relative_path_in_the_temporary_directory_moves_by_rename(storage, tmp_path):
    oid = storage.new_oid()
    path = write_temporary(storage, DATA)
    inode = os.stat(path).st_ino
    cwd = os.getcwd()
    os.chdir(storage.temporaryDirectory())
    try:
        tid = store_blob(storage, oid, ZERO, os.path.basename(path))
    finally:
        os.chdir(cwd)
    assert os.stat(storage.loadBlob(oid, tid)).st_ino == inode


def test_a_blob_of_a_savepoint_commits_by_rename(tmp_path):
    db = DB(zoe.Storage(str(tmp_path / "store")))
    try:
        conn = db.open()
        blob = Blob()
        conn.root()["blob"] = blob
        with blob.open("w") as file:
            file.write(DATA)
        inode = os.stat(blob._p_blob_uncommitted).st_ino
        # The savepoint moves the file into a directory of its own in the
        # temporary directory. The commit takes it from there.
        transaction.savepoint()
        transaction.commit()

        blob._p_activate()
        committed = blob.committed()
        assert os.stat(committed).st_ino == inode
        assert read(committed) == DATA
        conn.close()
    finally:
        transaction.abort()
        db.close()


def test_the_blob_directory_comes_from_the_option(tmp_path):
    path, blob_dir = str(tmp_path / "store"), tmp_path / "a" / "elsewhere"
    storage = zoe.Storage(path, blob_dir=str(blob_dir))
    try:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
        committed = storage.loadBlob(oid, tid)
        assert committed.startswith(str(blob_dir) + os.sep)
        assert storage.temporaryDirectory() == str(blob_dir / "tmp")
    finally:
        storage.close()
    assert (blob_dir / ".layout").read_text().strip() == "bushy"
    assert not (tmp_path / "store" / "blobs").exists()

    # The store does not record its blob directory. Without it, or with
    # another one, the store refuses to open.
    for options in ({}, {"blob_dir": str(tmp_path / "other")}):
        with pytest.raises(StorageError):
            zoe.Storage(path, **options)
    assert not (tmp_path / "store" / "blobs").exists()

    storage = zoe.Storage(path, blob_dir=str(blob_dir))
    try:
        assert read(storage.loadBlob(oid, tid)) == DATA
    finally:
        storage.close()


def test_a_blob_dir_with_other_files_is_refused(tmp_path):
    blob_dir = tmp_path / "reports"
    (blob_dir / "tmp").mkdir(parents=True)
    (blob_dir / "tmp" / "report.txt").write_bytes(b"theirs")
    for _ in range(2):
        with pytest.raises(StorageError):
            zoe.Storage(str(tmp_path / "store"), blob_dir=str(blob_dir))
        assert (blob_dir / "tmp" / "report.txt").read_bytes() == b"theirs"
    assert not (blob_dir / ".layout").exists()


def test_two_storages_never_share_a_blob_dir(tmp_path):
    blob_dir = str(tmp_path / "shared")
    first = zoe.Storage(str(tmp_path / "a"), blob_dir=blob_dir)
    try:
        oid = first.new_oid()
        tid = store_blob(first, oid, ZERO, write_temporary(first, DATA))
        pending = write_temporary(first, b"pending")
        with pytest.raises(StorageError):
            zoe.Storage(str(tmp_path / "b"), blob_dir=blob_dir)
        assert read(pending) == b"pending"
        assert read(first.loadBlob(oid, tid)) == DATA
    finally:
        first.close()


def test_a_copy_of_the_store_directory_keeps_its_own_blobs(tmp_path):
    original = tmp_path / "original"
    storage = zoe.Storage(str(original))
    try:
        oid = storage.new_oid()
        first = store_blob(storage, oid, ZERO, write_temporary(storage, b"first"))
        shutil.copytree(original, tmp_path / "copy", symlinks=True)
        second = store_blob(storage, oid, first, write_temporary(storage, b"second"))
    finally:
        storage.close()

    # The copy opens with its own blob directory, so its open does not touch
    # the blobs of the original.
    copy = zoe.Storage(str(tmp_path / "copy"))
    try:
        assert read(copy.loadBlob(oid, first)) == b"first"
        with pytest.raises(POSKeyError):
            copy.loadBlob(oid, second)
    finally:
        copy.close()
    storage = zoe.Storage(str(original))
    try:
        assert read(storage.loadBlob(oid, second)) == b"second"
    finally:
        storage.close()


def test_a_refused_blob_dir_creates_no_store(tmp_path):
    a_file = tmp_path / "file"
    a_file.write_bytes(b"x")
    (tmp_path / "empty").mkdir()
    for path in (tmp_path / "missing", tmp_path / "empty"):
        with pytest.raises(OSError):
            zoe.Storage(str(path), blob_dir=str(a_file))
    assert not (tmp_path / "missing").exists()
    assert list((tmp_path / "empty").iterdir()) == []


def test_a_missing_blob_file_raises_poskeyerror(storage):
    oid = storage.new_oid()
    tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
    os.remove(storage.loadBlob(oid, tid))
    with pytest.raises(POSKeyError):
        storage.loadBlob(oid, tid)
    with pytest.raises(POSKeyError):
        storage.openCommittedBlobFile(oid, tid)


def test_a_read_only_storage_refuses_the_blob_writes(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    try:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
    finally:
        storage.close()

    outside = tmp_path / "outside.blob"
    outside.write_bytes(DATA)
    storage = zoe.Storage(path, read_only=True)
    try:
        txn = TransactionMetaData()
        for call in (
            storage.temporaryDirectory,
            lambda: storage.storeBlob(oid, tid, blob_record(), str(outside), "", txn),
            lambda: storage.restoreBlob(
                oid, tid, blob_record(), str(outside), None, txn
            ),
        ):
            with pytest.raises(ReadOnlyError):
                call()
        assert read(storage.loadBlob(oid, tid)) == DATA
        with storage.openCommittedBlobFile(oid, tid) as file:
            assert file.read() == DATA
    finally:
        storage.close()
    assert outside.exists()

    # A read-only storage creates no parent of its blob directory.
    with pytest.raises(FileNotFoundError):
        zoe.Storage(path, read_only=True, blob_dir=str(tmp_path / "a" / "b"))
    assert not (tmp_path / "a").exists()


def test_the_blob_calls_of_a_closed_storage_raise_storage_error(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    oid = storage.new_oid()
    tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
    path = write_temporary(storage, DATA)
    storage.close()
    txn = TransactionMetaData()
    for call in (
        storage.temporaryDirectory,
        lambda: storage.loadBlob(oid, tid),
        lambda: storage.openCommittedBlobFile(oid, tid),
        lambda: storage.storeBlob(oid, tid, blob_record(), path, "", txn),
        lambda: storage.restoreBlob(oid, tid, blob_record(), path, None, txn),
    ):
        with pytest.raises(StorageError):
            call()
    assert read(path) == DATA


def test_restore_blob_commits_under_the_tid_of_the_transaction(storage, tmp_path):
    oid = storage.new_oid()
    first = store_blob(storage, oid, ZERO, write_temporary(storage, b"first"))
    tid = p64(u64(first) + 1000)
    outside = tmp_path / "outside.blob"
    outside.write_bytes(b"copied")
    other = storage.new_oid()

    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    # The serial and the previous transaction of ZODB are not used: the
    # previous serial is the current serial of the store.
    storage.restoreBlob(
        oid, tid, blob_record(), write_temporary(storage, b"restored"), None, txn
    )
    storage.restoreBlob(other, tid, blob_record(), str(outside), first, txn)
    storage.tpc_vote(txn)
    assert storage.tpc_finish(txn) == tid

    assert storage.lastTransaction() == tid
    assert read(storage.loadBlob(oid, tid)) == b"restored"
    assert read(storage.loadBlob(oid, first)) == b"first"
    assert read(storage.loadBlob(other, tid)) == b"copied"
    assert storage.loadBefore(oid, p64(2**63 - 1))[:2] == (blob_record(), tid)
    assert not outside.exists()


def test_restore_blob_needs_a_restore_transaction(storage):
    oid = storage.new_oid()
    path = write_temporary(storage, DATA)
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    with pytest.raises(StorageTransactionError):
        storage.restoreBlob(oid, ZERO, blob_record(), path, None, txn)
    storage.tpc_abort(txn)
    assert read(path) == DATA

    # A restore transaction needs a tid after the last one.
    last = store_blob(storage, oid, ZERO, path)
    with pytest.raises(StorageTransactionError):
        storage.tpc_begin(TransactionMetaData(), last)


def test_restore_blob_conflicts_when_the_object_changes_before_the_vote(storage):
    oid = storage.new_oid()
    first = store_blob(storage, oid, ZERO, write_temporary(storage, b"first"))
    txn = TransactionMetaData()
    storage.tpc_begin(txn, p64(2**63 - 2))
    storage.restoreBlob(
        oid, ZERO, blob_record(), write_temporary(storage, b"restored"), None, txn
    )

    second = store_blob(storage, oid, first, write_temporary(storage, b"second"))
    with pytest.raises(ConflictError) as error:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert error.value.serials == (second, first)
    assert storage._lock_table_is_empty()
    with pytest.raises(POSKeyError):
        storage.loadBlob(oid, p64(2**63 - 2))


def test_tpc_begin_takes_the_status_of_a_copied_transaction(storage):
    last = storage.lastTransaction()
    for n, status in enumerate([" ", "p", b" ", b"p"], start=1):
        tid = p64(u64(last) + n)
        txn = TransactionMetaData()
        storage.tpc_begin(txn, tid, status)
        storage.store(storage.new_oid(), ZERO, blob_record(), "", txn)
        storage.tpc_vote(txn)
        assert storage.tpc_finish(txn) == tid
    for status in ("u", "c", ""):
        with pytest.raises(StorageTransactionError):
            storage.tpc_begin(TransactionMetaData(), p64(u64(last) + 10), status)
    with pytest.raises(TypeError):
        storage.tpc_begin(TransactionMetaData(), p64(u64(last) + 10), 1)


def test_zodb_copies_a_storage_with_its_tids(tmp_path):
    from ZODB.BaseStorage import copy
    from ZODB.FileStorage import FileStorage

    source = FileStorage(str(tmp_path / "Data.fs"))
    db = DB(source)
    try:
        for n in range(3):
            with db.transaction() as conn:
                conn.root()[n] = n
    finally:
        db.close()

    source = FileStorage(str(tmp_path / "Data.fs"), read_only=True)
    storage = zoe.Storage(str(tmp_path / "store"))
    try:
        # ZODB calls tpc_begin with the tid and the status of each
        # transaction of the source.
        copy(source, storage)
        assert storage.lastTransaction() == source.lastTransaction()
        records = [record for txn in source.iterator() for record in txn]
        assert len(records) > 3
        for record in records:
            assert storage.loadSerial(record.oid, record.tid) == record.data
    finally:
        storage.close()
        source.close()
