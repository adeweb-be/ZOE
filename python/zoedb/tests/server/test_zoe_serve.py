"""Unit tests of `zoe.serve` and of the `<zoe-server>` section.

The acceptance test starts a server from a section. These tests cover the
rest: the keys against the options of `zoe serve`, the TOML file, the
twins, the search for the binary, the errors, and the process that runs.
"""

import ctypes
import io
import os
import re
import signal
import stat
import subprocess
import sys
import tempfile
import tomllib

import pytest
import ZConfig

from zoe import serve
from zoe.testing import ZoeServer

TIMEOUT = 30


@pytest.fixture(autouse=True)
def no_zoe_environment(monkeypatch):
    """Remove each ZOE_* variable, so that only the test sets them."""
    for name in list(os.environ):
        if name.startswith("ZOE_"):
            monkeypatch.delenv(name)


def write(tmp_path, *lines):
    """Write a file with one `<zoe-server>` section of lines. Return it."""
    body = "".join(f"  {line}\n" for line in lines)
    conf = tmp_path / "zoe-server.conf"
    conf.write_text(f"%import zoe\n<zoe-server>\n{body}</zoe-server>\n")
    return conf


def section(tmp_path, *lines):
    return serve.load(str(write(tmp_path, *lines)))


def test_the_keys_are_the_options_of_zoe_serve():
    help = subprocess.run(
        [ZoeServer.binary(), "serve", "--help"],
        capture_output=True,
        text=True,
        check=True,
    ).stdout
    options = set(re.findall(r"^\s+(?:-\w, )?--([a-z-]+)", help, re.MULTILINE))
    assert set(serve.KEYS) == options - {"config", "storage", "help"}
    schema = ZConfig.loadSchemaFile(io.StringIO(serve.SCHEMA))
    server = schema.gettype("zoe-server")
    keys = {key for key, info in server if not info.issection()}
    assert keys == {"binary", *serve.KEYS}
    storage = {key for key, _ in schema.gettype("storage")}
    assert storage == {"path", "blob-dir", "read-only", "pack-gc"}


def test_the_section_becomes_the_configuration_file(tmp_path):
    found = section(
        tmp_path,
        "listen 127.0.0.1:0",
        "lock-timeout 2m",
        "lock-warn 5s",
        "log-format json",
        "read-only on",
        "<storage main>",
        "  path /var/zoe/main",
        "  blob-dir /var/zoe/blobs",
        "</storage>",
        "<storage archive>",
        "  path /var/zoe/archive",
        "  read-only yes",
        "</storage>",
    )
    assert tomllib.loads(serve.toml(found)) == {
        "listen": "127.0.0.1:0",
        "lock-timeout": "2m",
        "lock-warn": "5s",
        "log-format": "json",
        "read-only": True,
        "storages": {
            "main": {
                "path": "/var/zoe/main",
                "blob-dir": "/var/zoe/blobs",
                "read-only": False,
            },
            "archive": {"path": "/var/zoe/archive", "read-only": True},
        },
    }


def test_pack_gc_false_goes_to_the_file_and_to_its_table(tmp_path):
    found = section(
        tmp_path,
        "pack-gc off",
        "<storage kept>",
        "  path /var/zoe/kept",
        "  pack-gc false",
        "</storage>",
        "<storage main>",
        "  path /var/zoe/main",
        "</storage>",
    )
    config = tomllib.loads(serve.toml(found))
    assert config["pack-gc"] is False
    assert config["storages"]["kept"]["pack-gc"] is False
    # The key is true by default, so the table of main does not give it.
    assert "pack-gc" not in config["storages"]["main"]


def test_the_structural_merges_go_to_the_file_and_to_their_table(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    found = section(
        tmp_path,
        "structural-merges shadow",
        "structural-merge-class BTrees.IOBTree.IOBucket on",
        "structural-merge-class BTrees.Length.Length off",
        "posting-dicts on",
        "resolution-log ~/merges.log",
    )
    assert tomllib.loads(serve.toml(found)) == {
        "structural-merges": "shadow",
        "posting-dicts": "on",
        "resolution-log": str(tmp_path / "merges.log"),
        "structural-merge-classes": {
            "BTrees.IOBTree.IOBucket": "on",
            "BTrees.Length.Length": "off",
        },
    }
    environ = {
        "ZOE_STRUCTURAL_MERGE_CLASSES": "A.B=on",
        "ZOE_POSTING_DICTS": "off",
        "ZOE_LISTEN": "127.0.0.1:1",
    }
    assert serve.environment(found, environ) == {"ZOE_LISTEN": "127.0.0.1:1"}


def test_an_empty_key_counts_as_not_set(tmp_path):
    found = section(tmp_path, "listen", "path /var/zoe")
    assert serve.settings(found) == {"path": "/var/zoe"}


def test_path_and_blob_dir_expand_the_home_directory(tmp_path, monkeypatch):
    monkeypatch.setenv("HOME", str(tmp_path))
    found = section(
        tmp_path,
        "path ~/one",
        "<storage two>",
        "  path ~/two",
        "  blob-dir ~/blobs",
        "</storage>",
    )
    config = tomllib.loads(serve.toml(found))
    assert config["path"] == str(tmp_path / "one")
    assert config["storages"]["two"]["path"] == str(tmp_path / "two")
    assert config["storages"]["two"]["blob-dir"] == str(tmp_path / "blobs")


@pytest.mark.parametrize("text", ['a"b', "a\\b", "a\tb", "a\x7fb", "é\U0001f600"])
def test_a_toml_string_keeps_its_text(text):
    assert tomllib.loads(f"key = {serve.toml_string(text)}")["key"] == text


def test_a_key_of_the_section_removes_its_twin(tmp_path):
    found = section(tmp_path, "listen 127.0.0.1:0")
    environ = {
        "ZOE_LISTEN": "192.0.2.1:1",
        "ZOE_LOCK_TIMEOUT": "2",
        "ZOE_BINARY": "zoe",
        "HOME": "/home",
    }
    assert serve.environment(found, environ) == {
        "ZOE_LOCK_TIMEOUT": "2",
        "ZOE_BINARY": "zoe",
        "HOME": "/home",
    }


def executable(path, content):
    path.write_bytes(content)
    path.chmod(0o755)
    return path


def test_the_binary_key_wins_over_zoe_binary_and_the_path(tmp_path):
    found = section(tmp_path, "binary /opt/zoe/bin/zoe")
    environ = {"ZOE_BINARY": "/usr/bin/zoe", "PATH": str(tmp_path)}
    assert serve.find_binary(found, environ) == "/opt/zoe/bin/zoe"
    found = section(tmp_path, "path /var/zoe")
    assert serve.find_binary(found, environ) == "/usr/bin/zoe"


def test_the_search_on_the_path_skips_a_script(tmp_path):
    scripts = tmp_path / "scripts"
    binaries = tmp_path / "binaries"
    scripts.mkdir()
    binaries.mkdir()
    executable(scripts / "zoe", b"#!/usr/bin/env python\n")
    binary = executable(binaries / "zoe", b"\x7fELF")
    found = section(tmp_path, "path /var/zoe")
    environ = {"PATH": os.pathsep.join([str(scripts), str(binaries)])}
    assert serve.find_binary(found, environ) == str(binary)
    assert serve.find_binary(found, {"PATH": str(scripts)}) is None


def test_a_missing_binary_fails_with_1(tmp_path, monkeypatch, capsys):
    monkeypatch.setenv("PATH", str(tmp_path))
    conf = write(tmp_path, "path /var/zoe")
    assert serve.main(["-C", str(conf)]) == serve.FAILURE
    assert "ZOE_BINARY" in capsys.readouterr().err


@pytest.mark.parametrize(
    "lines, error",
    [
        (["read-only maybe"], "read-only"),
        (["<storage main>", "</storage>"], "path"),
        (["<storage>", "  path /a", "</storage>"], "storage"),
        (["<storage main>", "  path /a", "  size 1", "</storage>"], "size"),
        (
            ["<storage main>", "  path /a", "</storage>"] * 2,
            "re-used",
        ),
    ],
)
def test_a_bad_section_is_a_usage_error(tmp_path, capsys, lines, error):
    conf = write(tmp_path, *lines)
    assert serve.main(["-C", str(conf)]) == serve.USAGE
    assert error in capsys.readouterr().err


def test_a_file_without_the_section_is_a_usage_error(tmp_path, capsys):
    conf = tmp_path / "empty.conf"
    conf.write_text("%import zoe\n")
    assert serve.main(["-C", str(conf)]) == serve.USAGE
    assert "zoe-server" in capsys.readouterr().err


@pytest.mark.parametrize("memory_file", [True, False])
def test_the_configuration_file_has_no_name_on_disk(tmp_path, monkeypatch, memory_file):
    if memory_file and not hasattr(os, "memfd_create"):
        pytest.skip("this platform has no memory file")
    if not memory_file:
        # The fallback of a platform without memfd_create runs on Linux too.
        monkeypatch.delattr(os, "memfd_create", raising=False)
    # The temporary file of a platform without memfd_create goes here.
    monkeypatch.setattr(tempfile, "tempdir", str(tmp_path))
    text = 'listen = "127.0.0.1:0"\n'
    descriptor, path = serve.config_file(text)
    try:
        assert os.get_inheritable(descriptor)
        if memory_file:
            assert path == f"/proc/self/fd/{descriptor}"
        else:
            assert path == f"/dev/fd/{descriptor}"
            assert stat.S_IMODE(os.fstat(descriptor).st_mode) == 0o600
            # On macOS an open of the path reads from this offset.
            assert os.lseek(descriptor, 0, os.SEEK_CUR) == 0
        # zoe serve opens the path and reads the whole file.
        with open(path) as file:
            assert file.read() == text
        assert list(tmp_path.iterdir()) == []
    finally:
        os.close(descriptor)


def program_path(pid):
    """Return the path of the program that the process `pid` runs, on macOS."""
    proc_pidpath = ctypes.CDLL(None, use_errno=True).proc_pidpath
    proc_pidpath.argtypes = (ctypes.c_int, ctypes.c_void_p, ctypes.c_uint32)
    proc_pidpath.restype = ctypes.c_int
    # The size is PROC_PIDPATHINFO_MAXSIZE of libproc.
    buffer = ctypes.create_string_buffer(4096)
    if proc_pidpath(pid, buffer, ctypes.sizeof(buffer)) <= 0:
        raise OSError(ctypes.get_errno(), f"proc_pidpath({pid}) failed")
    return os.fsdecode(buffer.value)


def test_the_process_becomes_zoe_serve_without_the_settings_in_its_arguments(
    tmp_path,
):
    binary = ZoeServer.binary()
    store = tmp_path / "store"
    conf = write(
        tmp_path,
        f"binary {binary}",
        "listen 127.0.0.1:0",
        f"path {store}",
    )
    process = subprocess.Popen(
        [sys.executable, "-m", "zoe.serve", "-C", str(conf)],
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        for line in process.stdout:
            if line.startswith("listening "):
                break
        else:
            raise AssertionError(f"zoe.serve stopped with {process.wait()}")
        if sys.platform == "linux":
            proc = f"/proc/{process.pid}"
            assert os.path.realpath(f"{proc}/exe") == os.path.realpath(binary)
            with open(f"{proc}/cmdline", "rb") as file:
                argv = file.read().split(b"\0")[:-1]
            assert argv[1:3] == [b"serve", b"--config"]
            assert re.fullmatch(rb"/proc/self/fd/\d+", argv[3])
            assert len(argv) == 4
        else:
            # macOS has no /proc. libproc gives the path of the program, and
            # ps gives the arguments, joined with spaces.
            program = program_path(process.pid)
            assert os.path.realpath(program) == os.path.realpath(binary)
            command = subprocess.run(
                ["ps", "-ww", "-o", "command=", "-p", str(process.pid)],
                capture_output=True,
                text=True,
                check=True,
            ).stdout.rstrip("\n")
            pattern = re.escape(binary) + r" serve --config /dev/fd/\d+"
            assert re.fullmatch(pattern, command)
    finally:
        process.send_signal(signal.SIGTERM)
        assert process.wait(TIMEOUT) == 0
        process.stdout.close()
    assert (store / "zoe.redb").exists()


def test_the_storage_name_is_the_section_name_in_lower_case(tmp_path):
    # ZConfig gives the name of a section in lower case, as for runzeo.
    found = section(tmp_path, "<storage Main>", "  path /a", "</storage>")
    assert list(tomllib.loads(serve.toml(found))["storages"]) == ["main"]
