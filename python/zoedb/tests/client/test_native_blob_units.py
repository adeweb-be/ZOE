"""Unit tests of the blobs of the native client.

`just test` runs this module, against `zoe serve` with a blob directory.
The acceptance test is `test_native_blobs.py`.
"""

import os

import pytest
from ZODB.interfaces import IBlobStorage
from ZODB.POSException import POSKeyError
from ZODB.utils import p64, u64, z64

import zoe.testing
import zoe.zeo
from zoe.testing import ZoeServer

TIMEOUT = 30


def native(server, blob_dir, **kw):
    """Open a native client of the storage 1 of `server`."""
    return zoe.testing.native_client(
        server.address,
        storage="1",
        wait_timeout=TIMEOUT,
        blob_dir=str(blob_dir),
        **kw,
    )


def blob_file(directory, data):
    """Write `data` to a new file in `directory` and return its path."""
    path = os.path.join(directory, "new.blob")
    with open(path, "wb") as f:
        f.write(data)
    return path


def commit_blob(storage, data):
    """Commit `data` as the blob of a new oid. Return the oid and the tid."""
    from ZODB.Connection import TransactionMetaData

    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        filename = blob_file(storage.temporaryDirectory(), data)
        storage.storeBlob(oid, z64, b"pickle", filename, "", txn)
        storage.tpc_vote(txn)
        tid = storage.tpc_finish(txn)
    except BaseException:
        storage.tpc_abort(txn)
        raise
    return oid, tid


def blob_files(directory):
    """Return the names of the files under `directory`, but the lock files."""
    found = []
    for base, _, files in os.walk(directory):
        for name in files:
            if name != ".lock" and name != ".layout":
                found.append(os.path.relpath(os.path.join(base, name), directory))
    return sorted(found)


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


def test_the_blob_arguments_are_built():
    for name in ("blob_dir", "shared_blob_dir", "blob_cache_size"):
        assert name not in zoe.zeo._NOT_BUILT
    assert "blob_cache_size_check" not in zoe.zeo._NOT_BUILT


def test_the_client_provides_iblobstorage(server, tmp_path):
    storage = native(server, tmp_path / "cache")
    try:
        assert IBlobStorage.providedBy(storage)
    finally:
        storage.close()


def test_a_commit_puts_its_blob_in_the_cache_of_the_client(server, tmp_path):
    cache = tmp_path / "cache"
    storage = native(server, cache)
    try:
        oid, tid = commit_blob(storage, b"committed")
        filename = storage.fshelper.getBlobFilename(oid, tid)
        assert blob_files(cache) == [os.path.relpath(filename, cache)]
        with open(filename, "rb") as f:
            assert f.read() == b"committed"
        assert storage.loadBlob(oid, tid) == filename
    finally:
        storage.close()


def test_a_failed_download_leaves_no_file_in_the_cache(server, tmp_path):
    writer = native(server, tmp_path / "writer")
    try:
        oid, tid = commit_blob(writer, b"data")
    finally:
        writer.close()
    cache = tmp_path / "cache"
    storage = native(server, cache)
    try:
        missing = p64(u64(tid) + 1)
        with pytest.raises(POSKeyError):
            storage.loadBlob(oid, missing)
        assert blob_files(cache) == []
    finally:
        storage.close()


def test_a_download_writes_again_a_partial_file_that_a_stopped_download_left(
    server, tmp_path
):
    writer = native(server, tmp_path / "writer")
    try:
        oid, tid = commit_blob(writer, b"the whole blob")
    finally:
        writer.close()
    cache = tmp_path / "cache"
    storage = native(server, cache)
    try:
        filename = storage.fshelper.getBlobFilename(oid, tid)
        storage.fshelper.createPathForOID(oid)
        with open(filename + ".dl", "wb") as f:
            f.write(b"a partial file that is longer than the blob")
        assert storage.loadBlob(oid, tid) == filename
        with open(filename, "rb") as f:
            assert f.read() == b"the whole blob"
        assert not os.path.exists(filename + ".dl")
        # The file of the cache is read-only, as ZEO makes it.
        assert os.stat(filename).st_mode & 0o777 == 0o400
    finally:
        storage.close()


def test_an_abort_removes_the_blob_file_that_the_transaction_took(server, tmp_path):
    from ZODB.Connection import TransactionMetaData

    cache = tmp_path / "cache"
    storage = native(server, cache)
    try:
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        filename = blob_file(storage.temporaryDirectory(), b"aborted")
        storage.storeBlob(storage.new_oid(), z64, "pickle", filename, "", txn)
        assert not os.path.exists(filename)
        storage.tpc_abort(txn)
        assert blob_files(cache) == []
    finally:
        storage.close()


def test_the_blob_cache_of_the_native_client_serves_the_client_of_zeo(server, tmp_path):
    import ZEO.ClientStorage

    cache = tmp_path / "cache"
    storage = native(server, cache)
    try:
        oid, tid = commit_blob(storage, b"in the cache")
        filename = storage.fshelper.getBlobFilename(oid, tid)
        inode = os.stat(filename).st_ino
    finally:
        storage.close()
    stock = ZEO.ClientStorage.ClientStorage(
        server.address, "1", wait_timeout=TIMEOUT, blob_dir=str(cache)
    )
    try:
        # The client of ZEO finds the file in place and downloads nothing.
        assert stock.loadBlob(oid, tid) == filename
        assert os.stat(filename).st_ino == inode
    finally:
        stock.close()
