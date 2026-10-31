"""Unit tests of the rest of the storage API of the native client.

`just test` runs them, against `zoe serve`. They cover what the acceptance
test of `test_native_api.py` does not: the exceptions of `_call`, the
calls that the listener refuses, the sizes of `len()` and `getSize()`,
`record_iternext`, `history`, `pack`, `restore`, `server_status`, the
interfaces that the client declares, the serial of a store, a call without
`wait`, the iterators at the loss of the listener and at the close, an
iterator id of a lost listener, and `load_current`, which the cache answers.
"""

import socket
import threading
import time

import msgpack
import pytest
import ZODB.interfaces
from ZEO.Exceptions import ClientDisconnected
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import POSKeyError, StorageTransactionError
from ZODB.utils import p64, z64

import zoe.testing
import zoe.zeo
from zoe.testing import ZoeServer

TIMEOUT = 30


@pytest.fixture
def storage(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        native = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        try:
            yield native
        finally:
            native.close()


def commit(storage, records, tid=None):
    """Store `records` (oid: (serial, data)) in one transaction."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    for oid, (serial, data) in records.items():
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def test_load_current_comes_from_the_cache_without_the_server(tmp_path):
    from ZODB.utils import load_current, maxtid

    with ZoeServer(str(tmp_path / "store")) as server:
        # A load that the cache does not answer waits one second for a
        # server, then raises ClientDisconnected.
        storage = zoe.testing.native_client(server.address, storage="1", wait_timeout=1)
        try:
            oid = storage.new_oid()
            tid = commit(storage, {oid: (z64, b"data")})
            server.stop()
            assert storage.loadBefore(oid, maxtid) == (b"data", tid, None)
            assert load_current(storage, oid) == (b"data", tid)
        finally:
            storage.close()


def test_the_serial_of_a_store_takes_the_str_of_zeo_tests():
    assert zoe.zeo._serial(None) == z64
    assert zoe.zeo._serial(b"") == z64
    assert zoe.zeo._serial("\0" * 8) == z64
    assert zoe.zeo._serial(p64(5)) == p64(5)


def test_the_client_declares_the_interfaces_of_the_server(storage):
    for iface in (
        ZODB.interfaces.IStorageIteration,
        ZODB.interfaces.IStorageCurrentRecordIteration,
        ZODB.interfaces.IStorageRestoreable,
        ZODB.interfaces.IExternalGC,
        ZODB.interfaces.IStorageUndoable,
    ):
        assert iface.providedBy(storage), iface
    # The server undoes.
    assert storage.supportsUndo()


def test_an_error_of_the_server_raises_as_zeo_raises_it(storage):
    with pytest.raises(POSKeyError) as raised:
        storage.history(p64(42))
    # The oid stays bytes, as in ZEO.
    assert raised.value.args == (p64(42),)
    with pytest.raises(POSKeyError) as raised:
        storage.loadSerial(p64(42), p64(1))
    assert raised.value.args == (p64(42),)


def test_the_listener_refuses_a_call_of_a_transaction(storage):
    for method in ("tpc_begin", "storea", "vote", "tpc_finish"):
        with pytest.raises(StorageTransactionError):
            storage._call(method, 1)
        with pytest.raises(StorageTransactionError):
            storage._async(method, 1)


def test_len_and_get_size_follow_the_commits(storage):
    assert len(storage) == 0
    oids = [storage.new_oid() for _ in range(3)]
    commit(storage, {oid: (z64, b"x") for oid in oids})
    assert len(storage) == 3
    assert storage.getSize() > 0


def test_history_record_iternext_and_server_status(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"a")})
    second = commit(storage, {oid: (first, b"b")})
    history = storage.history(oid, 5)
    assert [entry["tid"] for entry in history] == [second, first]
    assert storage.record_iternext() == (oid, second, b"b", None)
    assert isinstance(storage.server_status(), dict)


def test_a_pack_runs_on_the_server(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"a")})
    second = commit(storage, {oid: (first, b"b")})
    assert storage.loadSerial(oid, first) == b"a"
    storage.pack(None, None)
    # No root reaches the object, so the pack removes each of its records.
    for serial in (first, second):
        with pytest.raises(POSKeyError):
            storage.loadSerial(oid, serial)


def test_a_restore_copies_a_record_and_a_deletion(storage):
    oid, gone = storage.new_oid(), storage.new_oid()
    last = commit(storage, {gone: (z64, b"x")})
    tid = p64(int.from_bytes(last, "big") + 1)
    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    storage.restore(oid, tid, b"copied", "", None, txn)
    storage.restore(gone, tid, None, "", None, txn)
    storage.tpc_vote(txn)
    assert storage.tpc_finish(txn) == tid
    assert storage.load(oid) == (b"copied", tid)
    with pytest.raises(POSKeyError):
        storage.load(gone)


def test_a_dropped_iterator_is_released_at_the_end_of_a_transaction(storage):
    commit(storage, {storage.new_oid(): (z64, b"a")})
    iterator = storage.iterator()
    next(iterator)
    iid = iterator._iid
    assert iid in storage._iterator_ids
    del iterator
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.tpc_abort(txn)
    assert iid not in storage._iterator_ids


def test_the_gc_of_a_disconnect_forgets_the_iterators(storage):
    commit(storage, {storage.new_oid(): (z64, b"a")})
    iterator = storage.iterator()
    next(iterator)
    storage._iterator_gc(True)
    assert not storage._iterator_ids
    with pytest.raises(ClientDisconnected):
        next(iterator)


def wait_until(condition, message):
    """Poll `condition` until it is true, for `TIMEOUT` seconds at most."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(message)
        time.sleep(0.01)


def test_the_loss_of_the_listener_forgets_the_iterators(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        try:
            for _ in range(2):
                commit(storage, {storage.new_oid(): (z64, b"a")})
            iterator = storage.iterator()
            next(iterator)
            assert storage._iterator_ids
            server.kill()
            # The thread of the deliveries gets the stale mark of the lost
            # listener.
            wait_until(
                lambda: not storage._iterator_ids, "the iterators stayed after the loss"
            )
            assert iterator._iid == -1
            with pytest.raises(ClientDisconnected):
                next(iterator)
        finally:
            storage.close()


def test_an_iterator_id_of_a_lost_listener_never_reaches_the_new_one(tmp_path):
    import gc

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        try:
            tids = [commit(storage, {storage.new_oid(): (z64, b"a")}) for _ in range(2)]
            old = storage.iterator()
            iid = old._iid
            server.stop()
            wait_until(lambda: old._iid == -1, "the loss kept the iterator")
            server.start()
            wait_until(storage.is_connected, "the client did not connect again")
            # The reply of an iterator_start on the lost listener comes to a
            # thread after the stale mark, which registers it.
            late = storage._setup_iterator(type(old), iid)
            new = storage.iterator()
            # The new server numbers the iterators of the connection from 0.
            assert int(new._iid) == int(iid)
            with pytest.raises(KeyError):
                next(late)
            # The collection of the old id sends nothing to the new server.
            del late
            gc.collect()
            storage._iterator_gc()
            assert [info.tid for info in new] == tids
        finally:
            storage.close()


class Tap:
    """A TCP proxy in front of a server. For each connection of a client,
    it keeps the methods that the client sent and whether the client
    closed its side.
    """

    def __init__(self, upstream):
        self.upstream = upstream
        self.methods = {}
        self.closed = set()
        self._lock = threading.Lock()
        self._listener = socket.create_server(("127.0.0.1", 0))
        self.address = self._listener.getsockname()[:2]
        threading.Thread(target=self._accept, daemon=True).start()

    def close(self):
        self._listener.close()

    def _accept(self):
        number = 0
        while True:
            try:
                client, _ = self._listener.accept()
            except OSError:
                return
            server = socket.create_connection(self.upstream)
            number += 1
            with self._lock:
                self.methods[number] = []
            for args in ((number, client, server), (None, server, client)):
                threading.Thread(target=self._pump, args=args, daemon=True).start()

    def _pump(self, number, source, sink):
        first = True
        try:
            while True:
                head = self._receive(source, 4)
                body = self._receive(source, int.from_bytes(head, "big"))
                if number is not None and not first:
                    message = msgpack.unpackb(body, raw=False, strict_map_key=False)
                    with self._lock:
                        self.methods[number].append(message[2])
                first = False
                sink.sendall(head + body)
        except (OSError, EOFError):
            pass
        finally:
            if number is not None:
                with self._lock:
                    self.closed.add(number)
            for sock in (source, sink):
                try:
                    sock.shutdown(socket.SHUT_RDWR)
                except OSError:
                    pass

    @staticmethod
    def _receive(sock, size):
        chunks = []
        while size:
            chunk = sock.recv(size)
            if not chunk:
                raise EOFError
            chunks.append(chunk)
            size -= len(chunk)
        return b"".join(chunks)

    def numbers(self, method):
        """Return the connections that sent `method`."""
        with self._lock:
            return {n for n, methods in self.methods.items() if method in methods}


def test_the_close_releases_the_iterators_with_their_connection(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        tap = Tap(server.address)
        try:
            storage = zoe.testing.native_client(
                tap.address, storage="1", wait_timeout=TIMEOUT
            )
            try:
                commit(storage, {storage.new_oid(): (z64, b"a")})
                iterator = storage.iterator()
                next(iterator)
                [number] = tap.numbers("iterator_start")
                assert number not in tap.closed
            finally:
                storage.close()
            # The server drops the iterators of a connection that closes.
            wait_until(
                lambda: number in tap.closed,
                "the connection of the iterator stayed open after the close",
            )
        finally:
            tap.close()


def test_a_call_without_wait_gives_a_future(storage):
    assert storage._call("ping", wait=False).result(TIMEOUT) is None
    future = storage._call("history", p64(42), 1, wait=False)
    with pytest.raises(POSKeyError):
        future.result(TIMEOUT)
