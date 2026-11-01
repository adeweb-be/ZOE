"""Acceptance test: a span of the client and its server span share a trace.

`zoe.zeo.ClientStorage` declares the extension `trace_context` when the
package `opentelemetry` is installed, and sends the traceparent of the
current span with each call. `zoe serve` exports its spans over OTLP. The
span of the server's `loadBefore` has the trace id of the span of the
client, and that span as its parent: a slow Plone request and the server
work that it caused appear in one trace.

The test runs a small OTLP/HTTP collector that takes the JSON encoding.
"""

import json
import threading
from http.server import BaseHTTPRequestHandler, HTTPServer

from support import TIMEOUT, ZERO, record

from zoe.testing import ZoeServer


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


def test_the_span_of_a_load_joins_the_trace_of_the_client(tmp_path):
    from opentelemetry.sdk.trace import TracerProvider
    from ZODB.Connection import TransactionMetaData

    import zoe.zeo

    tracer = TracerProvider().get_tracer("acceptance")
    collector = Collector()
    env = {
        "ZOE_OTLP_ENDPOINT": collector.url,
        "OTEL_EXPORTER_OTLP_PROTOCOL": "http/json",
    }
    try:
        with ZoeServer(str(tmp_path / "store"), env=env) as server:
            storage = zoe.zeo.ClientStorage(
                server.address, storage="1", wait_timeout=TIMEOUT
            )
            try:
                assert "trace_context" in storage._info["extensions"]
                txn = TransactionMetaData()
                storage.tpc_begin(txn)
                oid = storage.new_oid()
                storage.store(oid, ZERO, record("x"), "", txn)
                storage.tpc_vote(txn)
                storage.tpc_finish(txn)
                # The load must reach the server, not the cache.
                storage._cache.clear()
                with tracer.start_as_current_span("plone-request") as span:
                    storage.load(oid)
                    context = span.get_span_context()
            finally:
                storage.close()
            # The server flushes its spans when it stops.
            assert server.stop() == 0
        trace_id = format(context.trace_id, "032x")
        span_id = format(context.span_id, "016x")
        loads = [
            span
            for span in collector.spans()
            if span["name"] == "loadBefore" and span["traceId"].lower() == trace_id
        ]
        assert len(loads) == 1, collector.spans()
        assert loads[0]["parentSpanId"].lower() == span_id
    finally:
        collector.close()
