"""Acceptance test of the `<zoe-server>` section and of `zoe.serve`.

For buildout users, the server takes its configuration from a ZConfig
file too. `python -m zoe.serve -C FILE` reads the `<zoe-server>` section of
the file, with the schema that `%import zoe` gives, and runs `zoe serve`
with the same settings: it starts the binary that the `binary` key names,
or else `ZOE_BINARY`, or else `zoe` on the PATH, and passes the settings
as a configuration file of `zoe serve`. It replaces itself with that
process, so a supervisor that runs it stops the server with SIGTERM.

The keys of the section are the options of `zoe serve` without `--`, as
in the TOML file. Each `<storage NAME>` subsection gives a named storage
with `path`, `blob-dir` and `read-only`.

The wheel ships no server binary: the extension module never links
tokio. The binary comes from the release or the container image.
"""

import os
import signal
import subprocess
import sys

import pytest

from zoe.testing import ZoeServer

TIMEOUT = 30


@pytest.fixture
def binary():
    """The `zoe` binary that the tests of the server run."""
    return ZoeServer.binary()


def start(conf, env):
    process = subprocess.Popen(
        [sys.executable, "-m", "zoe.serve", "-C", str(conf)],
        stdout=subprocess.PIPE,
        text=True,
        env=env,
    )
    for line in process.stdout:
        if line.startswith("listening "):
            host, port = line.split()[1].rsplit(":", 1)
            return process, (host, int(port))
    raise AssertionError(f"zoe.serve stopped with {process.wait()}")


def test_the_section_starts_zoe_serve_with_its_storages(tmp_path, binary):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.Connection import TransactionMetaData

    conf = tmp_path / "zoe-server.conf"
    conf.write_text(
        f"""
        %import zoe
        <zoe-server>
          binary {binary}
          listen 127.0.0.1:0
          <storage main>
            path {tmp_path / "main"}
          </storage>
          <storage archive>
            path {tmp_path / "archive"}
            read-only false
          </storage>
        </zoe-server>
        """
    )
    process, address = start(conf, dict(os.environ))
    try:
        for name in ("main", "archive"):
            storage = ClientStorage(address, storage=name, wait_timeout=TIMEOUT)
            try:
                txn = TransactionMetaData()
                storage.tpc_begin(txn)
                oid = storage.new_oid()
                storage.store(oid, bytes(8), b"data", "", txn)
                storage.tpc_vote(txn)
                tid = storage.tpc_finish(txn)
                assert storage.load(oid) == (b"data", tid)
            finally:
                storage.close()
    finally:
        process.send_signal(signal.SIGTERM)
        assert process.wait(TIMEOUT) == 0
    assert (tmp_path / "main" / "zoe.redb").exists()
    assert (tmp_path / "archive" / "zoe.redb").exists()


def test_the_binary_comes_from_the_environment(tmp_path, binary):
    conf = tmp_path / "zoe-server.conf"
    conf.write_text(
        f"""
        %import zoe
        <zoe-server>
          listen 127.0.0.1:0
          <storage 1>
            path {tmp_path / "store"}
          </storage>
        </zoe-server>
        """
    )
    process, _ = start(conf, dict(os.environ, ZOE_BINARY=str(binary)))
    process.send_signal(signal.SIGTERM)
    assert process.wait(TIMEOUT) == 0


def test_an_unknown_key_is_an_error_before_anything_starts(tmp_path):
    conf = tmp_path / "zoe-server.conf"
    conf.write_text(
        """
        %import zoe
        <zoe-server>
          no-such-key 1
        </zoe-server>
        """
    )
    result = subprocess.run(
        [sys.executable, "-m", "zoe.serve", "-C", str(conf)],
        capture_output=True,
        text=True,
        check=False,
    )
    assert result.returncode == 2
    assert "no-such-key" in result.stderr
