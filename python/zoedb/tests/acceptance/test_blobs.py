"""Acceptance test of IBlobStorage in-process.

A ZODB blob commits and reads back on `zoe.Storage`. `storeBlob` takes the
file from `temporaryDirectory()` by rename, never by copy. `loadBlob`
returns the path of a read-only file, laid out as ZODB's BushyLayout in the
blob directory.

Each test imports ZODB itself, so that the module collects before ZODB is
installed.
"""

import contextlib
import os
import pickle
import stat
import tempfile

import pytest

import zoe

# The tid and the serial of an object that has no record yet.
ZERO = bytes(8)
# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# Synthetic blob content, larger than one file system block.
DATA = bytes(range(256)) * 1200


def blob_record():
    """Return the record that ZODB writes for a blob."""
    return pickle.dumps((("ZODB.blob", "Blob"), None), 3) + pickle.dumps(None, 3)


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


def write_temporary(storage, data):
    """Write data to a new file in the temporary directory of storage."""
    fd, path = tempfile.mkstemp(dir=storage.temporaryDirectory(), suffix=".tmp")
    with os.fdopen(fd, "wb") as file:
        file.write(data)
    return path


def store_blob(storage, oid, serial, path):
    """Store the blob file path in one transaction. Return the tid."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        storage.storeBlob(oid, serial, blob_record(), path, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except Exception:
        storage.tpc_abort(txn)
        raise


def read(path):
    with open(path, "rb") as file:
        return file.read()


def committed_blob_files(directory):
    return sorted(str(path) for path in directory.rglob("*.blob"))


def test_a_blob_commits_and_reads_back_after_a_reopen(tmp_path):
    from ZODB.blob import Blob

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            conn.root()["blob"] = Blob(DATA)

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            with conn.root()["blob"].open("r") as file:
                assert file.read() == DATA


def test_the_storage_provides_iblobstorage(tmp_path):
    from ZODB.interfaces import IBlobStorage
    from zope.interface.verify import verifyObject

    with opened(tmp_path / "store") as storage:
        assert verifyObject(IBlobStorage, storage)
        assert os.path.isdir(storage.temporaryDirectory())


def test_store_blob_takes_the_file_by_rename(tmp_path):
    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        path = write_temporary(storage, DATA)
        before = os.stat(path)
        tid = store_blob(storage, oid, ZERO, path)

        committed = storage.loadBlob(oid, tid)
        after_commit = os.stat(committed)
        # A rename keeps the inode. A copy makes a new one.
        assert (after_commit.st_dev, after_commit.st_ino) == (
            before.st_dev,
            before.st_ino,
        )
        assert not os.path.exists(path)
        assert read(committed) == DATA


def test_a_blob_from_the_temporary_directory_reaches_the_store_by_rename(
    tmp_path,
):
    from ZODB.blob import Blob

    with database(tmp_path / "store") as db:
        with db.transaction() as conn:
            path = write_temporary(db.storage, DATA)
            before = os.stat(path)
            blob = Blob()
            conn.add(blob)
            # The blob of a connection keeps its uncommitted file in the
            # temporary directory of the storage.
            blob.consumeFile(path)
            conn.root()["blob"] = blob

        with db.transaction() as conn:
            blob = conn.root()["blob"]
            blob._p_activate()
            committed = blob.committed()
        after_commit = os.stat(committed)
        assert (after_commit.st_dev, after_commit.st_ino) == (
            before.st_dev,
            before.st_ino,
        )
        assert read(committed) == DATA


def test_load_blob_returns_a_read_only_file(tmp_path):
    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
        mode = stat.S_IMODE(os.stat(storage.loadBlob(oid, tid)).st_mode)
        assert mode & (stat.S_IWUSR | stat.S_IWGRP | stat.S_IWOTH) == 0


def test_open_committed_blob_file_reads_the_committed_data(tmp_path):
    from ZODB.blob import Blob, BlobFile

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))

        with storage.openCommittedBlobFile(oid, tid) as file:
            assert file.read() == DATA
        with storage.openCommittedBlobFile(oid, tid, Blob()) as file:
            assert isinstance(file, BlobFile)
            assert file.read() == DATA


@pytest.mark.parametrize("explicit", [False, True], ids=["default", "blob_dir"])
def test_the_files_follow_bushy_layout_in_the_blob_directory(tmp_path, explicit):
    from ZODB.blob import BushyLayout

    if explicit:
        blob_dir = tmp_path / "elsewhere"
        options = {"blob_dir": str(blob_dir)}
    else:
        blob_dir = tmp_path / "store" / "blobs"
        options = {}

    with opened(tmp_path / "store", **options) as storage:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
        path = storage.loadBlob(oid, tid)

    relative = os.path.relpath(os.path.realpath(path), os.path.realpath(blob_dir))
    assert relative == BushyLayout().getBlobFilePath(oid, tid)
    assert (blob_dir / ".layout").read_text().strip() == "bushy"


def test_each_revision_keeps_its_own_file(tmp_path):
    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        first = store_blob(storage, oid, ZERO, write_temporary(storage, b"first"))
        second = store_blob(storage, oid, first, write_temporary(storage, b"second"))

        assert storage.loadBlob(oid, first) != storage.loadBlob(oid, second)
        assert read(storage.loadBlob(oid, first)) == b"first"
        assert read(storage.loadBlob(oid, second)) == b"second"


def test_a_missing_blob_raises_poskeyerror(tmp_path):
    from ZODB.POSException import POSKeyError

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        tid = store_blob(storage, oid, ZERO, write_temporary(storage, DATA))
        with pytest.raises(POSKeyError):
            storage.loadBlob(oid, after(tid))
        with pytest.raises(POSKeyError):
            storage.loadBlob(storage.new_oid(), tid)


def test_an_aborted_blob_leaves_no_file(tmp_path):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import POSKeyError

    with opened(tmp_path / "store") as storage:
        oid = storage.new_oid()
        path = write_temporary(storage, DATA)
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.storeBlob(oid, ZERO, blob_record(), path, "", txn)
        storage.tpc_abort(txn)

        # The storage took the file, so the abort removes it.
        assert not os.path.exists(path)
        assert committed_blob_files(tmp_path / "store" / "blobs") == []
        with pytest.raises(POSKeyError):
            storage.loadBefore(oid, MAX_TID)
