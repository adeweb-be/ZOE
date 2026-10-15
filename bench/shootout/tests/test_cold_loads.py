"""Unit tests of the check of read_cold and of the storages of phase 3."""

import http.server
import threading

import pyperf
import pytest

import shootout
from shootout import KB, Cell, disk_needed
from shootout.storages import (
    STORAGES,
    admin_address,
    empty_database,
    reset_database,
    server_loads,
)

METRICS = (
    "# HELP zoe_loads_total The loadBefore calls.\n"
    "# TYPE zoe_loads_total counter\n"
    'zoe_loads_total{storage="1"} 1500\n'
    'zoe_loads_total{storage="other name"} 100\n'
    'zoe_load_latency_seconds_count{storage="1"} 1600\n'
    'zoe_commits_total{storage="1"} 7\n'
)


@pytest.fixture
def admin():
    class Handler(http.server.BaseHTTPRequestHandler):
        def do_GET(self):
            body = METRICS.encode()
            self.send_response(200 if self.path == "/metrics" else 404)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *args):
            pass

    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield f"127.0.0.1:{server.server_address[1]}"
    finally:
        server.shutdown()
        server.server_close()


def test_the_loads_are_the_sum_of_zoe_loads_total(admin):
    assert server_loads(admin) == 1600


def test_the_admin_address_comes_from_the_output_of_zoe_serve(tmp_path):
    (tmp_path / "zoe-serve.out").write_text(
        "admin 127.0.0.1:4001\nlistening 127.0.0.1:4000\n"
    )
    assert admin_address(tmp_path) == "127.0.0.1:4001"
    (tmp_path / "zoe-serve.out").write_text("listening 127.0.0.1:4000\n")
    with pytest.raises(RuntimeError, match="admin"):
        admin_address(tmp_path)


def test_the_check_of_read_cold_is_for_zoe_native_only():
    for storage in STORAGES:
        if storage != "zoe-native":
            shootout.check_cold_loads(storage, objects_read=1600, server_loads=0)


def test_the_objects_of_a_run_count_the_warmup_and_the_value():
    metadata = {"name": "read 100 cold objects", "loops": 1, "inner_loops": 30}
    run = pyperf.Run(
        [0.5], warmups=[(1, 0.5)], metadata=metadata, collect_metadata=False
    )
    benchmark = pyperf.Benchmark([run])
    cell = Cell("processes", 16, KB)
    assert shootout._objects(benchmark, cell) == 16 * 100 * 2 * 30


def test_relstorage_hf_needs_only_the_room_of_the_logs():
    for cell in shootout.FULL.cells:
        assert disk_needed("relstorage-hf", cell) == disk_needed("relstorage", cell)


def test_zoe_native_needs_the_room_of_zoe_server():
    for cell in shootout.FULL.cells:
        assert disk_needed("zoe-native", cell) == disk_needed("zoe-server", cell)


def test_relstorage_hf_needs_a_dsn():
    with pytest.raises(ValueError, match="DSN"):
        empty_database(STORAGES["relstorage-hf"], None)
    with pytest.raises(ValueError, match="DSN"):
        reset_database(STORAGES["relstorage-hf"], None)


@pytest.mark.parametrize(
    "storage", ["filestorage", "zeo", "zoe", "zoe-server", "zoe-native"]
)
def test_only_relstorage_has_a_schema_to_reset(storage):
    # The other storages need no DSN, and nothing happens.
    reset_database(STORAGES[storage], None)
