"""Acceptance test of the handshake, status and read methods of `zoe serve`.

An unmodified `ZEO.ClientStorage` gets from `zoe serve`:

- `get_info` at registration: `length`, `size`, `name`, `supportsUndo`
  (false until phase 4), `supports_record_iternext` and `interfaces`, the
  storage interfaces that the store provides;
- `get_size_info`, `server_status` with the keys of ZEO's own server, the
  `ruok` status protocol, `ping` and `set_client_label`;
- `history`, `loadSerial` and `record_iternext`, with the errors of ZODB.

The forms come from ZEO 6's own server, so ZEO's doctests of these methods
pass against `zoe serve`.
"""

import json
import pickle
import socket
import struct

import pytest

from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30

# The keys of `server_status` of ZEO 6's server, and of each storage in a
# `ruok` reply.
STATUS_KEYS = {
    "aborts",
    "active_txns",
    "commits",
    "conflicts",
    "conflicts_resolved",
    "connections",
    "last-transaction",
    "loads",
    "lock_time",
    "start",
    "stores",
    "timeout-thread-is-alive",
    "waiting",
}


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def client(server, **kw):
    from ZEO.ClientStorage import ClientStorage

    return ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT, **kw)


def commit(storage, stores, user=b"user", description=b"description"):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(user, description)
    storage.tpc_begin(txn)
    for oid, serial, data in stores:
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


@pytest.fixture
def storage(server):
    storage = client(server, client_label="reads-test")
    yield storage
    storage.close()


def test_get_info_describes_the_store(storage):
    import ZODB.interfaces

    info = storage._info
    assert info["supportsUndo"] is False
    assert storage.supportsUndo() is False
    assert info["supports_record_iternext"] is True
    assert isinstance(info["name"], str)
    assert isinstance(info["length"], int)
    assert isinstance(info["size"], int)
    interfaces = {tuple(item) for item in info["interfaces"]}
    for name in [
        "IStorage",
        "IStorageIteration",
        "IStorageCurrentRecordIteration",
        "IStorageRestoreable",
    ]:
        assert ("ZODB.interfaces", name) in interfaces, name
    assert ("ZODB.interfaces", "IStorageUndoable") not in interfaces
    # ClientStorage takes the interfaces of the server as its own.
    assert ZODB.interfaces.IStorageIteration.providedBy(storage)
    assert not ZODB.interfaces.IStorageUndoable.providedBy(storage)


def test_size_info_and_length(storage):
    commit(storage, [(storage.new_oid(), ZERO, record("x"))])
    info = storage._call("get_size_info")
    assert set(info) == {"length", "size"}
    assert info["size"] > 0
    assert storage.getSize() >= 0
    assert len(storage) >= 0


def test_server_status_has_the_keys_of_zeo(storage):
    tid = commit(storage, [(storage.new_oid(), ZERO, record("x"))])
    status = storage.server_status()
    assert set(status) == STATUS_KEYS
    assert status["connections"] == 1
    assert status["commits"] >= 1
    assert status["last-transaction"] == tid.hex()


def test_ping_answers(storage):
    assert storage._call("ping") is None


def test_ruok_gives_the_status_of_each_storage_and_closes(server, storage):
    commit(storage, [(storage.new_oid(), ZERO, record("x"))])
    with socket.create_connection(server.address, timeout=TIMEOUT) as sock:

        def read_frame():
            size = struct.unpack(">I", sock.recv(4, socket.MSG_WAITALL))[0]
            return sock.recv(size, socket.MSG_WAITALL)

        sock.sendall(struct.pack(">I", 4) + b"ruok")
        assert read_frame() == b"M5"
        data = json.loads(read_frame().decode("ascii"))
        assert set(data) == {"1"}
        assert set(data["1"]) == STATUS_KEYS
        # The server closes the connection after the status.
        assert sock.recv(1) == b""


def test_history_is_newest_first_and_bounded(storage):
    oid = storage.new_oid()
    tids = []
    serial = ZERO
    for n in range(4):
        serial = commit(
            storage,
            [(oid, serial, record(n))],
            user=f"user-{n}".encode(),
            description=f"revision {n}".encode(),
        )
        tids.append(serial)

    history = storage.history(oid, size=3)
    assert [entry["tid"] for entry in history] == tids[::-1][:3]
    assert history[0]["user_name"] == b"user-3"
    assert history[0]["description"] == b"revision 3"
    assert history[0]["size"] == len(record(3))
    assert storage.history(oid, size=10)[-1]["tid"] == tids[0]


def test_load_serial_reads_one_revision(storage):
    from ZODB.POSException import POSKeyError

    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record("first"))])
    commit(storage, [(oid, first, record("second"))])
    assert storage.loadSerial(oid, first) == record("first")
    with pytest.raises(POSKeyError):
        storage.loadSerial(oid, ZERO)


def test_record_iternext_visits_each_current_object_once(storage):
    oids = [storage.new_oid() for _ in range(5)]
    tid = commit(storage, [(oid, ZERO, record(n)) for n, oid in enumerate(oids)])
    # A later revision of one object is its current record.
    commit(storage, [(oids[2], tid, record("later"))])

    seen = {}
    next_oid = None
    while True:
        oid, serial, data, next_oid = storage.record_iternext(next_oid)
        seen[oid] = data
        if next_oid is None:
            break
    expected = {oid: record(n) for n, oid in enumerate(oids)}
    expected[oids[2]] = record("later")
    assert seen == expected
