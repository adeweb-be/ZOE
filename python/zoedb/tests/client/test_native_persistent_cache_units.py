"""Unit tests of the cache file of the native client in `zoe.zeo`.

`just test` runs them. They check the name of the file, the arguments
that give it, a file that is not a cache file of ZOE, and the lock of the
file, against `zoe serve`, and the logs of the verification at the open.
"""

import logging
import os

import pytest
import zc.lockfile
import ZODB.event
from ZEO.interfaces import StaleCache
from ZODB.POSException import Unsupported

import zoe.testing
import zoe.zeo
from zoe.testing import ZoeServer


def test_the_name_follows_zeo_with_its_own_extension(tmp_path):
    path = zoe.zeo._native_cache_path(None, str(tmp_path), "c", "1")
    assert path == os.path.join(str(tmp_path), "c-1.zoec")


def test_without_var_the_file_is_in_the_current_directory(tmp_path, monkeypatch):
    monkeypatch.chdir(tmp_path)
    path = zoe.zeo._native_cache_path(None, None, "c", "main")
    assert path == os.path.join(str(tmp_path), "c-main.zoec")


def test_a_str_cache_names_the_file_of_zeo_with_the_native_extension():
    assert zoe.zeo._native_cache_path("x/c-1.zec", "var", "c", "1") == "x/c-1.zoec"
    assert zoe.zeo._native_cache_path("x/y.cache", "var", "c", "1") == "x/y.cache.zoec"
    assert zoe.zeo._native_cache_path("x/y.zoec", "var", "c", "1") == "x/y.zoec"


def test_no_client_and_no_cache_keeps_the_cache_in_memory():
    assert zoe.zeo._native_cache_path(None, "var", None, "1") is None
    assert zoe.zeo._native_cache_path(None, "var", "", "1") is None


def test_a_cache_object_is_refused():
    with pytest.raises(Unsupported, match="cache object"):
        zoe.zeo._native_cache_path(object(), None, "c", "1")


def test_client_var_and_cache_are_built():
    for key in ("client", "var", "cache"):
        assert key not in zoe.zeo._NOT_BUILT


def test_the_file_of_zeo_stays_as_it_is(tmp_path):
    zec = tmp_path / "c-1.zec"
    zec.write_bytes(b"ZEC3" + bytes(8))
    with ZoeServer(tmp_path / "store") as server:
        storage = zoe.testing.native_client(
            server.address, wait_timeout=30, client="c", var=str(tmp_path)
        )
        storage.close()
    assert zec.read_bytes() == b"ZEC3" + bytes(8)
    assert (tmp_path / "c-1.zoec").exists()


def test_a_zec_path_as_cache_leaves_the_file_of_zeo_byte_for_byte(tmp_path):
    zec = tmp_path / "x.zec"
    content = b"ZEC3" + os.urandom(4096)
    zec.write_bytes(content)
    with ZoeServer(tmp_path / "store") as server:
        storage = zoe.testing.native_client(
            server.address, wait_timeout=30, cache=str(zec)
        )
        storage.close()
    assert zec.read_bytes() == content
    assert (tmp_path / "x.zoec").exists()


def test_a_file_that_is_not_a_cache_file_of_zoe_is_left_as_it_is(tmp_path):
    path = tmp_path / "c-1.zoec"
    content = b"something else" + os.urandom(4096)
    path.write_bytes(content)
    with ZoeServer(tmp_path / "store") as server:
        with pytest.raises(ValueError, match="not a cache file"):
            zoe.testing.native_client(
                server.address, wait_timeout=30, client="c", var=str(tmp_path)
            )
    assert path.read_bytes() == content


def test_a_second_open_in_the_same_process_fails_until_the_close(tmp_path):
    with ZoeServer(tmp_path / "store") as server:
        first = zoe.testing.native_client(
            server.address, wait_timeout=30, client="c", var=str(tmp_path)
        )
        try:
            with pytest.raises(zc.lockfile.LockError):
                zoe.testing.native_client(
                    server.address, wait_timeout=30, client="c", var=str(tmp_path)
                )
        finally:
            first.close()
        second = zoe.testing.native_client(
            server.address, wait_timeout=30, client="c", var=str(tmp_path)
        )
        second.close()


def test_a_missing_var_raises_an_os_error(tmp_path):
    with ZoeServer(tmp_path / "store") as server:
        with pytest.raises(FileNotFoundError):
            zoe.testing.native_client(
                server.address,
                wait_timeout=30,
                client="c",
                var=str(tmp_path / "missing"),
            )


class Native:
    """A native client that gives one result of the verification of its
    cache file."""

    def __init__(self, check):
        self.check = check

    def take_cache_check(self):
        check, self.check = self.check, None
        return check


class DB:
    def __init__(self):
        self.cache_invalidations = 0

    def invalidateCache(self):
        self.cache_invalidations += 1


def storage(check):
    """Return a native storage whose verification at the open gave `check`."""
    result = object.__new__(zoe.zeo._NativeClientStorage)
    result.__name__ = "test"
    result._db = DB()
    result._native = Native(check)
    return result


def critical(caplog):
    return [r.getMessage() for r in caplog.records if r.levelno == logging.CRITICAL]


def test_a_kept_or_quick_file_logs_nothing(caplog):
    for kind in ("kept", "quick"):
        s = storage((kind, ""))
        s._check_cache_file()
        assert critical(caplog) == []
        assert s._db.cache_invalidations == 0


def test_a_file_that_the_server_does_not_cover_is_a_stale_cache(caplog, monkeypatch):
    events = []
    monkeypatch.setattr(ZODB.event, "notify", events.append)
    s = storage(("dropped", ""))
    s._check_cache_file()
    s._check_cache_file()
    assert [type(event) for event in events] == [StaleCache]
    assert critical(caplog) == ["test dropping stale cache"]
    assert s._db.cache_invalidations == 1


def test_a_failed_verification_drops_the_file_and_says_why(caplog, monkeypatch):
    monkeypatch.setattr(ZODB.event, "notify", lambda event: None)
    s = storage(("failed", "the connection is lost"))
    s._check_cache_file()
    (message,) = critical(caplog)
    assert "dropping stale cache" in message
    assert "the connection is lost" in message


def test_a_file_ahead_of_the_server_logs_a_critical_message(caplog):
    s = storage(("ahead", "the server is behind"))
    s._check_cache_file()
    (message,) = critical(caplog)
    assert "out of sync" in message
    assert "the server is behind" in message
    assert s._db.cache_invalidations == 0
