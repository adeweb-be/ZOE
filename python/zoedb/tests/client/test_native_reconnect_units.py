"""Unit tests of the reconnects of the native client in `zoe.zeo`.

`just test` runs them. They give the events of the connection to the
storage of the native client directly, with a native client of the test.
"""

import logging
import socket

import ZODB.event
import zope.interface
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
    with caplog.at_level(logging.INFO, logger="zoe.zeo"):
        s._connected("quick")
    assert s._db.cache_invalidations == 0
    assert "8100" in s._server_addr
    assert any("Reconnected" in r.getMessage() for r in caplog.records)


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
