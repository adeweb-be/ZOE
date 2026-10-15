"""Acceptance test of `trace_context` through the native client.

`just test` runs this module. When the package `opentelemetry` is
installed, the native client declares the extension `trace_context` in
`register`. Each call then carries the W3C traceparent of the current span
of the calling thread, as a fifth element of the message: a map with the
key `traceparent`. `zoe serve` exports its spans over OTLP, and the span
of the server for a call has the span of the client as its parent. So a
slow Plone request and the server work that it caused appear in one
trace.

- A call outside any span carries no fifth element.
- Without `opentelemetry`, the client declares no `trace_context`, and the
  server sees calls of four elements.
- Against a stock ZEO server, which refuses the declaration, the client
  declares nothing, sends calls of four elements, and works.

The test runs a small OTLP/HTTP collector that takes the JSON encoding,
and a TCP proxy that keeps the messages between the client and the
server. The tests open the native client through
`zoe.testing.native_client`.
"""

import json
import pickle
import socket
import subprocess
import sys
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

import msgpack
from ZODB.Connection import TransactionMetaData

import zoe.testing
from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30

# A client process without opentelemetry: the entry None in sys.modules
# makes each import of the package fail. It commits and loads one record.
WITHOUT_OPENTELEMETRY = """\
import sys

sys.modules["opentelemetry"] = None

from ZODB.Connection import TransactionMetaData

import zoe.testing

host, port = sys.argv[1:]
storage = zoe.testing.native_client((host, int(port)), storage="1", wait_timeout=30)
try:
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, bytes(8), b"data", "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    assert storage.load(oid) == (b"data", tid)
finally:
    storage.close()
"""


class Collector:
    """An OTLP/HTTP collector that keeps the JSON bodies of the traces."""

    def __init__(self):
        bodies = self.bodies = []

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                length = int(self.headers.get("Content-Length", 0))
                body = self.rfile.read(length)
                if self.path.endswith("/v1/traces"):
                    bodies.append(json.loads(body))
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args):
                pass

        self.server = HTTPServer(("127.0.0.1", 0), Handler)
        self.url = f"http://127.0.0.1:{self.server.server_port}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def spans(self):
        return [
            span
            for body in self.bodies
            for resource in body.get("resourceSpans", [])
            for scope in resource.get("scopeSpans", [])
            for span in scope.get("spans", [])
        ]

    def close(self):
        self.server.shutdown()


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
    the message id, the flags, the method (`.reply` for a reply), the
    arguments, and on a connection that declared `trace_context`, the map
    of the trace. `calls` keeps the messages of the clients, before the
    proxy passes them on.
    """

    def __init__(self, upstream):
        self.upstream = upstream
        self.calls = []
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

    def kept(self):
        """Return a copy of `calls`."""
        with self._lock:
            return list(self.calls)

    def _accept(self):
        while True:
            try:
                client, _ = self._listener.accept()
            except OSError:
                return
            server = socket.create_connection(self.upstream)
            with self._lock:
                self._sockets += [client, server]
            threading.Thread(
                target=self._pump, args=(client, server, self.calls), daemon=True
            ).start()
            threading.Thread(
                target=self._pump, args=(server, client, None), daemon=True
            ).start()

    def _pump(self, source, sink, out):
        first = True
        try:
            while True:
                head = receive(source, 4)
                body = receive(source, int.from_bytes(head, "big"))
                if out is not None and not first:
                    message = msgpack.unpackb(
                        body, raw=False, use_list=False, strict_map_key=False
                    )
                    with self._lock:
                        out.append(message)
                first = False
                sink.sendall(head + body)
        except (OSError, EOFError):
            pass
        finally:
            try:
                sink.shutdown(socket.SHUT_WR)
            except OSError:
                pass


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def native(address):
    return zoe.testing.native_client(address, storage="1", wait_timeout=TIMEOUT)


def commit(storage, data):
    """Store `data` under a new oid in one transaction. Return the oid."""
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, ZERO, data, "", txn)
    storage.tpc_vote(txn)
    storage.tpc_finish(txn)
    return oid


def declared(calls):
    """Return the extensions that each `register` call declares."""
    return [
        message[3][2] if len(message[3]) > 2 else ()
        for message in calls
        if message[2] == "register"
    ]


def ids(span):
    context = span.get_span_context()
    return format(context.trace_id, "032x"), format(context.span_id, "016x")


def test_the_server_spans_of_a_load_and_a_commit_have_the_client_span_as_parent(
    tmp_path,
):
    from opentelemetry.sdk.trace import TracerProvider

    tracer = TracerProvider().get_tracer("acceptance")
    collector = Collector()
    env = {
        "ZOE_OTLP_ENDPOINT": collector.url,
        "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
    }
    try:
        with ZoeServer(str(tmp_path / "store"), env=env) as server:
            writer = native(server.address)
            try:
                with tracer.start_as_current_span("plone-commit") as span:
                    oid = commit(writer, record("x"))
                    commit_ids = ids(span)
            finally:
                writer.close()
            # A new client has an empty cache, so its load reaches the server.
            reader = native(server.address)
            try:
                with tracer.start_as_current_span("plone-request") as span:
                    reader.load(oid)
                    load_ids = ids(span)
            finally:
                reader.close()
            # The server flushes its spans when it stops.
            assert server.stop() == 0
        spans = collector.spans()
    finally:
        collector.close()

    def children(name, trace_id, span_id):
        return [
            span
            for span in spans
            if span["name"] == name
            and span["traceId"].lower() == trace_id
            and span.get("parentSpanId", "").lower() == span_id
        ]

    assert len(children("loadBefore", *load_ids)) == 1, spans
    assert children("vote", *commit_ids), spans
    assert children("tpc_finish", *commit_ids), spans


def test_a_call_outside_any_span_carries_no_traceparent(tmp_path):
    from opentelemetry.sdk.trace import TracerProvider

    tracer = TracerProvider().get_tracer("acceptance")
    with ZoeServer(str(tmp_path / "store")) as server:
        writer = native(server.address)
        try:
            inside = commit(writer, record("inside"))
            outside = commit(writer, record("outside"))
        finally:
            writer.close()

        with Wire(server.address) as wire:
            storage = native(wire.address)
            try:
                with tracer.start_as_current_span("plone-request") as span:
                    storage.load(inside)
                    trace_id, span_id = ids(span)
                storage.load(outside)
            finally:
                storage.close()
            calls = wire.kept()

    registrations = declared(calls)
    assert registrations, "the client did not register"
    assert all("trace_context" in names for names in registrations), registrations

    def load_of(oid):
        [message] = [m for m in calls if m[2] == "loadBefore" and m[3][0] == oid]
        return message

    traced = load_of(inside)
    assert len(traced) == 5, traced
    version, trace, parent, _flags = traced[4]["traceparent"].split("-")
    assert (version, trace, parent) == ("00", trace_id, span_id)
    assert len(load_of(outside)) == 4


def test_without_opentelemetry_the_client_declares_nothing(tmp_path):
    script = tmp_path / "without_opentelemetry.py"
    script.write_text(WITHOUT_OPENTELEMETRY)
    with ZoeServer(str(tmp_path / "store")) as server, Wire(server.address) as wire:
        host, port = wire.address
        done = subprocess.run(
            [sys.executable, str(script), host, str(port)],
            stdin=subprocess.DEVNULL,
            capture_output=True,
            text=True,
            timeout=TIMEOUT,
        )
        assert done.returncode == 0, done.stderr
        calls = wire.kept()

    registrations = declared(calls)
    assert registrations, "the client did not register"
    assert not any("trace_context" in names for names in registrations)
    assert all(len(message) == 4 for message in calls), [
        message for message in calls if len(message) != 4
    ]


def test_against_a_stock_zeo_server_the_client_declares_nothing_and_works(
    tmp_path,
):
    import ZEO
    from opentelemetry.sdk.trace import TracerProvider

    tracer = TracerProvider().get_tracer("acceptance")
    # zoe-client speaks the msgpack protocol M5. A ZEO server speaks it
    # when its configuration says so.
    address, stop = ZEO.server(
        path=str(tmp_path / "Data.fs"), zeo_conf={"msgpack": True}
    )
    try:
        with Wire(address) as wire:
            storage = native(wire.address)
            try:
                with tracer.start_as_current_span("plone-request"):
                    oid = commit(storage, record("stock"))
                    assert storage.load(oid)[0] == record("stock")
            finally:
                storage.close()
            calls = wire.kept()
    finally:
        stop()

    # The stock server refuses a third argument of register. The client
    # registers again with two arguments, and declares nothing.
    registrations = [m[3] for m in calls if m[2] == "register"]
    assert registrations and len(registrations[-1]) == 2, registrations
    assert all(len(message) == 4 for message in calls), [
        message for message in calls if len(message) != 4
    ]
