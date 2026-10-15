"""Acceptance test of the storage `floor` of the harness.

`floor` is the floor of the phase 3 gate (ADR 0037): a storage in the
zodbshootout process, with no server. It keeps each record in memory and
gives back the same `bytes` object that it stored. It runs the two-phase
commit with no lock across transactions, and it checks the serial of each
store. Each process starts from the records of the populate: the floor
writes them to a file of the value directory when the populate closes its
database.

The harness fails a value whose zodbshootout log holds an error:
zodbshootout logs an exception of a benchmark and reports a time for it.
"""

import contextlib
import json
import pathlib
import subprocess
import sys
import textwrap
import threading

import jsonschema
import pytest
import transaction
import ZODB.config
from persistent.mapping import PersistentMapping
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import ConflictError
from ZODB.utils import maxtid, z64

import shootout
from shootout import GATE, KB, Cell, Profile, run
from shootout.storages import STORAGES, zconfig

SCHEMA_PATH = pathlib.Path(__file__).parents[2] / "schema" / "results.schema.json"
SCENARIOS = ("add", "update", "read_cold", "read_warm", "read_hot")
# The cells of the gate set, with one value each.
ONE_VALUE = Profile(cells=GATE.cells, processes=1, values=1)
# A thread that waits on a lock across transactions never ends.
TIMEOUT = 30


@pytest.fixture
def storage(tmp_path):
    """Yield the floor storage of a database of the harness."""
    with zconfig(STORAGES["floor"], tmp_path, None) as text:
        db = ZODB.config.databaseFromString(text)
        try:
            yield db.storage
        finally:
            db.close()


def commit(storage, records, tid=None):
    """Commit records, a dict of oid to (serial, data). Return the tid."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    for oid, (serial, data) in records.items():
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def test_floor_is_a_storage_of_the_harness():
    floor = STORAGES["floor"]
    assert floor.multi_process is True
    assert floor.needs_dsn is False


def test_each_scenario_runs_on_floor_in_each_cell_of_the_gate_set(
    monkeypatch, tmp_path
):
    monkeypatch.setattr(shootout, "GATE", ONE_VALUE)
    result = run("floor", gate=True, workdir=tmp_path)
    jsonschema.validate(result, json.loads(SCHEMA_PATH.read_text()))
    assert result["storage"] == "floor"
    cells = {
        (s["name"], s["mode"], s["concurrency"], s["object_size"])
        for s in result["scenarios"]
    }
    assert cells == {
        (name, cell.mode, cell.concurrency, cell.object_size)
        for name in SCENARIOS
        for cell in GATE.cells
    }
    for scenario in result["scenarios"]:
        assert scenario["samples"] == 1
        assert scenario["ops_per_second"] > 0
    # Each value ran on a fresh store, and nothing stays after the run.
    assert list(tmp_path.iterdir()) == []


def test_two_threads_hold_open_transactions_at_once_and_both_finish(storage):
    first = TransactionMetaData()
    storage.tpc_begin(first)
    oid = storage.new_oid()
    storage.store(oid, z64, b"first", "", first)
    tids = []

    def second():
        tids.append(commit(storage, {storage.new_oid(): (z64, b"second")}))

    thread = threading.Thread(target=second)
    thread.start()
    thread.join(TIMEOUT)
    assert not thread.is_alive(), "the second transaction waits for the first"
    storage.tpc_vote(first)
    tids.append(storage.tpc_finish(first))
    assert tids[0] < tids[1]
    assert storage.load(oid)[0] == b"first"


def test_a_store_with_a_stale_serial_raises_conflict_error(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"one")})
    commit(storage, {oid: (first, b"two")})
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    with pytest.raises(ConflictError):
        storage.store(oid, first, b"three", "", txn)
    storage.tpc_abort(txn)
    assert storage.load(oid)[0] == b"two"


def test_two_transactions_that_store_one_object_cannot_both_vote(storage):
    oid = storage.new_oid()
    serial = commit(storage, {oid: (z64, b"one")})
    first, second = TransactionMetaData(), TransactionMetaData()
    for txn, data in ((first, b"first"), (second, b"second")):
        storage.tpc_begin(txn)
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(first)
    with pytest.raises(ConflictError):
        storage.tpc_vote(second)
    storage.tpc_abort(second)
    storage.tpc_finish(first)
    assert storage.load(oid)[0] == b"first"


def test_an_older_snapshot_reads_the_revision_before(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"one")})
    second = commit(storage, {oid: (first, b"two")})
    assert storage.loadBefore(oid, second) == (b"one", first, second)
    assert storage.loadSerial(oid, first) == b"one"
    # The revision before the one before is gone.
    commit(storage, {oid: (second, b"three")})
    assert storage.loadBefore(oid, second) is None


def test_zap_all_removes_every_record(storage):
    oid = storage.new_oid()
    commit(storage, {oid: (z64, b"one")})
    assert len(storage) >= 1
    storage.zap_all()
    assert len(storage) == 0


def test_a_snapshot_that_holds_a_commit_has_its_invalidations(tmp_path):
    with zconfig(STORAGES["floor"], tmp_path, None) as text:
        db = ZODB.config.databaseFromString(text)
    try:
        with db.transaction() as connection:
            connection.root()["x"] = PersistentMapping(v=0)
            connection.root()["y"] = PersistentMapping(v=0)
        first, second = (
            transaction.TransactionManager(),
            transaction.TransactionManager(),
        )
        writer, reader = db.open(first), db.open(second)
        # The reader has x in its cache.
        assert reader.root()["x"]["v"] == 0
        second.commit()
        writer.root()["x"]["v"] = 1
        writer.root()["y"]["v"] = 1
        # The reader begins while the commit of the writer runs its callback.
        adapter = db._mvcc_storage
        invalidate = adapter._invalidate_finish

        def begin_then_invalidate(*args):
            second.begin()
            invalidate(*args)

        adapter._invalidate_finish = begin_then_invalidate
        first.commit()
        adapter._invalidate_finish = invalidate
        root = reader.root()
        # One snapshot: both old, or both new.
        assert (root["x"]["v"], root["y"]["v"]) in ((0, 0), (1, 1))
    finally:
        db.close()


def test_a_loaded_record_is_the_stored_bytes_object(storage):
    oid = storage.new_oid()
    data = bytes(100 * KB)
    tid = commit(storage, {oid: (z64, data)})
    loaded, serial, _ = storage.loadBefore(oid, maxtid)
    assert loaded is data
    assert serial == tid
    assert storage.load(oid)[0] is data


def test_a_worker_process_reads_the_records_of_the_populate(tmp_path):
    # The populate of zodbshootout, for two workers of 10 objects of 1 KB.
    populate = textwrap.dedent(
        """
        from zodbshootout._dbsupport import (
            BenchmarkDBFactory,
            get_databases_from_string,
        )
        from zodbshootout.speedtest import SpeedTestData

        [config] = get_databases_from_string(TEXT)
        factory = BenchmarkDBFactory(config, 10, 2)
        data = SpeedTestData(2, 10, 1024)
        """
    )
    # The worker reads the objects of worker 1, and updates them with
    # their serials.
    worker = textwrap.dedent(
        """
        import transaction

        db = factory()
        connection = db.open()
        objects = data.data_for_worker_id(connection.root(), 1)
        print(data.read_test_read_values(objects.values()))
        data.write_test_update_values(objects.values())
        transaction.commit()
        connection.close()
        db.close()
        """
    )
    with zconfig(STORAGES["floor"], tmp_path, None) as text:
        setup = populate.replace("TEXT", repr(text))
        populated = subprocess.run(
            [sys.executable, "-c", setup + "data.populate(factory, True)"],
            capture_output=True,
            text=True,
        )
        assert populated.returncode == 0, populated.stderr
        done = subprocess.run(
            [sys.executable, "-c", setup + worker], capture_output=True, text=True
        )
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip() == "10"


# A package that ZConfig imports in each process of zodbshootout. Its
# import makes the benchmark of read_hot raise.
FAILING = """\
import zodbshootout.speedtest


def bench_hot_read(self, loops, db_factory):
    raise RuntimeError("the benchmark raises")


bench_hot_read.inner_loops = True
zodbshootout.speedtest.SpeedTestWorker.bench_hot_read = bench_hot_read
"""


@pytest.mark.parametrize("name", ["filestorage", "floor"])
def test_a_value_whose_log_holds_an_error_fails(monkeypatch, tmp_path, name):
    package = tmp_path / "path" / "failing"
    package.mkdir(parents=True)
    (package / "__init__.py").write_text(FAILING)
    (package / "component.xml").write_text("<component>\n</component>\n")
    monkeypatch.setenv("PYTHONPATH", str(package.parent))
    real = shootout.zconfig

    def failing(*args):
        with real(*args) as text:
            yield "%import failing\n" + text

    monkeypatch.setattr(shootout, "zconfig", contextlib.contextmanager(failing))
    valuedir = tmp_path / "value"
    valuedir.mkdir()
    with pytest.raises(RuntimeError, match="error"):
        shootout._run_value(
            STORAGES[name], Cell("threads", 1, KB), "hot", valuedir, None
        )
    assert "the benchmark raises" in (valuedir / "zodbshootout.log").read_text()
