"""Drive a stock ZEO client against a stock ZEO server and write the traces.

Run `python -m zoe.testing.wire_scenarios OUT [NAME...]`. Each scenario
writes OUT/NAME.jsonl: one record per frame, in the order of the proxy.
The scenario `heartbeat` waits for the heartbeat of the client.
"""

import json
import os
import pickle
import socket
import struct
import sys
import tempfile
import time
from pathlib import Path

from zoe.testing.wire import Recorder, stock_server

TIMEOUT = 30
ZERO = bytes(8)


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def counter(value):
    meta = (("ZODB.tests.ConflictResolution", "PCounter"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def client(address, tmp, name, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("blob_dir", str(tmp / f"blobs-{name}"))
    return ClientStorage(address, wait_timeout=TIMEOUT, client_label=name, **kw)


def commit(storage, stores=(), blobs=(), ext=None):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(b"user", b"description", ext or {"note": "spike"})
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        for oid, serial, data, filename in blobs:
            storage.storeBlob(oid, serial, data, filename, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except BaseException:
        storage.tpc_abort(txn)
        raise


def expect(error, call, *args):
    try:
        call(*args)
    except error:
        return
    raise AssertionError(f"{call} did not raise {error}")


def save(path, recorder):
    with open(path, "w") as out:
        for m in recorder.messages:
            item = {
                "connection": m.connection,
                "direction": m.direction,
                "id": repr(m.id),
                "flags": repr(m.flags),
                "method": m.method,
                "args": repr(m.args),
                "raw": m.raw.hex(),
            }
            out.write(json.dumps(item) + "\n")


def plain(tmp, address):
    from ZODB.POSException import (
        ConflictError,
        POSKeyError,
        ReadConflictError,
        StorageTransactionError,
    )
    from ZODB.TimeStamp import TimeStamp

    storage = client(address, tmp, "plain")
    other = client(address, tmp, "other")
    try:
        storage.ping()
        storage.server_status()
        oid = storage.new_oid()
        tid = commit(storage, [(oid, ZERO, record("one"))])
        tid2 = commit(
            storage,
            [(oid, tid, record("two"))],
            ext={"note": "spike", "paths": ["a", "b"]},
        )
        other.load(oid)
        storage.loadSerial(oid, tid)
        storage.history(oid, 2)
        storage.record_iternext()
        with_error(storage, "getInvalidations", tid)
        with_error(storage, "getInvalidations", ZERO)
        with_error(storage, "lastTransaction")
        with_error(storage, "getTid", oid)
        with_error(storage, "new_oid")
        expect(POSKeyError, storage.load, storage.new_oid())
        expect(StorageTransactionError, storage._call, "vote", 1)

        from ZODB.Connection import TransactionMetaData

        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.checkCurrentSerialInTransaction(oid, tid2, txn)
        storage.deleteObject(oid, tid2, txn)
        storage.tpc_abort(txn)

        # A stale serial: ConflictError. A stale read: ReadConflictError.
        expect(ConflictError, commit, storage, [(oid, tid, record("stale"))])
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.checkCurrentSerialInTransaction(oid, tid, txn)
        expect(ReadConflictError, storage.tpc_vote, txn)
        storage.tpc_abort(txn)

        # A conflict that the server resolves: vote returns the oid.
        coid = storage.new_oid()
        ctid = commit(storage, [(coid, ZERO, counter(1))])
        commit(storage, [(coid, ctid, counter(2))])
        commit(storage, [(coid, ctid, counter(3))])

        # Blobs through the wire.
        blob_file = tmp / "blob.bin"
        blob_file.write_bytes(os.urandom(60_000))
        boid = storage.new_oid()
        btid = commit(storage, [], [(boid, ZERO, record("blob"), str(blob_file))])
        reader = client(address, tmp, "reader")
        try:
            reader.loadBlob(boid, btid)
        finally:
            reader.close()

        # The iterator, its records, and an iterator dropped early.
        for transaction in storage.iterator():
            list(transaction)
        dropped = storage.iterator()
        next(dropped)
        del dropped
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.tpc_abort(txn)

        # Pack needs the root object. Without it, the reply is an error.
        expect(KeyError, storage.pack, TimeStamp(tid2).timeTime() + 1, None)
        commit(storage, [(ZERO, ZERO, record("root"))])
        storage.pack(TimeStamp(tid2).timeTime() + 1, None)
    finally:
        other.close()
        storage.close()


def with_error(storage, method, *args):
    try:
        storage._call(method, *args)
    except Exception as exc:
        return exc
    return None


def unregistered(tmp, address):
    from ZEO.Exceptions import ClientDisconnected

    storage = client(address, tmp, "unregistered")
    try:
        try:
            storage._call("get_size_info", timeout=5)
        except ClientDisconnected:
            pass
    finally:
        storage.close()
    # ruok: the protocol frame is the request.
    with socket.create_connection(address) as sock:
        sock.sendall(struct.pack(">I", 4) + b"ruok")
        sock.settimeout(10)
        while sock.recv(65536):
            pass


def copy(tmp, address):
    import ZODB.FileStorage

    source = ZODB.FileStorage.FileStorage(str(tmp / "source.fs"))
    try:
        commit(source, [(source.new_oid(), ZERO, record("copied"))])
        target = client(address, tmp, "copy")
        try:
            target.copyTransactionsFrom(source)
        finally:
            target.close()
    finally:
        source.close()


def resolve(tmp, address):
    storage = client(address, tmp, "resolve")
    try:
        oid = storage.new_oid()
        tid = commit(storage, [(oid, ZERO, counter(1))])
        commit(storage, [(oid, tid, counter(2))])
        commit(storage, [(oid, tid, counter(3))])
    finally:
        storage.close()


def shared(tmp, address, blob_dir):
    storage = client(address, tmp, "shared", blob_dir=blob_dir, shared_blob_dir=True)
    try:
        blob_file = tmp / "shared.bin"
        blob_file.write_bytes(b"shared blob")
        boid = storage.new_oid()
        commit(storage, [], [(boid, ZERO, record("shared"), str(blob_file))])
    finally:
        storage.close()


def read_only(tmp, address):
    storage = client(address, tmp, "read-only", read_only_fallback=True)
    try:
        storage.ping()
        with_error(storage, "new_oids")
    finally:
        storage.close()


def heartbeat(tmp, address):
    storage = client(address, tmp, "heartbeat")
    try:
        time.sleep(65)
        storage.ping()
    finally:
        storage.close()


SCENARIOS = {
    "plain": ({}, plain),
    "unregistered": ({}, unregistered),
    "copy": ({}, copy),
    "client-resolution": ({"client_conflict_resolution": True}, resolve),
    "shared-blobs": ({"shared_blob_dir": True}, shared),
    "read-only": ({"read_only": True}, read_only),
    "heartbeat": ({}, heartbeat),
}


def main(argv):
    out = Path(argv[0])
    out.mkdir(parents=True, exist_ok=True)
    names = argv[1:] or [n for n in SCENARIOS if n != "heartbeat"]
    for name in names:
        options, run = SCENARIOS[name]
        with tempfile.TemporaryDirectory() as tmp:
            tmp = Path(tmp)
            extra = ()
            if options.get("shared_blob_dir"):
                options = dict(options, blob_dir=str(tmp / "shared-blobs"))
                extra = (options["blob_dir"],)
            with stock_server(tmp / "server", **options) as address:
                with Recorder(address) as recorder:
                    run(tmp, recorder.address, *extra)
                    time.sleep(0.5)
                    save(out / f"{name}.jsonl", recorder)
        print(name, "done", flush=True)


if __name__ == "__main__":
    main(sys.argv[1:])
