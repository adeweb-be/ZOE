"""Acceptance test of the rest of the storage API in the native client.

`just test` runs this module. The native client gives each method of
`ZEO.ClientStorage`, with what `ZEO.ClientStorage` gives for the same
server answer:

- `prefetch(oids, tid)` sends one `loadBefore` for each oid at once and
  does not wait for the replies. The replies fill the cache, so the loads
  that follow do not reach the server.
- `sync()` with `server_sync` sends `ping` and returns after its reply.
- `client_label` names the connection in the log lines of the server.
- `deleteObject` deletes an object, so its next load raises
  `POSKeyError`.
- `iterator()` gives the transactions in order, with their records. When
  the Python iterator is dropped, `iterator_gc` releases the iterator of
  the server.
- `new_addr` moves the client to another server.

The server counts each `loadBefore` in `zoe_loads_total` on the admin port
(`--admin-listen`). A small TCP proxy between the client and the server
keeps the messages of both sides, so a test sees what reached the server.
The tests open the native client through `zoe.testing.native_client`.
"""

import gc
import itertools
import json
import os
import socket
import subprocess
import threading
import time
import urllib.request

import msgpack
import pytest
from ZODB.Connection import TransactionMetaData
from ZODB.utils import p64, u64

import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30
ZERO = bytes(8)
LABEL = "native-api-worker"


def native(address, **kw):
    """Open a native client of the storage 1 at `address`."""
    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.testing.native_client(address, storage="1", **kw)


def stock(address):
    """Open a stock ZEO client of the storage 1 at `address`."""
    from ZEO.ClientStorage import ClientStorage

    return ClientStorage(address, storage="1", wait_timeout=TIMEOUT)


def commit(storage, records, serials=None):
    """Store `records` (oid: data) in one transaction. Return its tid."""
    serials = serials or {}
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    for oid, data in records.items():
        storage.store(oid, serials.get(oid, ZERO), data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def wait_until(condition, message):
    """Poll `condition` until it is true, for `TIMEOUT` seconds at most."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(message)
        time.sleep(0.01)


class AdminServer(ZoeServer):
    """`zoe serve` with an admin port on 127.0.0.1, and its log in a file.

    The server writes the line `admin ADDRESS` before the line `listening
    ADDRESS`. `ZoeServer.start` reads past the first line, so this class
    starts the process itself. `admin` is the address of the admin port.
    With `log`, the log lines of the server go to that file.
    """

    def __init__(self, path, args=(), log=None):
        super().__init__(path, args=["--admin-listen", "127.0.0.1:0", *args])
        self.log = log
        self.admin = None

    def start(self, timeout=TIMEOUT):
        command = [self.binary(), "serve", "--path", str(self.path)]
        command += ["--listen", "127.0.0.1:0", *self.args]
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        stderr = open(self.log, "ab") if self.log else None
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=stderr,
                env=environ,
                text=True,
                # On Linux, the kernel kills the server when this thread
                # ends, as for ZoeServer.
                preexec_fn=zoe.testing._die_with_parent(),
            )
        finally:
            if stderr is not None:
                stderr.close()
        timer = threading.Timer(timeout, process.kill)
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
        process.wait()
        process.stdout.close()
        raise RuntimeError("zoe serve stopped before it listened")

    def metric(self, name):
        """Return the sample of the metric `name` for the storage 1."""
        url = f"http://{self.admin}/metrics"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
            text = reply.read().decode()
        prefix = name + '{storage="1"} '
        for line in text.splitlines():
            if line.startswith(prefix):
                return float(line[len(prefix) :])
        raise AssertionError(f"/metrics has no sample of {name} for the storage 1")


def receive(sock, size):
    """Read exactly `size` bytes from `sock`. Raise EOFError at the end."""
    chunks = []
    while size:
        chunk = sock.recv(size)
        if not chunk:
            raise EOFError
        chunks.append(chunk)
        size -= len(chunk)
    return b"".join(chunks)


class Wire:
    """A TCP proxy in front of a ZEO server that keeps the messages.

    A client connects to `address`, and the proxy connects to `upstream`.
    A frame is a 4-byte big-endian length, then its bytes. The first frame
    of each side names the protocol. Each later frame is a msgpack array:
    the message id, the flags, the method (`.reply` for a reply) and the
    arguments. `calls` keeps `(connection, message)` for each frame of a
    client, and `replies` for each frame of the server, before the proxy
    passes the frame on.
    """

    def __init__(self, upstream):
        self.upstream = upstream
        self.calls = []
        self.replies = []
        self._lock = threading.Lock()
        self._sockets = []
        self._listener = socket.create_server(("127.0.0.1", 0))
        self.address = self._listener.getsockname()[:2]
        threading.Thread(target=self._accept, daemon=True).start()

    def __enter__(self):
        return self

    def __exit__(self, *exc_info):
        self.close()

    def close(self):
        try:
            self._listener.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        self._listener.close()
        with self._lock:
            sockets, self._sockets = self._sockets, []
        for sock in sockets:
            sock.close()

    def kept(self, messages):
        """Return a copy of `calls` or `replies`."""
        with self._lock:
            return list(messages)

    def _accept(self):
        for number in itertools.count():
            try:
                client, _ = self._listener.accept()
            except OSError:
                return
            server = socket.create_connection(self.upstream)
            with self._lock:
                self._sockets += [client, server]
            for source, sink, out in (
                (client, server, self.calls),
                (server, client, self.replies),
            ):
                threading.Thread(
                    target=self._pump, args=(number, source, sink, out), daemon=True
                ).start()

    def _pump(self, number, source, sink, out):
        first = True
        try:
            while True:
                head = receive(source, 4)
                body = receive(source, int.from_bytes(head, "big"))
                if not first:
                    message = msgpack.unpackb(
                        body, raw=False, use_list=False, strict_map_key=False
                    )
                    with self._lock:
                        out.append((number, message))
                first = False
                sink.sendall(head + body)
        except (OSError, EOFError):
            pass
        finally:
            try:
                sink.shutdown(socket.SHUT_WR)
            except OSError:
                pass


def values_of(value, key):
    """Yield each value of `key` anywhere in a JSON value."""
    if isinstance(value, dict):
        for k, v in value.items():
            if k == key:
                yield v
            yield from values_of(v, key)
    elif isinstance(value, list):
        for item in value:
            yield from values_of(item, key)


def test_prefetch_loads_at_once_and_fills_the_cache(tmp_path):
    records = {p64(n): b"record %d" % n for n in range(1, 101)}
    with AdminServer(tmp_path / "store") as server:
        writer = stock(server.address)
        try:
            commit(writer, records)
        finally:
            writer.close()

        storage = native(server.address)
        try:
            loads = server.metric("zoe_loads_total")
            ended = server.metric("zoe_load_latency_seconds_count")
            # ZODB prefetches at the start of the snapshot of a connection.
            start = p64(u64(storage.lastTransaction()) + 1)
            storage.prefetch(list(records), start)
            # prefetch does not wait for the replies. Wait until the
            # server ended the 100 calls.
            wait_until(
                lambda: server.metric("zoe_load_latency_seconds_count") - ended >= 100,
                "the server did not get the 100 loads of the prefetch",
            )
            assert server.metric("zoe_loads_total") - loads == 100

            for oid, data in records.items():
                assert storage.load(oid)[0] == data
            assert server.metric("zoe_loads_total") - loads == 100
        finally:
            storage.close()


def test_sync_with_server_sync_reaches_the_server_before_it_returns(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server, Wire(server.address) as wire:
        storage = native(wire.address, server_sync=True)
        try:
            seen = len(wire.kept(wire.calls))
            storage.sync()
            # Take both lists at the return of sync.
            calls = wire.kept(wire.calls)[seen:]
            replies = wire.kept(wire.replies)
        finally:
            storage.close()

    pings = {(number, message[0]) for number, message in calls if message[2] == "ping"}
    assert pings, "sync() sent no ping to the server"
    answered = {
        (number, message[0]) for number, message in replies if message[2] == ".reply"
    }
    assert pings & answered, "sync() returned before the reply of its ping"


def test_the_client_label_names_the_connection_in_the_log_of_the_server(tmp_path):
    log = tmp_path / "server.log"
    args = ["--log-format", "json", "--lock-warn", "1"]
    with AdminServer(tmp_path / "store", args, log) as server:
        storage = native(server.address, client_label=LABEL)
        try:
            oid = storage.new_oid()
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.store(oid, ZERO, b"held", "", txn)
            storage.tpc_vote(txn)
            # The vote holds the lock of the oid. The server logs a lock
            # held longer than --lock-warn when the finish releases it.
            time.sleep(1.3)
            storage.tpc_finish(txn)
        finally:
            storage.close()

        def logged():
            for line in log.read_text().splitlines():
                try:
                    event = json.loads(line)
                except ValueError:
                    continue
                clients = [v for v in values_of(event, "client") if isinstance(v, str)]
                if any(LABEL in client for client in clients):
                    return True
            return False

        wait_until(logged, f"no log line of the server names the client {LABEL}")


def test_delete_object_makes_the_next_load_raise_pos_key_error(tmp_path):
    from ZODB.POSException import POSKeyError

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = native(server.address)
        try:
            oid = storage.new_oid()
            serial = commit(storage, {oid: b"current"})
            assert storage.load(oid) == (b"current", serial)

            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.deleteObject(oid, serial, txn)
            storage.tpc_vote(txn)
            storage.tpc_finish(txn)

            with pytest.raises(POSKeyError):
                storage.load(oid)
        finally:
            storage.close()


def test_an_iterator_gives_its_transactions_in_order_and_is_released(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server, Wire(server.address) as wire:
        storage = native(wire.address)
        try:
            committed = []
            for n in range(50):
                oid = storage.new_oid()
                data = b"transaction %d" % n
                committed.append((commit(storage, {oid: data}), [(oid, data)]))

            given = [
                (info.tid, [(record.oid, record.data) for record in info])
                for info in storage.iterator()
            ]
            assert given == committed

            # An iterator before its end stays open on the server.
            iterator = storage.iterator()
            assert next(iterator).tid == committed[0][0]
            number, call = [
                (number, message)
                for number, message in wire.kept(wire.calls)
                if message[2] == "iterator_start"
            ][-1]
            [iid] = [
                message[3]
                for n, message in wire.kept(wire.replies)
                if n == number and message[0] == call[0] and message[2] == ".reply"
            ]

            del iterator
            gc.collect()
            # A stock client sends iterator_gc at its next transaction
            # boundary. The native client may send it before.
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.tpc_abort(txn)

            def released():
                return any(
                    n == number and message[2] == "iterator_gc" and iid in message[3][0]
                    for n, message in wire.kept(wire.calls)
                )

            wait_until(released, f"no iterator_gc released the iterator {iid}")
        finally:
            storage.close()


def test_new_addr_moves_the_client_to_another_server(tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    oid = p64(1)
    with (
        ZoeServer(str(tmp_path / "first")) as first,
        ZoeServer(str(tmp_path / "second")) as second,
    ):
        tids = []
        for server, data in ((first, b"on the first"), (second, b"on the second")):
            writer = stock(server.address)
            try:
                tids.append(commit(writer, {oid: data}))
            finally:
                writer.close()
        # The second commit is the later one, so the cache of the client is
        # never newer than the second server.
        second_tid = tids[-1]

        storage = native(first.address)
        try:
            assert storage.load(oid)[0] == b"on the first"
            storage.new_addr(second.address)
            first.kill()

            def moved():
                try:
                    return storage.load(oid)[0] == b"on the second"
                except ClientDisconnected:
                    return False

            wait_until(moved, "the client did not move to the second server")
            assert storage.lastTransaction() == second_tid
        finally:
            storage.close()
