"""Acceptance test of read-only serving.

`zoe serve --read-only` (`ZOE_READ_ONLY`) opens each store with the
read-only open of the core: it changes no byte of the store and holds its
lock, so no writer opens it meanwhile. A store of the configuration file
with `read-only = true` is read-only alone. A read-only store serves reads.
A registration that asks to write fails with `ReadOnlyError`, so a client
with `read_only_fallback` registers again as read-only. `tpc_begin`,
`new_oids` and `pack` fail with `ReadOnlyError`.
"""

import hashlib
import os
import pickle

import pytest

import zoe
from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def client(server, name="1", **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage=name, **kw)


def make_store(path):
    """Write one object in-process. Return its oid and serial."""
    from ZODB.Connection import TransactionMetaData

    storage = zoe.Storage(str(path))
    try:
        oid = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, ZERO, record("kept"), "", txn)
        storage.tpc_vote(txn)
        tid = storage.tpc_finish(txn)
    finally:
        storage.close()
    return oid, tid


def digest(directory):
    """A digest of every file of a directory: names and bytes."""
    h = hashlib.sha256()
    for root, dirs, files in sorted(os.walk(directory)):
        dirs.sort()
        for name in sorted(files):
            full = os.path.join(root, name)
            h.update(os.path.relpath(full, directory).encode())
            with open(full, "rb") as f:
                h.update(f.read())
    return h.hexdigest()


def test_a_read_only_server_serves_reads_and_changes_nothing(tmp_path):
    path = tmp_path / "store"
    oid, tid = make_store(path)
    before = digest(path)
    with ZoeServer(str(path), args=["--read-only"]) as server:
        storage = client(server, read_only=True)
        try:
            assert storage.isReadOnly()
            assert storage.load(oid) == (record("kept"), tid)
            assert storage.lastTransaction() == tid
        finally:
            storage.close()
        # The server holds the lock of the store.
        from ZODB.POSException import StorageError

        with pytest.raises(StorageError):
            zoe.Storage(str(path))
        assert server.stop() == 0
    assert digest(path) == before


def test_a_writing_client_falls_back_to_read_only(tmp_path):
    path = tmp_path / "store"
    oid, tid = make_store(path)
    with ZoeServer(str(path), args=["--read-only"]) as server:
        storage = client(server, read_only_fallback=True)
        try:
            assert storage.isReadOnly()
            assert storage.load(oid) == (record("kept"), tid)
        finally:
            storage.close()


def test_a_writing_client_without_fallback_is_refused(tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    path = tmp_path / "store"
    make_store(path)
    with ZoeServer(str(path), args=["--read-only"]) as server:
        with pytest.raises(ClientDisconnected):
            client(server, wait_timeout=2)


def test_write_calls_fail_with_read_only_error(tmp_path):
    from ZODB.POSException import ReadOnlyError

    path = tmp_path / "store"
    oid, tid = make_store(path)
    with ZoeServer(str(path), args=["--read-only"]) as server:
        storage = client(server, read_only=True)
        try:
            with pytest.raises(ReadOnlyError):
                storage._call("new_oids", 1)
            with pytest.raises(ReadOnlyError):
                storage._call("pack", 0.0, 1)
            # tpc_begin has no reply; its error comes at the vote.
            storage._async("tpc_begin", 1, b"", b"", {}, None, " ")
            storage._async("storea", oid, tid, record("refused"), 1)
            with pytest.raises(ReadOnlyError):
                storage._call("vote", 1)
            assert storage.load(oid) == (record("kept"), tid)
        finally:
            storage.close()


def test_one_store_of_the_configuration_is_read_only(tmp_path):
    from ZODB.POSException import ReadOnlyError

    archive, live = tmp_path / "archive", tmp_path / "live"
    oid, tid = make_store(archive)
    config = tmp_path / "zoe.toml"
    config.write_text(
        f'[storages.archive]\npath = "{archive}"\nread-only = true\n\n'
        f'[storages.live]\npath = "{live}"\n'
    )
    with ZoeServer(args=["--config", str(config)]) as server:
        reader = client(server, "archive", read_only_fallback=True)
        writer = client(server, "live")
        try:
            assert reader.isReadOnly()
            assert reader.load(oid) == (record("kept"), tid)
            with pytest.raises(ReadOnlyError):
                reader._call("new_oids", 1)
            assert not writer.isReadOnly()
            writer.new_oid()
        finally:
            reader.close()
            writer.close()
