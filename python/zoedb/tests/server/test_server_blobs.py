"""Acceptance test of blob streaming through `zoe serve`.

A client without a shared blob directory uploads a blob with
`storeBlobStart`, `storeBlobChunk` and `storeBlobEnd`. The server writes the
chunks to a temporary file of its blob directory and stages it with the
transaction, so the crash rules of the core hold. A client downloads a blob
that its cache does not hold with `sendBlob`: the server streams the file
in chunks of 64 KiB with `receiveBlobStart`, `receiveBlobChunk` and
`receiveBlobStop`. The server checks the size and the hash of the file as
it reads it.
"""

import os

import pytest

from zoe.testing import ZoeServer

TIMEOUT = 30
# Larger than many chunks of 64 KiB, and not a multiple of it.
LARGE = 3 * 1024 * 1024 + 12345


def client(server, blob_dir, **kw):
    from ZEO.ClientStorage import ClientStorage

    return ClientStorage(
        server.address,
        storage="1",
        wait_timeout=TIMEOUT,
        blob_dir=str(blob_dir),
        shared_blob_dir=False,
        **kw,
    )


@pytest.fixture
def path(tmp_path):
    return tmp_path / "store"


def write_blob(db, name, data):
    from ZODB.blob import Blob

    with db.transaction() as conn:
        blob = Blob()
        with blob.open("w") as f:
            f.write(data)
        conn.root()[name] = blob


def read_blob(db, name):
    with db.transaction() as conn, conn.root()[name].open("r") as f:
        return f.read()


def test_a_blob_goes_up_and_comes_back_down(path, tmp_path):
    import ZODB

    data = os.urandom(LARGE)
    with ZoeServer(str(path)) as server:
        writer = ZODB.DB(client(server, tmp_path / "cache-writer"))
        try:
            write_blob(writer, "blob", data)
        finally:
            writer.close()

        # A client with an empty cache downloads the blob from the server.
        reader = ZODB.DB(client(server, tmp_path / "cache-reader"))
        try:
            assert read_blob(reader, "blob") == data
        finally:
            reader.close()


def test_load_blob_gives_a_file_in_the_client_cache(path, tmp_path):
    import ZODB

    with ZoeServer(str(path)) as server:
        db = ZODB.DB(client(server, tmp_path / "cache-writer"))
        try:
            write_blob(db, "blob", b"small blob")
            with db.transaction() as conn:
                oid, serial = conn.root()["blob"]._p_oid, conn.root()["blob"]._p_serial
        finally:
            db.close()

        cache = tmp_path / "cache-loader"
        storage = client(server, cache)
        try:
            filename = storage.loadBlob(oid, serial)
            assert filename.startswith(str(cache))
            with open(filename, "rb") as f:
                assert f.read() == b"small blob"
            with storage.openCommittedBlobFile(oid, serial) as f:
                assert f.read() == b"small blob"
        finally:
            storage.close()


def test_the_server_keeps_the_blob_in_its_own_blob_directory(path, tmp_path):
    import ZODB
    from ZODB.blob import BushyLayout

    with ZoeServer(str(path)) as server:
        db = ZODB.DB(client(server, tmp_path / "cache"))
        try:
            write_blob(db, "blob", b"kept on the server")
            with db.transaction() as conn:
                oid, serial = conn.root()["blob"]._p_oid, conn.root()["blob"]._p_serial
        finally:
            db.close()

    blobs = path / "blobs"
    assert (blobs / ".layout").read_text().strip() == "bushy"
    layout = BushyLayout()
    filename = blobs / layout.getBlobFilePath(oid, serial)
    assert filename.read_bytes() == b"kept on the server"


def test_an_aborted_blob_leaves_no_file_on_the_server(path, tmp_path):
    import transaction
    import ZODB
    from ZODB.blob import Blob

    with ZoeServer(str(path)) as server:
        db = ZODB.DB(client(server, tmp_path / "cache"))
        try:
            tm = transaction.TransactionManager()
            conn = db.open(transaction_manager=tm)
            blob = Blob()
            with blob.open("w") as f:
                f.write(os.urandom(200_000))
            conn.root()["blob"] = blob
            # The vote uploads the blob, then the transaction aborts.
            tm.get().join(_FailingResource())
            with pytest.raises(RuntimeError):
                tm.commit()
            tm.abort()
            conn.close()
        finally:
            db.close()

    blob_files = [
        name
        for _, _, files in os.walk(path / "blobs")
        for name in files
        if name != ".layout"
    ]
    assert blob_files == []


class _FailingResource:
    """A resource that fails its vote, after the storage voted."""

    def sortKey(self):
        return "~~~ last"

    def tpc_begin(self, txn):
        pass

    def commit(self, txn):
        pass

    def tpc_vote(self, txn):
        raise RuntimeError("the vote of the second resource fails")

    def tpc_finish(self, txn):
        pass

    def tpc_abort(self, txn):
        pass

    def abort(self, txn):
        pass
