"""The asyncio client of ZEO inside `zoe.zeo.ClientStorage`.

`ClientStorage` is the class of the package. With `native=False`, and for
a subclass, it runs the client of ZEO, with the patches of this module: a
connection declares `EXTENSIONS` in `register`, falls back to two
arguments on a stock server, and sends the traceparent of the current span
when the server accepted `trace_context`. Without `native`, the class makes
a `zoe.zeo._native._NativeClientStorage`.
"""

import contextvars

from ZEO.asyncio.client import ClientIO, ClientThread, Fallback, Protocol
from ZEO.asyncio.futures import run_coroutine_threadsafe
from ZEO.ClientStorage import ClientStorage as ZEOClientStorage
from ZODB.POSException import ReadOnlyError, Unsupported

from zoe.kv import _check_flavour

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

# The extension that lets a call carry the trace context of the client.
_TRACE_CONTEXT = "trace_context"

# The extension of the KV namespaces.
_KV = "kv"

# The extensions that each connection declares in `register`. Without
# OpenTelemetry, the client has no trace context to send.
_TRACES = () if _PROPAGATOR is None else (_TRACE_CONTEXT,)
# The extension of the early write-intent locks (ADR 0045).
_LOCK_INTENT = "lock_intent"
EXTENSIONS = ("sequences", _LOCK_INTENT, _KV, *_TRACES)


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

    By default, the instance is a `_NativeClientStorage`: the native client
    runs inside. With `native=False`, the asyncio client of ZEO runs
    inside. A subclass runs the asyncio client of ZEO by default, and
    `native=True` on it raises `TypeError`.

    Without `native`, a server that speaks only the pickle protocol `Z5`
    at the open with `wait` gives the asyncio client of ZEO inside: the
    instance becomes a `ClientStorage` with `native=False`. With
    `native=True`, such a server is a failed connection.
    """

    def __new__(cls, *args, native=None, **kw):
        from zoe.zeo._native import _NativeClientStorage

        fall_back = native is None and cls is ClientStorage
        if native is None:
            native = issubclass(cls, _NativeClientStorage) or cls is ClientStorage
        if native and cls is ClientStorage:
            cls = _NativeClientStorage
        elif native and not issubclass(cls, _NativeClientStorage):
            raise TypeError(
                f"{cls.__name__} runs the client of ZEO: native=True needs"
                " zoe.zeo.ClientStorage itself"
            )
        instance = super().__new__(cls)
        if fall_back:
            # `_NativeClientStorage.__init__` reads and removes the mark.
            instance._fall_back = True
        return instance

    def __init__(
        self,
        *args,
        native=None,
        early_locks=False,
        early_lock_classes=None,
        _client_factory=_ClientThread,
        _ssl_section=None,
        _prefetch_references=None,
        **kw,
    ):
        # The asyncio client of ZEO has one session and one commit lock for
        # every thread of the process, so it takes no early locks.
        if early_locks:
            raise ValueError("early-locks on needs the native client: native true")
        self.early_lock_classes = None
        # The asyncio client of ZEO prefetches no references, so it ignores
        # `_prefetch_references`, the key `prefetch-references` of
        # `<zoeclient>`.
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

    # The KV namespaces.

    def kv_supported(self):
        """Tell if the server accepted `kv` on the connection."""
        return _KV in self._info.get("extensions", ())

    def _check_kv(self, flavour=None):
        if not self.kv_supported():
            raise Unsupported("the server does not serve the extension kv")
        if flavour is not None:
            _check_flavour(flavour)

    def kv_get(self, flavour, name, key):
        """Return the value of `key` in the namespace `name` of `flavour`,
        `"ephemeral"` or `"durable"`, as bytes, or None when the key has no
        value or is expired. The read gives the last committed value."""
        self._check_kv(flavour)
        return self._call("kv_get", flavour, name, key)

    def kv_scan(self, flavour, name, start=None, stop=None, limit=1000):
        """Return the pairs `(key, value)` of the namespace `name` of
        `flavour`, in the byte order of the keys, from `start` (included)
        to `stop` (excluded), at most `limit` and at most 1,000."""
        self._check_kv(flavour)
        pairs = self._call("kv_scan", flavour, name, start, stop, limit)
        return [(key, value) for key, value in pairs]

    def kv_generation(self, name):
        """Return the generation of the durable namespace `name`, the 8
        bytes of the tid of the last commit that wrote it."""
        self._check_kv()
        return self._call("kv_generation", name)

    def kv_set(self, flavour, name, key, value, ttl, transaction):
        """Stage a write of `value` for `key` in the namespace `name` of
        `flavour` in `transaction`, with the rules of `store`. An ephemeral
        key needs `ttl`, in seconds; a durable key takes None. The server
        reports an error of the write at the vote."""
        self._check_kv(flavour)
        self._check_trans(transaction, "kv_set")
        self._async("kv_set", flavour, name, key, value, ttl)

    def kv_delete(self, flavour, name, key, transaction):
        """Stage a delete of `key` in the namespace `name` of `flavour` in
        `transaction`, with the rules of `kv_set`."""
        self._check_kv(flavour)
        self._check_trans(transaction, "kv_delete")
        self._async("kv_delete", flavour, name, key)
