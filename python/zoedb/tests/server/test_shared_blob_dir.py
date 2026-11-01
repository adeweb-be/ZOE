"""Acceptance test of a blob directory that the server and its clients share.

`zoe serve --shared-blob-dir` (`ZOE_SHARED_BLOB_DIR`) accepts
`storeBlobShared`: a client with `shared_blob_dir=True` and the blob
directory of the server as its `blob_dir` puts the blob file in the
directory of its oid, under a temporary name that ends in `.tmp` or
`.tmp-`, and sends only that name. The server takes the file into the
transaction, so the crash rules of the core hold, and places it at the
BushyLayout path of `(oid, tid)` at `tpc_finish`. A client reads a
committed blob file in place. `--blob-dir` (`ZOE_BLOB_DIR`) puts the blob
directory of the store elsewhere, as the `blob-dir` key of the stanza does.

Without `--shared-blob-dir`, `storeBlobShared` fails the vote with an error
that names the option. A name with a path separator, or a name that is not
a temporary name, fails the vote too, and the server touches no file.
"""

import os

import pytest
import support
from support import ZERO

from zoe.testing import ZoeServer


def client(server, blob_dir, **kw):
    """Open a stock client that shares the blob directory `blob_dir`."""
    return support.client(server, blob_dir=str(blob_dir), shared_blob_dir=True, **kw)


def write_blob(db, name, data):
    from ZODB.blob import Blob

    with db.transaction() as conn:
        blob = Blob()
        with blob.open("w") as f:
            f.write(data)
        conn.root()[name] = blob
    with db.transaction() as conn:
        blob = conn.root()[name]
        blob._p_activate()
        return blob._p_oid, blob._p_serial


def test_clients_write_and_read_blobs_in_the_shared_directory(tmp_path):
    import ZODB
    from ZODB.blob import BushyLayout

    store, blobs = tmp_path / "store", tmp_path / "shared-blobs"
    data = os.urandom(300_000)
    args = ["--blob-dir", str(blobs), "--shared-blob-dir"]
    with ZoeServer(str(store), args=args) as server:
        writer = ZODB.DB(client(server, blobs))
        try:
            oid, serial = write_blob(writer, "blob", data)
        finally:
            writer.close()

        # The file is in the shared directory, at the path of BushyLayout.
        layout = BushyLayout()
        placed = blobs / layout.getBlobFilePath(oid, serial)
        assert placed.read_bytes() == data

        # Another client reads it in place.
        storage = client(server, blobs)
        try:
            assert storage.loadBlob(oid, serial) == str(placed)
        finally:
            storage.close()

    # No temporary file is left in the directory of the oid.
    leftovers = [name for name in os.listdir(placed.parent) if ".tmp" in name]
    assert leftovers == []


def test_store_blob_shared_without_the_option_fails_the_vote(tmp_path):
    import ZODB
    from ZODB.POSException import StorageError

    store, blobs = tmp_path / "store", tmp_path / "store" / "blobs"
    with ZoeServer(str(store)) as server:
        db = ZODB.DB(client(server, blobs))
        try:
            with pytest.raises(StorageError) as caught:
                write_blob(db, "blob", b"refused")
            assert "shared-blob-dir" in str(caught.value)
        finally:
            db.close()


@pytest.mark.parametrize(
    "name", ["../escape.tmp", "sub/dir.tmp", "not-a-temporary-file"]
)
def test_a_bad_file_name_fails_the_vote_and_touches_nothing(tmp_path, name):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import StorageError

    store, blobs = tmp_path / "store", tmp_path / "shared-blobs"
    victim = tmp_path / "escape.tmp"
    victim.write_bytes(b"must stay")
    args = ["--blob-dir", str(blobs), "--shared-blob-dir"]
    with ZoeServer(str(store), args=args) as server:
        storage = client(server, blobs)
        try:
            oid = storage.new_oid()
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.store(oid, ZERO, b"record", "", txn)
            storage._async("storeBlobShared", oid, ZERO, b"record", name, id(txn))
            with pytest.raises(StorageError):
                storage.tpc_vote(txn)
            storage.tpc_abort(txn)
        finally:
            storage.close()
    assert victim.read_bytes() == b"must stay"
