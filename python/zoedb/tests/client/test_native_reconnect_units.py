"""Unit tests of the reconnects of the native client in `zoe.zeo`.

`just test` runs them. They give the events of the connection to the
storage of the native client directly, with a native client of the test.
"""

import logging
import socket
import threading
import time

import pytest
import ZODB.event
import zope.interface
from ZEO.Exceptions import ClientDisconnected
from ZEO.interfaces import StaleCache
from ZODB.interfaces import IBlobStorage

import zoe.zeo


class Native:
    """A native client that answers as a connected one."""

    def __init__(self, read_only=False):
        self.read_only_flag = read_only

    def read_only(self):
        return self.read_only_flag

    def info(self):
        return {"interfaces": ()}

    def address(self):
        return ("127.0.0.1", 8100)

    def take_cache_check(self):
        return None

    def cache_entries(self):
        return []

    connected = True

    def is_connected(self):
        return self.connected


class DB:
    def __init__(self):
        self.cache_invalidations = 0

    def invalidateCache(self):
        self.cache_invalidations += 1


def storage(native):
    """Return a native storage around `native`, without an open."""
    result = object.__new__(zoe.zeo._NativeClientStorage)
    result.__name__ = "test"
    result._read_only = False
    result._info = {"interfaces": ()}
    result._server_addr = None
    result._db = DB()
    result._native = native
    result._cache = zoe.zeo._NativeCache(native, 0, None)
    result._server = zoe.zeo._NativeServer(result, 30)
    result._operational = threading.Event()
    return result


def test_the_wait_of_the_open_is_built():
    assert "wait" not in zoe.zeo._NOT_BUILT


def test_read_only_follows_the_server_of_the_native_client():
    native = Native(read_only=True)
    s = storage(native)
    assert s.isReadOnly()
    native.read_only_flag = False
    assert not s.isReadOnly()


def test_a_connection_with_a_verified_cache_keeps_the_db(caplog):
    s = storage(Native())
    with caplog.at_level(logging.INFO, logger="ZEO.ClientStorage"):
        s._connected("quick")
        s._connected("kept")
    assert s._db.cache_invalidations == 0
    assert "8100" in s._server_addr
    # The client logs its connections as ZEO does, under the logger of ZEO.
    messages = [r.getMessage() for r in caplog.records]
    assert any("Connected to storage" in m for m in messages)
    assert any("Reconnected to storage" in m for m in messages)
    assert s._server.client.verify_result == "empty cache"
    assert s._connection_generation == 2


def test_a_dropped_cache_notifies_stale_cache_and_invalidates_the_db(
    caplog, monkeypatch
):
    s = storage(Native())
    events = []
    monkeypatch.setattr(ZODB.event, "notify", events.append)
    s._connected("dropped")
    assert s._db.cache_invalidations == 1
    assert [type(event) for event in events] == [StaleCache]
    assert events[0].storage is s
    assert [r.levelno for r in caplog.records if "stale cache" in r.getMessage()] == [
        logging.CRITICAL
    ]


def test_a_refused_server_logs_a_critical_message(caplog):
    s = storage(Native())
    s._refused("the server is behind")
    (record,) = [r for r in caplog.records if r.levelno == logging.CRITICAL]
    assert "the server is behind" in record.getMessage()
    assert s._db.cache_invalidations == 0


def test_a_call_without_a_server_waits_for_one_as_zeo_does():
    native = Native()
    native.connected = False
    s = storage(native)
    s._closed = False
    start = time.monotonic()
    with pytest.raises(ClientDisconnected):
        s._wait(0.2)
    assert time.monotonic() - start >= 0.2
    # A connection during the wait ends it.
    threading.Timer(0.1, lambda: setattr(native, "connected", True)).start()
    start = time.monotonic()
    s._wait(5)
    assert time.monotonic() - start < 1
    # After the close, the wait raises at once.
    native.connected = False
    s._closed = True
    start = time.monotonic()
    with pytest.raises(ClientDisconnected):
        s._wait(5)
    assert time.monotonic() - start < 1


def test_the_cache_gives_its_entries_as_the_cache_of_zeo():
    native = Native()
    native.cache_entries = lambda: [
        (b"\0" * 8, b"\1" * 8, None),
        (b"\0" * 8, b"\0" * 7 + b"\1", b"\1" * 8),
    ]
    cache = zoe.zeo._NativeCache(native, 1000, "c-1.zoec")
    assert (cache.maxsize, cache.path) == (1000, "c-1.zoec")
    assert len(cache) == 2
    assert list(cache.contents()) == [(b"\0" * 8, b"\1" * 8)]


def test_the_stale_cache_event_comes_before_the_drop_and_once(caplog, monkeypatch):
    native = Native()
    s = storage(native)
    s._native_ready = threading.Event()
    s._native_ready.set()
    seen = []

    def notify(event):
        # The handler sees the entries: the drop comes after the call.
        seen.append((type(event), len(event.storage._cache)))

    monkeypatch.setattr(ZODB.event, "notify", notify)
    native.cache_entries = lambda: [(b"\0" * 8, b"\1" * 8, None)]
    s._cache_dropping("")
    native.cache_entries = lambda: []
    s._connected("dropped")
    assert seen == [(StaleCache, 1)]
    critical = [r.getMessage() for r in caplog.records if r.levelno >= logging.CRITICAL]
    assert critical == ["test dropping stale cache"]
    assert s._db.cache_invalidations == 1
    assert s._server.client.verify_result == "cache too old, clearing"


class Lookups:
    """A `socket.gethostbyaddr` that counts its calls."""

    def __init__(self):
        self.calls = []

    def __call__(self, host):
        self.calls.append(host)
        return ("db.example", [], [host])


def test_the_canonical_name_of_the_server_comes_at_its_first_read(monkeypatch):
    lookups = Lookups()
    monkeypatch.setattr(socket, "gethostbyaddr", lookups)
    s = storage(Native())
    s._storage = "1"
    s._take_server()
    assert lookups.calls == []
    # `sortKey` reads the canonical name, as for ZEO.
    assert s.sortKey() == "1:('db.example', 8100)"
    assert s._server_addr == "('db.example', 8100)"
    assert lookups.calls == ["127.0.0.1"]
    # A new connection asks again at the next read.
    s._take_server()
    assert s._server_addr == "('db.example', 8100)"
    assert lookups.calls == ["127.0.0.1", "127.0.0.1"]


def test_a_unix_socket_needs_no_lookup(monkeypatch):
    lookups = Lookups()
    monkeypatch.setattr(socket, "gethostbyaddr", lookups)
    native = Native()
    native.address = lambda: "/run/zoe.sock"
    s = storage(native)
    s._take_server()
    assert s._server_addr == "/run/zoe.sock"
    assert lookups.calls == []


def test_the_interfaces_of_the_server_are_declared_once(monkeypatch):
    calls = []
    also = zope.interface.alsoProvides
    monkeypatch.setattr(
        zope.interface,
        "alsoProvides",
        lambda *args: (calls.append(args[1:]), also(*args)),
    )
    native = Native()
    native.info = lambda: {"interfaces": (("ZODB.interfaces", "IBlobStorage"),)}
    s = storage(native)
    s._take_server()
    s._take_server()
    assert IBlobStorage.providedBy(s)
    assert calls == [(IBlobStorage,)]
