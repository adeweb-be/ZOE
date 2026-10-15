"""The ZOE client: `zoe.zeo.ClientStorage`, a drop-in for `ZEO.ClientStorage`.

`ClientStorage` is a subclass of `ZEO.ClientStorage.ClientStorage`, with the
same arguments. Each connection declares the extensions of `EXTENSIONS` in a
third argument of `register`. A ZOE server accepts the names that it serves
and gives them back under the key `extensions` of `get_info`. A stock ZEO
server takes two arguments only: it refuses the third with `TypeError`, and
the connection registers again with two arguments. The client then works as
`ZEO.ClientStorage` does.

`sequence(name).next()` gives the next value of the sequence `name` of the
store, an `int`, as `zoe.Storage` does: unique and strictly increasing per
store and per name, not part of a transaction. It needs a server that
accepted `sequences` on the connection. Without it, `next()` raises
`ZODB.POSException.Unsupported`.

When the package `opentelemetry` is installed, each connection also declares
`trace_context`. When the server accepts it, each call that the client makes
inside a span carries the W3C traceparent of that span: a fifth element of
the message, a map with the key `traceparent`. The span of the call on the
server then joins the trace of the client. A call outside a span has four
elements. The client exports no span: the application configures
OpenTelemetry.

`%import zoe` gives the ZConfig section `<zoeclient>`, which opens this class.
Its `<ssl>` section gives the files of TLS: the client builds its TLS from
them, as ZEO's `<clientstorage>` does.

`ClientStorage(..., native=True)` runs the client on the native client of
`zoe._zoe`, built on the Rust crate `zoe-client`, and not on the asyncio
client of ZEO. It is still an instance of `ZEO.ClientStorage.ClientStorage`,
and no thread of ZEO starts. It connects as ZEO does: to a `(host, port)`
address, to the path of a unix domain socket, or to the first server of a
list that opens, with TLS from the `<ssl>` section of `<zoeclient>`. It
takes no `ssl` context: TLS runs in Rust, and a context does not give back
the files of its certificate and key. It has one connection, one
transaction at a time, no cache, no blob and no reconnect. It declares
`sequences`, and `trace_context` when the package `opentelemetry` is
installed, and sends the traceparent as the client of ZEO does. It speaks
only the msgpack protocol `M5`: a stock ZEO server needs `msgpack true`.
`_NOT_BUILT` names the arguments whose feature it does not have yet.
`native` is false by default.
"""

import contextvars
import logging
import os
import threading

from ZEO.asyncio.client import ClientIO, ClientThread, Fallback, Protocol
from ZEO.asyncio.futures import run_coroutine_threadsafe
from ZEO.ClientStorage import ClientStorage as ZEOClientStorage
from ZEO.Exceptions import ClientDisconnected
from ZODB.POSException import (
    POSKeyError,
    ReadOnlyError,
    StorageTransactionError,
    Unsupported,
)
from ZODB.utils import maxtid, z64

try:
    from opentelemetry.trace import get_current_span
    from opentelemetry.trace.propagation.tracecontext import (
        TraceContextTextMapPropagator,
    )
except ImportError:
    _PROPAGATOR = None
else:
    _PROPAGATOR = TraceContextTextMapPropagator()

__all__ = ["EXTENSIONS", "ClientStorage", "Sequence"]

logger = logging.getLogger(__name__)

# The extension that lets a call carry the trace context of the client.
_TRACE_CONTEXT = "trace_context"

# The extensions that each connection declares in `register`. Without
# OpenTelemetry, the client has no trace context to send.
EXTENSIONS = ("sequences",) if _PROPAGATOR is None else ("sequences", _TRACE_CONTEXT)


def _traceparent():
    """Return the W3C traceparent of the current span, or None.

    Without a valid current span, or without OpenTelemetry, return None.
    """
    if _PROPAGATOR is None:
        return None
    # Without a valid span, build no carrier.
    if not get_current_span().get_span_context().is_valid:
        return None
    carrier = {}
    _PROPAGATOR.inject(carrier)
    return carrier.get("traceparent")


class _InContext:
    """A coroutine that runs each of its steps in the context of its caller.

    The network thread runs the coroutine. ZEO runs the steps of a coroutine
    without a context of their own, so a step sees the context that the
    network thread has at that time. This wrapper runs each step in a copy
    of the context of the thread that made the coroutine. So the current
    span of OpenTelemetry in a step is the span of the caller.
    """

    __slots__ = ("_coro", "_context")

    def __init__(self, coro):
        self._coro = coro
        self._context = contextvars.copy_context()

    def send(self, value):
        return self._context.run(self._coro.send, value)

    def throw(self, error):
        return self._context.run(self._coro.throw, error)

    def close(self):
        return self._coro.close()


class _Protocol(Protocol):
    """A connection to one server. It declares `EXTENSIONS` in `register`.

    When the server accepted `trace_context`, each call carries the
    traceparent of the current span.
    """

    # The connection sends the declaration until a server refuses it.
    declares = True
    # True when the server accepted `trace_context` on the connection.
    # `ClientStorage.notify_connected` sets it.
    traces = False

    def finish_connection(self, protocol_version):
        super().finish_connection(protocol_version)
        # A connection without a protocol in common has no encoder.
        self._encode = getattr(self, "encode", None)
        if self._encode is not None:
            self.encode = self._encode_call

    def _encode_call(self, message_id, async_, method, args):
        """Encode a call, with the traceparent of the current span when the
        connection traces."""
        traceparent = _traceparent() if self.traces else None
        return self._encode_traced(message_id, async_, method, args, traceparent)

    def _encode_traced(self, message_id, async_, method, args, traceparent):
        """Encode a call. With a `traceparent`, add the trace context."""
        if traceparent is None:
            return self._encode(message_id, async_, method, args)
        context = {"traceparent": traceparent}
        return self._encode(message_id, async_, method, args, context)

    def call_async_iter(self, it):
        # This is `call_async_iter` of ZEO, with the traceparent of now. The
        # socket can take the messages after this call ends, in another
        # context.
        traceparent = _traceparent() if self.traces else None
        self.write_message_iter(
            self._encode_traced(0, True, method, args, traceparent)
            for method, args in it
        )

    async def _register(self, read_only):
        if self.declares:
            try:
                return await self.server_call(
                    "register", self.storage_key, read_only, list(EXTENSIONS)
                )
            except TypeError:
                # A stock server takes two arguments. Its connection stays
                # open and unregistered, so the client registers again.
                self.declares = False
        return await self.server_call("register", self.storage_key, read_only)

    async def verify_connection(self):
        # This is `Protocol.verify_connection` of ZEO, with `_register`.
        async with self.client.register_lock:
            try:
                try:
                    server_tid = await self._register(
                        False if self.read_only is Fallback else self.read_only
                    )
                except ReadOnlyError:
                    if self.read_only is not Fallback:
                        raise
                    self.read_only = True
                    server_tid = await self._register(True)
                else:
                    if self.read_only is Fallback:
                        self.read_only = False
            except Exception as exc:
                self.client.register_failed(self, exc)
            else:
                await self.client.register(self, server_tid)


class _ClientIO(ClientIO):
    """The client in the network thread. Its connections are `_Protocol`."""

    def try_connecting(self):
        # This is `ClientIO.try_connecting` of ZEO, with `_Protocol`.
        if not self.closed:
            self.protocols = [
                _Protocol(
                    self.loop,
                    addr,
                    self,
                    self.storage_key,
                    self.read_only,
                    self.connect_poll,
                    ssl=self.ssl,
                    ssl_server_hostname=self.ssl_server_hostname,
                )
                for addr in self.addrs
            ]


class _ClientThread(ClientThread):
    """The network thread. It runs a `_ClientIO`."""

    def set_options(
        self,
        addrs,
        wrapper,
        cache,
        storage_key,
        read_only,
        timeout=30,
        disconnect_poll=1,
        **kwargs,
    ):
        super().set_options(
            addrs,
            wrapper,
            cache,
            storage_key,
            read_only,
            timeout,
            disconnect_poll,
            **kwargs,
        )
        self._io_args = (addrs, wrapper, cache, storage_key, read_only, disconnect_poll)
        self._io_kwargs = kwargs

    def setup_delegation(self, loop):
        # This is `ClientRunner.setup_delegation` of ZEO, with `_ClientIO`,
        # and an `io_call` that runs its coroutine in the context of its
        # caller. The asynchronous calls need no change: the event loop
        # runs a callback of `call_soon_threadsafe` in the context of its
        # caller.
        self.loop = loop
        self.client = client = _ClientIO(loop, *self._io_args, **self._io_kwargs)
        self.call_sync_co = client.call_sync_co
        self.load_before_co = client.load_before_co

        def io_call(coro, wait=True):
            if _PROPAGATOR is not None:
                # The call sees the current span of its caller.
                coro = _InContext(coro)
            future = run_coroutine_threadsafe(coro, loop)
            try:
                return future.result() if wait else future
            finally:
                del future

        self.io_call = io_call


class Sequence:
    """A named counter of the store of a `ClientStorage`.

    `next()` gives values that are unique and strictly increasing per store
    and per name, from 1.
    """

    def __init__(self, storage, name):
        self._storage = storage
        self._name = name

    def next(self):
        """Return the next value of the sequence.

        The value is not part of a transaction: an abort does not give it
        back. Each restart of the server can skip up to 100 values. Raise
        `Unsupported` when the server did not accept `sequences`.
        """
        return self._storage._sequence_next(self._name)


class ClientStorage(ZEOClientStorage):
    """`ZEO.ClientStorage` with the ZOE extensions that the server accepts.

    With `native=True`, the instance is a `_NativeClientStorage`: the
    native client runs inside.
    """

    def __new__(cls, *args, native=False, **kw):
        if native and cls is ClientStorage:
            cls = _NativeClientStorage
        elif native and not issubclass(cls, _NativeClientStorage):
            raise TypeError(
                f"{cls.__name__} runs the client of ZEO: native=True needs"
                " zoe.zeo.ClientStorage itself"
            )
        return super().__new__(cls)

    def __init__(
        self,
        *args,
        native=False,
        _client_factory=_ClientThread,
        _ssl_section=None,
        **kw,
    ):
        # `_ssl_section` is the `<ssl>` section of `<zoeclient>`. ZEO's
        # client takes the context that it gives.
        if _ssl_section is not None:
            from ZEO.zconfig import client_ssl

            kw["ssl"], server_hostname = client_ssl(_ssl_section)
            if kw.get("ssl_server_hostname") is None:
                kw["ssl_server_hostname"] = server_hostname
        super().__init__(*args, _client_factory=_client_factory, **kw)

    def notify_connected(self, conn, info):
        # The calls of the connection carry the trace context only when the
        # server accepted `trace_context`.
        conn.protocol.traces = _TRACE_CONTEXT in info.get("extensions", ())
        super().notify_connected(conn, info)
        # A server that got no declaration gives no key `extensions`. Then
        # remove the key of an earlier connection. The key stays during the
        # update, so a call of `next()` meanwhile does not fail.
        if "extensions" not in info:
            self._info.pop("extensions", None)

    def sequence(self, name):
        """Return the sequence `name` of the store."""
        if not isinstance(name, str):
            raise TypeError(
                f"the name of a sequence is a str, not {type(name).__name__}"
            )
        return Sequence(self, name)

    def _sequence_next(self, name):
        if "sequences" not in self._info.get("extensions", ()):
            raise Unsupported("the server does not serve the extension sequences")
        return self._call("sequence_next", name)


# The size of the cache of ZEO when the caller gives none.
_MB = 1024**2

# The arguments of `ZEO.ClientStorage` whose feature the native client does
# not have yet, each with the value that leaves its feature off. Any other
# value fails the open with an error that names the argument.
_NOT_BUILT = {
    "cache_size": 20 * _MB,
    "blob_dir": None,
    "shared_blob_dir": False,
    "blob_cache_size": None,
    "blob_cache_size_check": 10,
    "client_label": None,
    "cache": None,
    "client": None,
    "var": None,
    "wait": True,
    "server_sync": False,
}

# The extensions that the native client declares: those of the client of
# ZEO.
_NATIVE_EXTENSIONS = EXTENSIONS

# The number of oids that the native client asks for at a time.
_NEW_OIDS = 100

# The longest time that `close` waits for the thread of the invalidations.
_JOIN_TIMEOUT = 30


def _as_bytes(value):
    """Return the bytes of a user name or a description."""
    return value.encode("utf-8") if isinstance(value, str) else bytes(value)


def _native_tls(section, server_hostname):
    """Return the TLS of the native client from an `<ssl>` section.

    The tuple holds the certificate, the key or None, the CA file of
    `authenticate`, `check-hostname`, and the server name: `server_hostname`,
    else the `server-hostname` of the section, else None. Raise `Unsupported`
    for what the native client does not read: an encrypted key, no
    `authenticate`, or a directory as `authenticate`.
    """
    if section.password_function:
        raise Unsupported(
            "the native client reads only a key that is not encrypted: "
            "leave password-function out of <ssl>"
        )
    if not section.authenticate:
        raise Unsupported(
            "the native client checks the server against the CA "
            "certificates of a file: give authenticate in <ssl>"
        )
    if os.path.isdir(section.authenticate):
        raise Unsupported(
            "the native client takes a file of CA certificates as "
            f"authenticate, not the directory {section.authenticate}"
        )
    key = os.path.abspath(section.key) if section.key else None
    return (
        os.path.abspath(section.certificate),
        key,
        os.path.abspath(section.authenticate),
        bool(section.check_hostname),
        server_hostname or section.server_hostname or None,
    )


def _native_address(addr):
    """Return an address for the native client.

    A `(host, port)` address with an empty host is on `127.0.0.1`, as for
    ZEO. The path of a unix domain socket stays as it is.
    """
    if isinstance(addr, tuple) and len(addr) == 2:
        host, port = addr
        return (host or "127.0.0.1", port)
    return addr


class _NativeClientStorage(ClientStorage):
    """`zoe.zeo.ClientStorage` with the native client inside.

    It takes the arguments of `ZEO.ClientStorage`. It ignores the arguments
    that ZEO 6 ignores too. Each argument of `_NOT_BUILT` must keep the
    value that leaves its feature off. `native` must stay true. `ssl` must
    stay None: the TLS comes from the `<ssl>` section of `<zoeclient>`.

    With several addresses, the client connects to all of them and keeps
    one server: with `read_only_fallback`, a writable server before a
    read-only one.

    A thread of the client gives the invalidations of the other clients to
    the DB, in tid order. `lastTransaction()` gives a tid only after the DB
    got the invalidations of that tid. `tpc_finish` calls its callback
    before `lastTransaction()` gives the tid to another thread.
    """

    def __init__(
        self,
        addr,
        storage="1",
        cache_size=20 * _MB,
        name="",
        wait_timeout=None,
        disconnect_poll=None,
        read_only=0,
        read_only_fallback=0,
        blob_dir=None,
        shared_blob_dir=False,
        blob_cache_size=None,
        blob_cache_size_check=10,
        client_label=None,
        cache=None,
        ssl=None,
        ssl_server_hostname=None,
        client=None,
        var=None,
        min_disconnect_poll=1,
        max_disconnect_poll=None,
        wait=True,
        drop_cache_rather_verify=True,
        credentials=None,
        server_sync=False,
        username=None,
        password=None,
        realm=None,
        native=True,
        _ssl_section=None,
    ):
        if not native:
            raise ValueError("this class runs the native client: native is true")
        given = locals()
        # Refuse a feature that is not built before any other step.
        for key, off in _NOT_BUILT.items():
            if given[key] != off:
                raise Unsupported(
                    f"the native client does not have {key} yet: leave {key} out"
                )
        if ssl is not None:
            raise Unsupported(
                "the native client takes no ssl context: give the files of "
                "TLS in the <ssl> section of <zoeclient>"
            )
        tls = None
        if _ssl_section is not None:
            tls = _native_tls(_ssl_section, ssl_server_hostname)
        self.__name__ = name or str(addr)
        addr = self._normalize_addr(addr)
        # ZEO's tests read `_addr` to open a second client.
        self._addr = addr
        self._storage = storage
        self._is_read_only = bool(read_only)
        self._read_only_fallback = bool(read_only_fallback)
        self._server_addr = None
        self._client_label = None
        self._info = {
            "length": 0,
            "size": 0,
            "name": "ZEO Client",
            "supportsUndo": 0,
            "interfaces": (),
        }
        self._db = None
        self._oids = []
        self._oids_lock = threading.Lock()
        # A child of `os.fork()` must not use the oids of its parent.
        self._pid = os.getpid()
        self.blob_dir = None
        self.shared_blob_dir = False
        self.fshelper = None
        self.server_sync = False
        self.protocol_version = b"M5"
        # One transaction at a time, as ZEO's client.
        self._commit_lock = threading.Lock()
        self._transaction = None
        # The delivery of the invalidations, `lastTransaction()` and the
        # callback of `tpc_finish` take this lock.
        self._deliver_lock = threading.RLock()
        self._delivery = None

        from zoe import _zoe

        self._native = _zoe.Client(
            [_native_address(one) for one in addr],
            storage,
            read_only=self._is_read_only,
            read_only_fallback=self._read_only_fallback,
            extensions=list(_NATIVE_EXTENSIONS),
            wait_timeout=float(wait_timeout or 30),
            tls=tls,
            traceparent=_traceparent,
        )
        try:
            # A read-only fallback registers read-only.
            self._is_read_only = self._native.read_only()
            self._last_tid = self._native.opened_at()
            self._info.update(self._native.info())
            self.set_server_addr(self._native.address())
            self._connection_generation = 1
            logger.info(
                "%s native client connected to storage %r at %s",
                self.__name__,
                storage,
                self._server_addr,
            )
            self._delivery = threading.Thread(
                target=self._deliver_forever,
                name=f"{self.__name__} zoe native client invalidations",
                daemon=True,
            )
            self._delivery.start()
        except BaseException:
            self._native.close()
            raise

    def _check_process(self):
        """Raise `RuntimeError` in a child of `os.fork()`.

        The check comes before any lock: a thread of the parent can hold a
        lock at the fork, and the child would wait for it for ever.
        """
        if os.getpid() != self._pid:
            raise RuntimeError(
                "the client was opened in another process, before a fork"
            )

    # The calls that the native client does not have yet.

    def _call(self, method, *args, **kw):
        raise Unsupported(f"the native client does not call {method} yet")

    def _async(self, method, *args):
        raise Unsupported(f"the native client does not call {method} yet")

    def _async_iter(self, it):
        raise Unsupported("the native client does not upload blobs yet")

    def new_addr(self, addr):
        raise Unsupported("the native client does not change its address yet")

    def loadBlob(self, oid, serial):
        raise Unsupported("the native client has no blobs yet")

    def storeBlob(self, oid, serial, data, blobfilename, version, txn):
        raise Unsupported("the native client has no blobs yet")

    def prefetch(self, oids, tid):
        # The client has no cache, so a prefetch has nothing to fill.
        pass

    # The state of the connection.

    def is_connected(self, test=False):
        return self._native.is_connected()

    def is_read_only(self):
        return self._is_read_only

    isReadOnly = is_read_only

    def supportsUndo(self):
        return False

    def close(self):
        self._check_process()
        self._native.close()
        delivery = self._delivery
        if delivery is not None and delivery is not threading.current_thread():
            delivery.join(_JOIN_TIMEOUT)

    # The invalidations.

    def _deliver_forever(self):
        """Give each invalidation to the DB until the connection closes."""
        native = self._native
        try:
            while native.wait_invalidation():
                with self._deliver_lock:
                    self._deliver()
        except Exception:
            logger.exception("%s the invalidations stopped", self.__name__)

    def _deliver(self, tid=None, callback=None):
        """Give the invalidations that wait to the DB, in tid order.

        With `tid`, call `callback(tid)` for the commit of this client at
        its place in the tid order. The caller holds `_deliver_lock`.
        """
        lagged, items = self._native.drain()
        if lagged and self._db is not None:
            # The client missed invalidations: the cache of the DB is stale.
            self._db.invalidateCache()
        failure = None
        for item_tid, oids in items:
            if tid is not None and item_tid > tid:
                failure = self._committed(tid, callback)
                tid = None
            if self._db is not None:
                self._db.invalidate(item_tid, oids)
            self._last_tid = max(self._last_tid, item_tid)
        if tid is not None:
            failure = self._committed(tid, callback)
        if failure is not None:
            raise failure

    def _committed(self, tid, callback):
        """Call `callback(tid)` and raise the last tid to `tid`.

        Return the exception of the callback, or None.
        """
        try:
            callback(tid)
        except Exception as error:
            return error
        finally:
            self._last_tid = max(self._last_tid, tid)
        return None

    def lastTransaction(self):
        self._check_process()
        with self._deliver_lock:
            self._deliver()
            return self._last_tid

    # The loads.

    def load(self, oid, version=""):
        result = self.loadBefore(oid, maxtid)
        if result is None:
            raise POSKeyError(oid)
        return result[:2]

    def loadBefore(self, oid, tid):
        return self._native.load_before(oid, tid)

    def loadSerial(self, oid, serial):
        return self._native.load_serial(oid, serial)

    def getTid(self, oid):
        return self.load(oid)[1]

    def new_oid(self):
        self._check_process()
        if self._is_read_only:
            raise ReadOnlyError()
        with self._oids_lock:
            if not self._oids:
                self._oids = list(reversed(self._native.new_oids(_NEW_OIDS)))
            return self._oids.pop()

    def _sequence_next(self, name):
        return self._native.sequence_next(name)

    # The two-phase commit.

    def _check_trans(self, trans, meth):
        if self._is_read_only:
            raise ReadOnlyError()
        if trans is None or trans is not self._transaction:
            raise StorageTransactionError("Transaction not committing", meth, trans)

    def tpc_transaction(self):
        return self._transaction

    def tpc_begin(self, txn, tid=None, status=" "):
        self._check_process()
        if self._is_read_only:
            raise ReadOnlyError()
        if txn is self._transaction:
            raise StorageTransactionError(
                "Duplicate tpc_begin calls for same transaction"
            )
        self._commit_lock.acquire()
        try:
            if isinstance(status, bytes):
                status = status.decode("ascii")
            self._native.begin(
                _as_bytes(txn.user),
                _as_bytes(txn.description),
                txn.extension,
                tid,
                status,
            )
        except BaseException:
            self._commit_lock.release()
            raise
        self._transaction = txn

    def store(self, oid, serial, data, version, txn):
        if version:
            raise Unsupported("versions are not supported")
        self._check_trans(txn, "store")
        self._native.store(oid, serial or z64, data)

    def checkCurrentSerialInTransaction(self, oid, serial, transaction):
        self._check_trans(transaction, "checkCurrentSerialInTransaction")
        self._native.check_current_serial(oid, serial)

    def tpc_vote(self, txn):
        self._check_trans(txn, "tpc_vote")
        return self._native.vote() or None

    def tpc_finish(self, txn, f=lambda tid: None):
        self._check_process()
        self._check_trans(txn, "tpc_finish")
        try:
            # The lock covers the round trip. So the thread of the
            # invalidations gives no later tid to the DB before `f(tid)`.
            # The call waits only for the server, never for that thread.
            with self._deliver_lock:
                tid = self._native.finish()
                self._deliver(tid, f)
        finally:
            self.tpc_end(txn)
        return tid

    def tpc_abort(self, txn, timeout=None):
        self._check_process()
        if txn is None or txn is not self._transaction:
            return
        try:
            self._native.abort()
        except ClientDisconnected:
            logger.debug("%s ClientDisconnected in tpc_abort() ignored", self.__name__)
        finally:
            self.tpc_end(txn)

    def tpc_end(self, txn):
        if txn is not None and txn is self._transaction:
            self._transaction = None
            self._commit_lock.release()
