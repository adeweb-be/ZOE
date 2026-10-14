"""Acceptance test: `zoe serve --listen PATH` on a unix domain socket.

A `--listen` value that holds a `/` is the path of a unix domain socket, as
an `address` with a `/` is in the `<zeo>` section of ZEO. `zoe serve` then
listens on that socket, and a stock `ZEO.ClientStorage` connects with the
path as its address. `ZOE_LISTEN` takes the same value. A relative path is
relative to the current directory of the server, and the server binds it
as it is, so a short relative path works in a deep directory.

At its start, the server fails with 1, and names the path, when another
process accepts connections on the path. Otherwise it removes a stale
socket file at the path, as runzeo does. At its stop, it removes the socket
file.
"""

import os
import signal
import socket
import subprocess
import time

import pytest

from zoe.testing import ZoeServer

TIMEOUT = 30


class Serve:
    """A `zoe serve` process with its stdout and stderr in files."""

    def __init__(self, tmp_path, args, env=None, cwd=None):
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        environ.update(env or {})
        self.stderr_path = tmp_path / "serve.stderr"
        with open(tmp_path / "serve.stdout", "w") as out:
            with open(self.stderr_path, "w") as err:
                self.process = subprocess.Popen(
                    [ZoeServer.binary(), "serve", *map(str, args)],
                    stdin=subprocess.DEVNULL,
                    stdout=out,
                    stderr=err,
                    env=environ,
                    cwd=cwd,
                )

    def stderr(self):
        return self.stderr_path.read_text()

    def stop(self):
        """Send SIGTERM and return the exit code."""
        self.process.send_signal(signal.SIGTERM)
        return self.process.wait(TIMEOUT)

    def kill(self):
        if self.process.poll() is None:
            self.process.kill()
            self.process.wait()


def accepts(path):
    """Return whether a process accepts connections on the socket at path."""
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as probe:
        try:
            probe.connect(str(path))
        except OSError:
            return False
        return True


def wait_until_it_accepts(server, path):
    deadline = time.monotonic() + TIMEOUT
    while not accepts(path):
        assert server.process.poll() is None, server.stderr()
        assert time.monotonic() < deadline, "zoe serve did not listen"
        time.sleep(0.05)


def client(address, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(address, storage="1", **kw)


def commit_and_load(address):
    from ZODB.Connection import TransactionMetaData

    storage = client(address)
    try:
        oid = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, bytes(8), b"unix", "", txn)
        storage.tpc_vote(txn)
        tid = storage.tpc_finish(txn)
        assert storage.load(oid) == (b"unix", tid)
    finally:
        storage.close()


@pytest.fixture
def run(tmp_path):
    servers = []

    def run(args, **kw):
        server = Serve(tmp_path, args, **kw)
        servers.append(server)
        return server

    yield run
    for server in servers:
        server.kill()


def test_a_stock_client_commits_over_a_unix_domain_socket(tmp_path, run):
    path = tmp_path / "zeo.sock"
    server = run(["--path", tmp_path / "store", "--listen", path])
    wait_until_it_accepts(server, path)
    commit_and_load(str(path))
    assert server.stop() == 0
    assert not path.exists()


def test_zoe_listen_takes_a_path(tmp_path, run):
    path = tmp_path / "zeo.sock"
    server = run(["--path", tmp_path / "store"], env={"ZOE_LISTEN": str(path)})
    wait_until_it_accepts(server, path)
    commit_and_load(str(path))
    assert server.stop() == 0


def test_a_relative_path_is_relative_to_the_current_directory(
    tmp_path, run, monkeypatch
):
    # A directory deeper than the 108 bytes of a socket address.
    deep = tmp_path.joinpath(*["directory"] * 12)
    deep.mkdir(parents=True)
    assert len(str(deep / "sock")) > 108
    server = run(["--path", tmp_path / "store", "--listen", "./sock"], cwd=deep)
    deadline = time.monotonic() + TIMEOUT
    while not (deep / "sock").exists():
        assert server.process.poll() is None, server.stderr()
        assert time.monotonic() < deadline, "zoe serve did not listen"
        time.sleep(0.05)
    monkeypatch.chdir(deep)
    commit_and_load("./sock")
    assert server.stop() == 0
    assert not (deep / "sock").exists()


def test_a_stale_socket_file_is_replaced(tmp_path, run):
    path = tmp_path / "zeo.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as stale:
        stale.bind(str(path))
    assert path.exists() and not accepts(path)
    server = run(["--path", tmp_path / "store", "--listen", path])
    wait_until_it_accepts(server, path)
    commit_and_load(str(path))
    assert server.stop() == 0


def test_a_socket_that_accepts_connections_fails_the_start(tmp_path, run):
    path = tmp_path / "zeo.sock"
    with socket.socket(socket.AF_UNIX, socket.SOCK_STREAM) as other:
        other.bind(str(path))
        other.listen()
        server = run(["--path", tmp_path / "store", "--listen", path])
        assert server.process.wait(TIMEOUT) == 1
        assert str(path) in server.stderr()
        # The socket of the other process stays.
        assert accepts(path)
