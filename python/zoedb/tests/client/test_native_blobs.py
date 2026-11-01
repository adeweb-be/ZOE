"""Acceptance test of blobs through the native client.

`just test` runs this module. The native client handles blobs as
`ZEO.ClientStorage` does, in both of its modes.

- With `blob_dir` and no `shared_blob_dir`, the directory is a blob cache
  in the layout `zeocache` of ZEO. `loadBlob` gives the path of the file in
  the cache. It downloads the file first when the cache does not hold it,
  and never again after that. Two downloads of one blob never write the
  same file at once, so a reader never opens a partial file.
  `blob_cache_size` and `blob_cache_size_check` prune the cache as ZEO
  does.
- With `shared_blob_dir`, the client and the server share the directory. A
  commit puts the file there, and `loadBlob` reads it in place.
- `openCommittedBlobFile` gives the committed bytes.

The tests open the native client through `zoe.testing.native_client`.
"""

import os
import threading
import time

import pytest
from client_support import blob_native as native
from support import TIMEOUT

from zoe.testing import ZoeServer

# Larger than many chunks of 64 KiB, and not a multiple of it.
LARGE = 3 * 1024 * 1024 + 12345
# The target size of the blob cache for the pruning test.
CACHE_SIZE = 1_000_000
# Sixteen blobs of this size make 4 MiB.
PRUNED_BLOB = 256 * 1024


@pytest.fixture
def path(tmp_path):
    return tmp_path / "store"


def write_blobs(db, blobs):
    """Commit each blob of `blobs` (name: bytes) in the root.

    Return {name: (oid, serial)}.
    """
    from ZODB.blob import Blob

    with db.transaction() as conn:
        for name, data in blobs.items():
            blob = Blob()
            with blob.open("w") as f:
                f.write(data)
            conn.root()[name] = blob
    keys = {}
    with db.transaction() as conn:
        for name in blobs:
            blob = conn.root()[name]
            blob._p_activate()
            keys[name] = (blob._p_oid, blob._p_serial)
    return keys


def commit_blobs(server, blob_dir, blobs, shared=False):
    """Commit `blobs` through a native client. Return {name: (oid, serial)}."""
    import ZODB

    db = ZODB.DB(native(server, blob_dir, shared))
    try:
        return write_blobs(db, blobs)
    finally:
        db.close()


def read(filename):
    with open(filename, "rb") as f:
        return f.read()


def cache_bytes(blob_dir):
    """Return the bytes of the blob files in a blob cache, as ZEO counts them."""
    total = 0
    for base, _, files in os.walk(blob_dir):
        for name in files:
            if name.endswith(".blob"):
                try:
                    total += os.stat(os.path.join(base, name)).st_size
                except FileNotFoundError:
                    # The pruning removed the file during the walk.
                    pass
    return total


def wait_until(condition, message):
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(message())
        time.sleep(0.05)


def test_a_second_load_blob_downloads_nothing(path, tmp_path):
    import ZEO.ClientStorage  # noqa: F401 (it registers the layout zeocache)
    import ZODB
    from ZODB.blob import FilesystemHelper

    data = os.urandom(LARGE)
    with ZoeServer(str(path)) as server:
        keys = commit_blobs(server, tmp_path / "cache-writer", {"blob": data})
        oid, serial = keys["blob"]

        cache = tmp_path / "cache-reader"
        storage = native(server, cache)
        db = ZODB.DB(storage)
        try:
            # Another native client with a blob cache opens the blob.
            with db.transaction() as conn, conn.root()["blob"].open("r") as f:
                assert f.read() == data

            filename = storage.loadBlob(oid, serial)
            assert read(filename) == data
            # The cache has the layout of ZEO's blob cache.
            assert (cache / ".layout").read_text().strip() == "zeocache"
            helper = FilesystemHelper(str(cache), layout_name="zeocache")
            assert filename == helper.getBlobFilename(oid, serial)
            first = os.stat(filename)

            again = storage.loadBlob(oid, serial)
            second = os.stat(again)
        finally:
            db.close()

    # The file of the cache is the same file: no download wrote it again.
    # ZEO sets the time of access with a float, so the modification time
    # can move by less than one microsecond.
    assert again == filename
    assert second.st_ino == first.st_ino
    assert second.st_size == first.st_size == len(data)
    assert abs(second.st_mtime_ns - first.st_mtime_ns) < 1_000


def test_two_clients_that_download_one_blob_at_once_both_get_the_whole_file(
    path, tmp_path
):
    blobs = {f"blob-{n}": os.urandom(8 * 1024 * 1024 + n) for n in range(3)}
    with ZoeServer(str(path)) as server:
        keys = commit_blobs(server, tmp_path / "cache-writer", blobs)

        # The two clients share one blob cache, as two processes of one
        # machine do.
        cache = tmp_path / "cache-shared-by-two"
        clients = [native(server, cache), native(server, cache)]
        try:
            for name, data in blobs.items():
                oid, serial = keys[name]
                results = download_together(clients, oid, serial)
                for result in results:
                    assert not isinstance(result, Exception), result
                    filename, got = result
                    assert len(got) == len(data)
                    assert got == data
                assert results[0][0] == results[1][0]
                assert read(results[0][0]) == data
        finally:
            for storage in clients:
                storage.close()


def download_together(clients, oid, serial):
    """Call `loadBlob(oid, serial)` on each client at the same time.

    Each thread reads the file as soon as `loadBlob` returns, also while
    another client still downloads. Return, for each client, the file name
    and the bytes that the thread read, or the error.
    """
    barrier = threading.Barrier(len(clients), timeout=TIMEOUT)
    results = [None] * len(clients)

    def download(index, storage):
        try:
            barrier.wait()
            filename = storage.loadBlob(oid, serial)
            results[index] = (filename, read(filename))
        except Exception as error:
            results[index] = error

    threads = [
        threading.Thread(target=download, args=(index, storage))
        for index, storage in enumerate(clients)
    ]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(TIMEOUT)
    assert not any(thread.is_alive() for thread in threads), "a download hangs"
    return results


def test_the_blob_cache_is_pruned_to_its_size(path, tmp_path):
    blobs = {f"blob-{n}": os.urandom(PRUNED_BLOB) for n in range(16)}
    with ZoeServer(str(path)) as server:
        keys = commit_blobs(server, tmp_path / "cache-writer", blobs)

        cache = tmp_path / "cache-pruned"
        storage = native(server, cache, blob_cache_size=CACHE_SIZE)
        try:
            for name, data in blobs.items():
                oid, serial = keys[name]
                # The pruning can remove a file of the cache at any time.
                # openCommittedBlobFile downloads it again then, as in ZEO.
                with storage.openCommittedBlobFile(oid, serial) as f:
                    assert f.read() == data
            # The client prunes the cache in the background, as ZEO does.
            wait_until(
                lambda: cache_bytes(cache) <= CACHE_SIZE,
                lambda: f"the blob cache holds {cache_bytes(cache)} bytes",
            )
        finally:
            storage.close()
    assert cache_bytes(cache) <= CACHE_SIZE


def test_a_shared_blob_directory_holds_the_committed_file(tmp_path):
    import ZODB
    from ZODB.blob import BushyLayout

    store, shared = tmp_path / "store", tmp_path / "shared-blobs"
    data = os.urandom(300_000)
    args = ["--blob-dir", str(shared), "--shared-blob-dir"]
    with ZoeServer(str(store), args=args) as server:
        keys = commit_blobs(server, shared, {"blob": data}, shared=True)
        oid, serial = keys["blob"]

        # The commit put the file in the shared directory, at the path of
        # BushyLayout.
        placed = shared / BushyLayout().getBlobFilePath(oid, serial)
        assert placed.read_bytes() == data

        # Another client reads it there.
        storage = native(server, shared, shared=True)
        db = ZODB.DB(storage)
        try:
            assert storage.loadBlob(oid, serial) == str(placed)
            with db.transaction() as conn, conn.root()["blob"].open("r") as f:
                assert f.read() == data
        finally:
            db.close()


def test_open_committed_blob_file_gives_the_committed_bytes(path, tmp_path):
    from ZODB.blob import Blob, BlobFile

    data = os.urandom(LARGE)
    with ZoeServer(str(path)) as server:
        keys = commit_blobs(server, tmp_path / "cache-writer", {"blob": data})
        oid, serial = keys["blob"]

        storage = native(server, tmp_path / "cache-reader")
        try:
            # The cache does not hold the blob yet.
            with storage.openCommittedBlobFile(oid, serial) as f:
                assert f.read() == data
            with storage.openCommittedBlobFile(oid, serial, Blob()) as f:
                assert isinstance(f, BlobFile)
                assert f.read() == data
        finally:
            storage.close()
