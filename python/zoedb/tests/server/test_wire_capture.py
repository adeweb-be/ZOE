"""Acceptance test of the spike on the ZEO 6 wire contract.

The spike records what a stock ZEO 6 server and an unmodified
`ZEO.ClientStorage` send each other over msgpack, for each method of the
protocol compatibility matrix of the specification and each call from the
server to the client. The finding, `docs/spikes/zeo-wire.md`, gives the
form of each message from these records: the handshake, the framing, the
message ids, the arguments and the result of each method, the error
replies, and the calls from the server.

The spike code stays on the card branch. It gives `zoe.testing.wire`:

- `stock_server(directory, **options)`: a context manager that runs a stock
  ZEO 6 server with msgpack on a FileStorage in `directory` and yields its
  address. The options are those of ZEO's server configuration, for example
  `client_conflict_resolution=True`, and `shared_blob_dir=True` for a
  shared blob directory.
- `Recorder(address)`: a TCP proxy to `address`, as a context manager. A
  client connects to `recorder.address`. `recorder.messages` lists each
  frame that passed, decoded: `direction` ("client" or "server"), `id`,
  `flags`, `method` and `args`, and `raw`, the bytes of the frame. The
  first frame of each side is the protocol, with `method` `None`.

With `ZOE_WIRE_TRACES` set to a directory, the test writes the records of
each part there, one file per part, for the finding.
"""

import json
import os
import pickle

import pytest

from zoe.testing.wire import Recorder, stock_server

TIMEOUT = 30
ZERO = bytes(8)

# The methods of the compatibility matrix that phase 2 serves, and the
# methods that a stock server registers beside them. Undo and the ZOE
# extensions are out.
CLIENT_METHODS = {
    # handshake
    "register",
    "get_info",
    "set_client_label",
    "ping",
    "server_status",
    "lastTransaction",
    # reads
    "loadBefore",
    "loadSerial",
    "getInvalidations",
    "history",
    "record_iternext",
    # commit
    "tpc_begin",
    "storea",
    "vote",
    "tpc_finish",
    "tpc_abort",
    "checkCurrentSerialInTransaction",
    "deleteObject",
    "new_oids",
    # blobs
    "storeBlobStart",
    "storeBlobChunk",
    "storeBlobEnd",
    "storeBlobShared",
    "sendBlob",
    # copy and backup
    "restorea",
    "iterator_start",
    "iterator_next",
    "iterator_record_start",
    "iterator_record_next",
    "iterator_gc",
    # maintenance
    "pack",
}

# The calls from the server to the client.
SERVER_METHODS = {
    "invalidateTransaction",
    "info",
    "receiveBlobStart",
    "receiveBlobChunk",
    "receiveBlobStop",
}


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def counter(value):
    """A record of a class with `_p_resolveConflict`, for the conflicts."""
    meta = (("ZODB.tests.ConflictResolution", "PCounter"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def commit(storage, stores, blobs=()):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(b"user", b"description", {"note": "spike"})
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        for oid, serial, data, filename in blobs:
            storage.storeBlob(oid, serial, data, filename, "", txn)
        storage.tpc_vote(txn)
    except BaseException:
        # The client holds its commit lock until the abort.
        storage.tpc_abort(txn)
        raise
    return storage.tpc_finish(txn)


def client(address, tmp_path, name, **kw):
    from ZEO.ClientStorage import ClientStorage

    return ClientStorage(
        address,
        wait_timeout=TIMEOUT,
        blob_dir=str(tmp_path / f"blobs-{name}"),
        client_label=name,
        **kw,
    )


def save(name, recorder):
    traces = os.environ.get("ZOE_WIRE_TRACES")
    if not traces:
        return
    os.makedirs(traces, exist_ok=True)
    with open(os.path.join(traces, f"{name}.jsonl"), "w") as out:
        for m in recorder.messages:
            item = {
                "direction": m.direction,
                "id": repr(m.id),
                "flags": repr(m.flags),
                "method": m.method,
                "args": repr(m.args),
                "raw": m.raw.hex(),
            }
            out.write(json.dumps(item) + "\n")


def methods(recorder, direction):
    return {m.method for m in recorder.messages if m.direction == direction} - {
        None,
        ".reply",
    }


def test_the_records_cover_every_method_of_the_matrix(tmp_path):
    from ZODB.POSException import ConflictError
    from ZODB.TimeStamp import TimeStamp

    seen = {"client": set(), "server": set()}

    # Plain FileStorage, blobs through the wire.
    with stock_server(tmp_path / "plain") as address, Recorder(address) as recorder:
        storage = client(recorder.address, tmp_path, "plain")
        other = client(recorder.address, tmp_path, "other")
        try:
            storage.ping()
            storage.server_status()
            oid = storage.new_oid()
            tid = commit(storage, [(oid, ZERO, record("one"))])
            # The other client gets invalidateTransaction.
            tid2 = commit(storage, [(oid, tid, record("two"))])
            other.load(oid)
            storage.loadSerial(oid, tid)
            storage.history(oid, 2)
            storage.record_iternext()
            # ClientStorage has no method for these two calls.
            storage._call("getInvalidations", tid)
            storage._call("lastTransaction")

            # read-current, deleteObject, abort
            from ZODB.Connection import TransactionMetaData

            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.checkCurrentSerialInTransaction(oid, tid2, txn)
            storage.deleteObject(oid, tid2, txn)
            storage.tpc_abort(txn)

            # A conflict: a stale serial gives ConflictError with serials.
            with pytest.raises(ConflictError):
                commit(storage, [(oid, tid, record("stale"))])

            # Blobs: upload through chunks, download through sendBlob.
            blob_file = tmp_path / "blob.bin"
            blob_file.write_bytes(os.urandom(200_000))
            boid = storage.new_oid()
            btid = commit(
                storage,
                [],
                [(boid, ZERO, record("blob"), str(blob_file))],
            )
            reader = client(recorder.address, tmp_path, "reader")
            try:
                assert (
                    open(reader.loadBlob(boid, btid), "rb").read()
                    == open(storage.loadBlob(boid, btid), "rb").read()
                )
            finally:
                reader.close()

            # The iterator and its records.
            transactions = 0
            for transaction in storage.iterator():
                list(transaction)
                transactions += 1
            assert transactions
            # An iterator dropped before its end. The client sends
            # iterator_gc with its next transaction.
            dropped = storage.iterator()
            next(dropped)
            del dropped
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.tpc_abort(txn)

            # pack, which sends info to the clients. Pack needs the root
            # object.
            commit(storage, [(ZERO, ZERO, record("root"))])
            storage.pack(TimeStamp(tid2).timeTime() + 1, None)
        finally:
            other.close()
            storage.close()
        save("plain", recorder)
        seen["client"] |= methods(recorder, "client")
        seen["server"] |= methods(recorder, "server")
        # The handshake: the server announces its protocol, the client
        # answers it, and both use msgpack.
        first = [m for m in recorder.messages if m.method is None]
        assert [m.direction for m in first[:2]] == ["server", "client"]
        assert first[0].raw.startswith(b"M")

    # restorea: copy a transaction into an empty server.
    with stock_server(tmp_path / "copy") as address, Recorder(address) as recorder:
        import ZODB.FileStorage

        source = ZODB.FileStorage.FileStorage(str(tmp_path / "source.fs"))
        try:
            commit(source, [(source.new_oid(), ZERO, record("copied"))])
            target = client(recorder.address, tmp_path, "copy")
            try:
                target.copyTransactionsFrom(source)
            finally:
                target.close()
        finally:
            source.close()
        save("copy", recorder)
        seen["client"] |= methods(recorder, "client")

    # Client conflict resolution: the vote reply carries the conflicts.
    with (
        stock_server(tmp_path / "resolve", client_conflict_resolution=True) as address,
        Recorder(address) as recorder,
    ):
        storage = client(recorder.address, tmp_path, "resolve")
        try:
            oid = storage.new_oid()
            tid = commit(storage, [(oid, ZERO, counter(1))])
            commit(storage, [(oid, tid, counter(2))])
            try:
                commit(storage, [(oid, tid, counter(3))])
            except ConflictError:
                pass
        finally:
            storage.close()
        save("client-resolution", recorder)
        seen["client"] |= methods(recorder, "client")
        votes = [m for m in recorder.messages if m.method == "vote"]
        assert votes, "no vote was recorded"

    # A shared blob directory: storeBlobShared.
    shared = tmp_path / "shared-blobs"
    with (
        stock_server(
            tmp_path / "shared", blob_dir=str(shared), shared_blob_dir=True
        ) as address,
        Recorder(address) as recorder,
    ):
        from ZEO.ClientStorage import ClientStorage

        storage = ClientStorage(
            recorder.address,
            wait_timeout=TIMEOUT,
            blob_dir=str(shared),
            shared_blob_dir=True,
        )
        try:
            blob_file = tmp_path / "shared.bin"
            blob_file.write_bytes(b"shared blob")
            boid = storage.new_oid()
            commit(storage, [], [(boid, ZERO, record("shared"), str(blob_file))])
        finally:
            storage.close()
        save("shared-blobs", recorder)
        seen["client"] |= methods(recorder, "client")

    missing_client = CLIENT_METHODS - seen["client"]
    missing_server = SERVER_METHODS - seen["server"]
    assert not missing_client, f"no record of the client methods {missing_client}"
    assert not missing_server, f"no record of the server calls {missing_server}"


def test_the_records_show_the_forms_that_the_finding_describes(tmp_path):
    """The forms that the walking skeleton already relies on."""
    with stock_server(tmp_path / "forms") as address, Recorder(address) as recorder:
        storage = client(recorder.address, tmp_path, "forms")
        try:
            oid = storage.new_oid()
            commit(storage, [(oid, ZERO, record("x"))])
            storage._cache.clear()
            storage.load(oid)
            with pytest.raises(Exception):  # noqa: B017
                storage.load(storage.new_oid())
        finally:
            storage.close()

    loads = [
        m
        for m in recorder.messages
        if m.direction == "client" and m.method == "loadBefore"
    ]
    assert loads
    # The stock client sends loadBefore with the message id (oid, tid).
    assert all(isinstance(m.id, (list, tuple)) for m in loads)
    # An error reply has the flags 2 and the args [class, args].
    errors = [m for m in recorder.messages if m.method == ".reply" and m.flags == 2]
    assert errors
    assert errors[0].args[0] == "ZODB.POSException.POSKeyError"
