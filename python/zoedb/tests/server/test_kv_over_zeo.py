"""Unit tests of the extension `kv` over the protocol (ADR 0043): the calls
of `zoe.zeo.ClientStorage` with the native client and with the asyncio
client of ZEO, `zoe.kv` on a database of such a client, the expiry of an
ephemeral key, a restart, and a stock client that sees nothing new."""

import time

import pytest
import transaction
import ZODB
from ZEO.Exceptions import ServerException
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import Unsupported
from ZODB.utils import z64

import zoe.kv
from zoe.testing import ZoeServer

TIMEOUT = 30


def zoe_client(address, native=True, **kw):
    import zoe.zeo

    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.zeo.ClientStorage(address, storage="1", native=native, **kw)


def stock_client(address, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(address, storage="1", **kw)


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


def commit(storage, *writes):
    meta = TransactionMetaData()
    storage.tpc_begin(meta)
    for write in writes:
        if write[0] == "delete":
            storage.kv_delete(*write[1:], meta)
        else:
            storage.kv_set(*write, meta)
    storage.tpc_vote(meta)
    return storage.tpc_finish(meta)


@pytest.mark.parametrize("native", [True, False])
def test_the_calls_of_the_namespaces(server, native):
    storage = zoe_client(server.address, native=native)
    try:
        assert "kv" in storage._info["extensions"]
        assert storage.kv_supported()
        assert storage.kv_get("durable", "registry", b"title") is None
        assert storage.kv_generation("registry") == z64
        tid = commit(
            storage,
            ("durable", "registry", b"title", b"site", None),
            ("ephemeral", "sessions", b"s1", b"data", 60),
        )
        assert storage.kv_get("durable", "registry", b"title") == b"site"
        assert storage.kv_get("ephemeral", "sessions", b"s1") == b"data"
        assert storage.kv_generation("registry") == tid
        assert storage.kv_scan("ephemeral", "sessions") == [(b"s1", b"data")]
        commit(storage, ("delete", "durable", "registry", b"title"))
        assert storage.kv_get("durable", "registry", b"title") is None
    finally:
        storage.close()


@pytest.mark.parametrize("native", [True, False])
def test_an_ephemeral_key_expires(server, native):
    storage = zoe_client(server.address, native=native)
    try:
        commit(storage, ("ephemeral", "sessions", b"s1", b"data", 0.2))
        assert storage.kv_get("ephemeral", "sessions", b"s1") == b"data"
        time.sleep(0.3)
        assert storage.kv_get("ephemeral", "sessions", b"s1") is None
    finally:
        storage.close()


@pytest.mark.parametrize("native", [True, False])
def test_a_bad_write_fails_the_vote(server, native):
    storage = zoe_client(server.address, native=native)
    try:
        meta = TransactionMetaData()
        storage.tpc_begin(meta)
        storage.kv_set("ephemeral", "sessions", b"s1", b"data", None, meta)
        # The client of ZEO raises the class of the server that it does not
        # know as `ServerException`, as for a stock server.
        with pytest.raises((ValueError, ServerException)) as raised:
            storage.tpc_vote(meta)
        assert "an ephemeral key needs a TTL" in str(raised.value)
        storage.tpc_abort(meta)
        assert storage.kv_get("ephemeral", "sessions", b"s1") is None
    finally:
        storage.close()


@pytest.mark.parametrize("native", [True, False])
def test_a_namespace_on_a_database_of_the_client(server, native):
    db = ZODB.DB(zoe_client(server.address, native=native))
    try:
        manager = transaction.TransactionManager()
        conn = db.open(manager)
        registry = zoe.kv.Namespace(conn, "durable", "registry")
        conn.root()["x"] = 1
        registry.set("title", b"site")
        manager.commit()
        assert registry.generation() == conn.root()._p_serial
        sessions = zoe.kv.Namespace(conn, "ephemeral", "sessions")
        sessions.set("s1", b"data", ttl=60)
        manager.commit()
        assert sessions.get("s1") == b"data"
        assert registry.get("title") == b"site"
        conn.close()
    finally:
        db.close()


@pytest.mark.parametrize("native", [True, False])
def test_kv_supported_keeps_its_answer_while_the_server_is_away(server, native):
    storage = zoe_client(server.address, native=native)
    try:
        assert storage.kv_supported()
        server.kill()
        deadline = time.monotonic() + 30
        while storage.is_connected() and time.monotonic() < deadline:
            time.sleep(0.05)
        assert not storage.is_connected()
        # The locking backend of zoe.plone never takes the stock path then.
        assert storage.kv_supported()
    finally:
        storage.close()
        server.start()


def test_a_durable_key_survives_a_restart(server):
    storage = zoe_client(server.address)
    try:
        commit(storage, ("durable", "registry", b"title", b"site", None))
    finally:
        storage.close()
    server.kill()
    server.start()
    storage = zoe_client(server.address)
    try:
        assert storage.kv_get("durable", "registry", b"title") == b"site"
    finally:
        storage.close()


def test_a_stock_client_sees_nothing_new(server):
    writer = zoe_client(server.address)
    stock = stock_client(server.address)
    try:
        assert "extensions" not in stock._info
        last = stock.lastTransaction()
        tid = commit(writer, ("durable", "registry", b"title", b"site", None))
        # The namespace-only commit is a transaction without records.
        deadline = time.monotonic() + TIMEOUT
        while stock.lastTransaction() < tid and time.monotonic() < deadline:
            time.sleep(0.01)
        assert stock.lastTransaction() == tid > last
        with pytest.raises(Exception):  # noqa: B017
            stock._call("kv_get", "durable", "registry", b"title")
        assert stock.lastTransaction() == tid
    finally:
        stock.close()
        writer.close()


@pytest.mark.parametrize("native", [True, False])
def test_against_a_server_without_kv_the_calls_are_unsupported(tmp_path, native):
    import ZEO

    # The native client speaks msgpack only.
    zeo_conf = {"msgpack": "true"}
    address, stop = ZEO.server(path=str(tmp_path / "Data.fs"), zeo_conf=zeo_conf)
    storage = zoe_client(address, native=native)
    try:
        assert "extensions" not in storage._info
        assert not storage.kv_supported()
        with pytest.raises(Unsupported):
            storage.kv_get("durable", "registry", b"title")
        meta = TransactionMetaData()
        storage.tpc_begin(meta)
        with pytest.raises(Unsupported):
            storage.kv_set("durable", "registry", b"title", b"site", None, meta)
        storage.tpc_abort(meta)
    finally:
        storage.close()
        stop()
