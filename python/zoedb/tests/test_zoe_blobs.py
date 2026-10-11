"""Unit tests of the blobs of `zoe.Storage`.

The acceptance test covers the commit of a blob, the rename from the
temporary directory, the read-only files and the layout. These tests cover
the rest: the refused calls, the copy of a file from elsewhere, a blob of a
savepoint, `blob_dir` and the calls on a read-only or a closed storage.
"""

import os
import pickle
import tempfile
import threading

import pytest
import transaction
from ZODB import DB
from ZODB.blob import Blob
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
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


def test_a_committed_blob_file_is_copied_and_kept(storage):
    oid = storage.new_oid()
    first = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
    committed = storage.loadBlob(oid, first)
    second = store_blob(storage, oid, first, committed)
    assert read(storage.loadBlob(oid, second)) == DATA
    assert read(committed) == DATA


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


def test_the_blob_directory_stays_with_the_store(tmp_path):
    path, blob_dir = str(tmp_path / "store"), tmp_path / "elsewhere"
    storage = zoe.Storage(path, blob_dir=str(blob_dir))
    try:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
    finally:
        storage.close()

    # A later open without blob_dir, or with the same one, finds the blobs.
    for options in ({}, {"blob_dir": str(blob_dir)}):
        storage = zoe.Storage(path, **options)
        try:
            committed = os.path.realpath(storage.loadBlob(oid, tid))
            assert committed.startswith(os.path.realpath(blob_dir) + os.sep)
            assert read(committed) == DATA
        finally:
            storage.close()

    # The store refuses another blob directory.
    with pytest.raises(StorageError):
        zoe.Storage(path, blob_dir=str(tmp_path / "other"))
    zoe.Storage(path, blob_dir=os.path.join(path, "blobs")).close()


def test_a_store_with_blobs_refuses_another_blob_directory(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    try:
        store_blob(storage, storage.new_oid(), ZERO, write_temporary(storage, DATA))
    finally:
        storage.close()
    with pytest.raises(StorageError):
        zoe.Storage(path, blob_dir=str(tmp_path / "elsewhere"))
    assert not (tmp_path / "elsewhere").exists()


def test_a_new_link_needs_an_empty_blob_directory(tmp_path):
    used = tmp_path / "used"
    used.mkdir()
    (used / "file").write_bytes(b"x")
    with pytest.raises(StorageError):
        zoe.Storage(str(tmp_path / "store"), blob_dir=str(used))

    # An empty directory is taken.
    empty = tmp_path / "empty"
    empty.mkdir()
    storage = zoe.Storage(str(tmp_path / "other"), blob_dir=str(empty))
    try:
        tid = store_blob(
            storage, storage.new_oid(), ZERO, write_temporary(storage, DATA)
        )
        assert tid > ZERO
        assert (empty / ".layout").read_text().strip() == "bushy"
    finally:
        storage.close()


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
        ):
            with pytest.raises(ReadOnlyError):
                call()
        assert read(storage.loadBlob(oid, tid)) == DATA
        with storage.openCommittedBlobFile(oid, tid) as file:
            assert file.read() == DATA
    finally:
        storage.close()
    assert outside.exists()

    # A read-only storage links no blob directory.
    zoe.Storage(str(tmp_path / "plain")).close()
    zoe.Storage(
        str(tmp_path / "plain"), read_only=True, blob_dir=str(tmp_path / "other")
    ).close()
    assert not (tmp_path / "other").exists()
    assert not (tmp_path / "plain" / "blobs").exists()


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
    ):
        with pytest.raises(StorageError):
            call()
    assert read(path) == DATA
