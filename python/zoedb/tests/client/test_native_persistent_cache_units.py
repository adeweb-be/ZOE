"""Unit tests of the cache file of the native client in `zoe.zeo`.

`just test` runs them. They check the name of the file, the arguments
that give it, and the lock of the file, against `zoe serve`.
"""

import os

import pytest
import zc.lockfile
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


def test_a_str_cache_is_the_path_and_no_client_keeps_the_cache_in_memory():
    assert zoe.zeo._native_cache_path("x/y.cache", "var", "c", "1") == "x/y.cache"
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
