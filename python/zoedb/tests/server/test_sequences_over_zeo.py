"""Acceptance test of sequences over the protocol (F29).

The extension `sequences` adds one method to the protocol:
`sequence_next(name)` gives the next value of the sequence `name` of the
store, with the rules of the in-process primitive: values unique and
strictly increasing per store and per name, from 1, outside the two-phase
commit, never given twice, also after a restart of the server.

`zoe.zeo.ClientStorage` is a subclass of `ZEO.ClientStorage.ClientStorage`.
It declares the extensions that it uses in `register`, and has
`sequence(name).next()`, as `zoe.Storage` has. Against a server that does
not accept `sequences`, a stock ZEO server among them, it works as
`ZEO.ClientStorage` does and `sequence(name).next()` raises
`ZODB.POSException.Unsupported`. `%import zoe` gives the ZConfig section
`<zoeclient>`, with every key of `<zeoclient>`, which opens it.

A stock client declares nothing: it never sees the extension, and a call
of `sequence_next` without the declaration is refused.
"""

import threading

import pytest
from support import TIMEOUT
from support import client as stock_client

# The largest number of values that a restart can skip.
MAX_GAP = 100


def zoe_client(address, **kw):
    import zoe.zeo

    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.zeo.ClientStorage(address, storage="1", **kw)


def test_the_client_is_a_zeo_client_storage():
    from ZEO.ClientStorage import ClientStorage

    import zoe.zeo

    assert issubclass(zoe.zeo.ClientStorage, ClientStorage)


def test_values_start_at_one_and_increase(server):
    storage = zoe_client(server.address)
    try:
        assert "sequences" in storage._info["extensions"]
        intids = storage.sequence("intids")
        assert [intids.next() for _ in range(250)] == list(range(1, 251))
        assert storage.sequence("lexicon").next() == 1
    finally:
        storage.close()


def test_clients_never_get_the_same_value(server):
    clients = [zoe_client(server.address) for _ in range(4)]
    values = [[] for _ in clients]

    def take(storage, out):
        sequence = storage.sequence("intids")
        for _ in range(200):
            out.append(sequence.next())

    threads = [
        threading.Thread(target=take, args=(storage, out))
        for storage, out in zip(clients, values, strict=True)
    ]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
    finally:
        for storage in clients:
            storage.close()
    every = [value for out in values for value in out]
    assert len(set(every)) == len(every) == 800
    for out in values:
        assert out == sorted(out)
    assert sorted(every) == list(range(1, 801))


def test_values_keep_increasing_after_a_restart(server):
    storage = zoe_client(server.address)
    try:
        last = [storage.sequence("intids").next() for _ in range(5)][-1]
    finally:
        storage.close()
    server.kill()
    server.start()
    storage = zoe_client(server.address)
    try:
        value = storage.sequence("intids").next()
    finally:
        storage.close()
    assert last < value <= last + MAX_GAP + 1


def test_a_stock_client_never_sees_the_extension(server):
    storage = stock_client(server.address)
    try:
        assert "extensions" not in storage._info
        with pytest.raises(Exception):  # noqa: B017
            storage._call("sequence_next", "intids")
    finally:
        storage.close()


def test_against_a_stock_server_sequences_are_unsupported(tmp_path):
    import ZEO
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import Unsupported

    address, stop = ZEO.server(path=str(tmp_path / "Data.fs"))
    try:
        storage = zoe_client(address)
        try:
            oid = storage.new_oid()
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.store(oid, bytes(8), b"data", "", txn)
            storage.tpc_vote(txn)
            tid = storage.tpc_finish(txn)
            assert storage.load(oid) == (b"data", tid)
            with pytest.raises(Unsupported):
                storage.sequence("intids").next()
        finally:
            storage.close()
    finally:
        stop()


def test_the_zoeclient_section_opens_the_client(server):
    from ZODB.config import storageFromString

    import zoe.zeo

    host, port = server.address
    storage = storageFromString(
        f"""
        %import zoe
        <zoeclient>
          server {host}:{port}
          storage 1
          wait-timeout {TIMEOUT}
        </zoeclient>
        """
    )
    try:
        assert isinstance(storage, zoe.zeo.ClientStorage)
        assert storage.sequence("intids").next() == 1
    finally:
        storage.close()
