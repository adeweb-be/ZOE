"""Acceptance test of reconnects and cache verification in the native client.

`just test` runs it. The tests open the native client through
`zoe.testing.native_client`, against a `zoe serve` that stops and starts
again on the same port and the same store.

- While the server is down, a load that the cache does not hold raises
  `ClientDisconnected`. The client connects again in the background,
  without a new open.
- After each connection, the client verifies its cache as `verify` of
  `ZEO/asyncio/client.py` does. A cache tid lower than the tid of the
  server asks `getInvalidations`. The client then ends the cache entry of
  each oid that it gives, and gives the DB the invalidation before
  `lastTransaction()` moves. The other entries of the cache stay. When the
  server answers `None`, the client drops its cache and calls
  `invalidateCache` of the DB. A cache tid higher than the
  tid of the server makes the client refuse the server and log a critical
  message.
- A transaction that is open when the connection drops fails at its next
  step with `ClientDisconnected`. It never commits on the next connection,
  so it leaves no lock on the server.
- With `read_only_fallback`, the client opens on a read-only server and
  takes the writable server when it starts on the other address.
- With `wait=False`, the open does not wait for a server, and the first
  load raises `ClientDisconnected`.
- The client takes `min_disconnect_poll`, `max_disconnect_poll` and
  `drop_cache_rather_verify` and ignores them, as ZEO 6 does: they change
  neither the delay of a reconnect nor the verification.

The test counts the loads that reach the server with `zoe_loads_total` of
`GET /metrics` on the admin port of `zoe serve`. A commit of another client
while the server is down goes through `zoe.Storage` on the store.
"""

import logging
import os
import socket
import subprocess
import threading
import time
import urllib.request

import pytest
from ZEO.Exceptions import ClientDisconnected
from ZODB.POSException import POSKeyError, ReadOnlyError
from ZODB.utils import p64

import zoe
import zoe.testing
from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30
# The wait of a call while the client has no server.
DOWN = 2
# An oid that no test stores, so no cache holds it.
ABSENT = p64(1 << 40)


class Server:
    """A `zoe serve` process with an admin port.

    `zoe.testing.ZoeServer` keeps only the line `listening ADDRESS` of the
    server. This class also keeps the line `admin ADDRESS` before it, so
    the test reads `/metrics`. A start after a stop listens on the same
    address and serves the store of `path`. Each start has a new admin
    port.
    """

    def __init__(self, path, args=(), address=None):
        self.path = path
        self.args = list(args)
        self.address = address
        self.admin = None
        self._process = None

    def start(self):
        listen = "127.0.0.1:0"
        if self.address is not None:
            listen = "{}:{}".format(*self.address)
        command = [
            ZoeServer.binary(), "serve", "--path", str(self.path),
            "--listen", listen, "--admin-listen", "127.0.0.1:0", *self.args,
        ]  # fmt: skip
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
        )
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, process.kill)
        timer.start()
        try:
            for line in process.stdout:
                word, _, value = line.strip().partition(" ")
                if word == "admin":
                    self.admin = value
                elif word == "listening":
                    host, port = value.rsplit(":", 1)
                    self.address = (host.strip("[]"), int(port))
                    self._process = process
                    return self
        finally:
            timer.cancel()
        code = process.wait()
        process.stdout.close()
        raise RuntimeError(f"zoe serve stopped with {code} before it listened")

    def stop(self):
        """Stop the server with SIGTERM, as an operator does."""
        process, self._process = self._process, None
        process.terminate()
        assert process.wait(TIMEOUT) == 0
        process.stdout.close()

    def kill(self):
        process, self._process = self._process, None
        if process is not None:
            process.kill()
            process.wait()
            process.stdout.close()

    def loads(self):
        """Return the number of `loadBefore` calls since the start."""
        url = f"http://{self.admin}/metrics"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
            text = reply.read().decode()
        for line in text.splitlines():
            if line.startswith('zoe_loads_total{storage="1"} '):
                return int(line.split()[-1])
        raise AssertionError(f"/metrics has no zoe_loads_total:\n{text}")


class Recorder:
    """A DB that records what the storage gives it."""

    def __init__(self):
        self.invalidations = []
        self.cache_invalidations = 0

    def invalidate(self, tid, oids, *args):
        self.invalidations.append((tid, set(oids)))

    def invalidateCache(self):
        self.cache_invalidations += 1

    def transform_record_data(self, data):
        return data

    def untransform_record_data(self, data):
        return data


def commit(storage, stores):
    """Commit the stores and return the tid.

    A failed step aborts the transaction and raises.
    """
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except BaseException:
        storage.tpc_abort(txn)
        raise


def commit_in_process(path, records):
    """Commit each record in its own transaction through `zoe.Storage`.

    Another client does this while no server runs. Return the last tid.
    """
    storage = zoe.Storage(str(path))
    try:
        for oid, data in records:
            try:
                serial = storage.load(oid)[1]
            except POSKeyError:
                serial = ZERO
            tid = commit(storage, [(oid, serial, data)])
    finally:
        storage.close()
    return tid


def wait_for(condition):
    """Wait until condition() is true. Fail after TIMEOUT."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.01)


def free_address():
    """Return a TCP address where nothing listens."""
    with socket.socket() as probe:
        probe.bind(("127.0.0.1", 0))
        return probe.getsockname()


@pytest.fixture
def serve(tmp_path):
    """Start servers. Kill them at the end of the test."""
    servers = []

    def start(name="store", *args, address=None):
        server = Server(tmp_path / name, args, address)
        servers.append(server)
        return server.start()

    yield start
    for server in servers:
        server.kill()


@pytest.fixture
def native():
    """Open native clients. Close them at the end of the test.

    A client tries to reconnect every 0.1 seconds.
    """
    opened = []

    def open_native(address, **kw):
        kw.setdefault("storage", "1")
        kw.setdefault("wait_timeout", DOWN)
        kw.setdefault("disconnect_poll", 0.1)
        storage = zoe.testing.native_client(address, **kw)
        opened.append(storage)
        return storage

    yield open_native
    for storage in opened:
        storage.close()


def cached(storage, server, *records):
    """Commit the records through storage, load them, and check the hits.

    Return the oids. Each record is the data of a new oid.
    """
    oids = [storage.new_oid() for _ in records]
    pairs = list(zip(oids, records, strict=True))
    commit(storage, [(oid, ZERO, data) for oid, data in pairs])
    for oid in oids:
        storage.load(oid)
    loads = server.loads()
    for oid, data in pairs:
        assert storage.load(oid)[0] == data
    assert server.loads() == loads, "the cache does not hold the records"
    return oids


def test_the_client_reconnects_after_a_restart(serve, native):
    server = serve()
    storage = native(server.address)
    (oid,) = cached(storage, server, b"before")

    server.stop()
    wait_for(lambda: not storage.is_connected())
    with pytest.raises(ClientDisconnected):
        storage.load(ABSENT)

    server.start()
    wait_for(storage.is_connected)
    serial = storage.load(oid)[1]
    tid = commit(storage, [(oid, serial, b"after")])
    assert storage.load(oid) == (b"after", tid)


def test_a_commit_while_disconnected_reaches_the_client(serve, native):
    server = serve()
    storage = native(server.address)
    db = Recorder()
    storage.registerDB(db)
    changed, kept = cached(storage, server, b"changed 1", b"kept")

    server.stop()
    wait_for(lambda: not storage.is_connected())
    tid = commit_in_process(server.path, [(changed, b"changed 2")])
    server.start()

    # Quick verification: the DB gets the invalidation of the tid once,
    # before lastTransaction() gives the tid.
    wait_for(lambda: storage.lastTransaction() == tid)
    assert db.invalidations == [(tid, {changed})]

    # The entry of the changed oid ends. The other entry stays.
    loads = server.loads()
    assert storage.load(kept)[0] == b"kept"
    assert server.loads() == loads
    assert storage.load(changed) == (b"changed 2", tid)
    assert server.loads() == loads + 1


def test_more_commits_than_the_queue_drop_the_cache(serve, native):
    server = serve("store", "--invalidation-queue-size", "2")
    storage = native(server.address)
    db = Recorder()
    storage.registerDB(db)
    changed, kept = cached(storage, server, b"changed 0", b"kept")

    server.stop()
    wait_for(lambda: not storage.is_connected())
    records = [(changed, f"changed {n}".encode()) for n in range(1, 4)]
    tid = commit_in_process(server.path, records)
    server.start()

    # getInvalidations answers None: the client drops its cache.
    wait_for(lambda: storage.lastTransaction() == tid)
    assert db.cache_invalidations >= 1
    loads = server.loads()
    assert storage.load(kept)[0] == b"kept"
    assert server.loads() == loads + 1
    assert storage.load(changed) == (b"changed 3", tid)


def test_the_arguments_that_zeo_ignores_change_nothing(serve, native):
    server = serve()
    storage = native(
        server.address,
        drop_cache_rather_verify=True,
        min_disconnect_poll=60,
        max_disconnect_poll=120,
    )
    db = Recorder()
    storage.registerDB(db)
    changed, kept = cached(storage, server, b"changed 1", b"kept")

    server.stop()
    wait_for(lambda: not storage.is_connected())
    tid = commit_in_process(server.path, [(changed, b"changed 2")])
    server.start()

    # The client reconnects with the delay of disconnect_poll, not of
    # min_disconnect_poll, and verifies its cache through getInvalidations.
    wait_for(lambda: storage.lastTransaction() == tid)
    assert db.invalidations == [(tid, {changed})]
    loads = server.loads()
    assert storage.load(kept)[0] == b"kept"
    assert server.loads() == loads


def test_a_cache_newer_than_the_server_is_refused(serve, native, caplog):
    server = serve()
    storage = native(server.address)
    cached(storage, server, b"newer")

    server.stop()
    wait_for(lambda: not storage.is_connected())
    # The same address serves an empty store: its tid is lower than the
    # tid of the cache.
    server.path = server.path.with_name("empty")
    server.start()

    def critical():
        return [r for r in caplog.records if r.levelno >= logging.CRITICAL]

    wait_for(critical)
    assert not storage.is_connected()
    with pytest.raises(ClientDisconnected):
        storage.load(ABSENT)


def test_a_transaction_open_at_the_drop_fails_and_leaves_no_lock(serve, native):
    from ZODB.Connection import TransactionMetaData

    server = serve()
    storage = native(server.address)
    oid = storage.new_oid()
    base = commit(storage, [(oid, ZERO, b"base")])

    # The vote takes the lock of the oid on the server.
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, base, b"lost", "", txn)
    storage.tpc_vote(txn)

    server.stop()
    server.start()
    wait_for(storage.is_connected)
    with pytest.raises(ClientDisconnected):
        storage.tpc_finish(txn)
    storage.tpc_abort(txn)

    # Another client commits the oid at once: no lock holds it, and the
    # lost transaction did not commit.
    other = native(server.address, wait_timeout=TIMEOUT)
    tid = commit(other, [(oid, base, b"other")])
    assert other.load(oid) == (b"other", tid)

    # The client commits again.
    wait_for(lambda: storage.lastTransaction() == tid)
    last = commit(storage, [(oid, tid, b"again")])
    assert storage.load(oid) == (b"again", last)


def test_read_only_fallback_takes_the_writable_server_when_it_starts(
    tmp_path, serve, native
):
    zoe.Storage(str(tmp_path / "read-only")).close()
    read_only = serve("read-only", "--read-only")
    address = free_address()
    storage = native([read_only.address, address], read_only_fallback=True)
    assert storage.isReadOnly()
    with pytest.raises(ReadOnlyError):
        commit(storage, [(p64(1), ZERO, b"refused")])

    # The client goes on trying the other address.
    writable = serve("writable", address=address)
    wait_for(lambda: not storage.isReadOnly())
    oid = storage.new_oid()
    tid = commit(storage, [(oid, ZERO, b"written")])
    check = native(writable.address)
    assert check.load(oid) == (b"written", tid)


def test_wait_false_opens_without_a_server(native):
    storage = native(free_address(), wait=False)
    assert not storage.is_connected()
    with pytest.raises(ClientDisconnected):
        storage.load(ABSENT)
