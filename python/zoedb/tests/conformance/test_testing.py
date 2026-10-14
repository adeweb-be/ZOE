"""Unit tests of `zoe.testing`."""

import importlib
import os

import pytest
import transaction
import ZODB
import ZODB.MappingStorage
from test_zodb_suites import FULL, walk
from ZODB.POSException import ReadOnlyError, StorageError
from ZODB.tests.MinPO import MinPO
from ZODB.tests.StorageTestBase import zodb_unpickle

import zoe
import zoe.testing


def test_a_store_stays_open_until_the_layer_tears_down():
    layer = zoe.testing.StorageLayer("TEST_LAYER")
    layer.setUp()
    directory = layer._directory
    try:
        layer.testSetUp()
        db = ZODB.DB(None)
        conn = db.open()
        layer.testTearDown()

        # A later test commits through the connection that a test left
        # open.
        layer.testSetUp()
        conn.root()["key"] = "value"
        transaction.commit()
        layer.testTearDown()
    finally:
        layer.tearDown()

    assert not os.path.exists(directory)
    with pytest.raises(StorageError):
        db.storage.lastTransaction()
    # The connection to the closed store does not take part in a later
    # commit.
    transaction.commit()


def test_each_database_of_a_test_has_a_new_store():
    layer = zoe.testing.StorageLayer("TEST_LAYER")
    layer.setUp()
    try:
        layer.testSetUp()
        try:
            dbs = [ZODB.DB(None), ZODB.DB(None)]
            names = {db.storage.getName() for db in dbs}
            assert len(names) == 2
            for db in dbs:
                db.close()
        finally:
            layer.testTearDown()
        real = os.path.realpath(layer._directory)
        assert all(name.startswith(real + os.sep) for name in names)
    finally:
        layer.tearDown()


def bindings(value):
    """Yield each name of the FULL modules and of their doctests bound to value."""
    for name in FULL:
        module = importlib.import_module(f"ZODB.tests.{name}")
        for key, item in vars(module).items():
            if item is value:
                yield f"{module.__name__}.{key}"
        if hasattr(module, "test_suite"):
            for test, _ in walk(module.test_suite()):
                globs = getattr(getattr(test, "_dt_test", None), "globs", {})
                for key, item in globs.items():
                    if item is value:
                        yield f"{test.id()}: {key}"


def test_no_module_of_full_keeps_the_function_of_zodb():
    original = ZODB.MappingStorage.DB
    names = list(bindings(original))
    # testBroken binds the function at import, and its doctests copy it.
    assert "ZODB.tests.testBroken.DB" in names
    assert "ZODB.tests.testBroken.test_integration: DB" in names
    layer = zoe.testing.STORAGE_LAYER
    layer.setUp()
    try:
        layer.testSetUp()
        try:
            db = original()
            assert isinstance(db.storage, zoe.Storage)
            db.close()
        finally:
            layer.testTearDown()
    finally:
        layer.tearDown()
    db = original()
    assert not isinstance(db.storage, zoe.Storage)
    db.close()


class TestStorageTestCase(zoe.testing.StorageTestCase):
    def test_open_opens_the_same_store_again(self):
        oid = self._storage.new_oid()
        self._dostore(oid, data=MinPO(7))
        self._storage.close()
        self.open(read_only=True)
        assert zodb_unpickle(self._storage.load(oid)[0]).value == 7
        with pytest.raises(ReadOnlyError):
            self._storage.new_oid()


FILESTORAGE = "<filestorage 1>\npath Data.fs\n</filestorage>\n"


def zeo_config(**options):
    from ZEO.tests import forker

    return forker.ZEOConfig(("127.0.0.1", 0), **options)


def commit(storage, data):
    """Store `data` as a new object of `storage`. Return its oid and tid."""
    from ZODB.Connection import TransactionMetaData

    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, bytes(8), data, "", txn)
    storage.tpc_vote(txn)
    return oid, storage.tpc_finish(txn)


@pytest.fixture
def layer(tmp_path, monkeypatch):
    """A `ZeoLayer` set up in a new current directory."""
    monkeypatch.chdir(tmp_path)
    layer = zoe.testing.ZeoLayer("TEST_ZEO_LAYER")
    layer.setUp()
    yield layer
    layer.tearDown()


@pytest.mark.parametrize(
    "storage_conf, zeo_options, name",
    [
        (FILESTORAGE, {"transaction_timeout": 5}, "transaction-timeout"),
        (FILESTORAGE, {"pid_filename": "zeo.pid"}, "pid-filename"),
        (FILESTORAGE, {"invalidation_age": 5}, "invalidation-age"),
        ("<filestorage>\npath Data.fs\nquota 9\n</filestorage>", {}, "quota"),
        ("<demostorage>\n</demostorage>", {}, "<demostorage>"),
    ],
)
def test_an_option_that_zoe_serve_lacks_fails_the_start_with_its_name(
    layer, storage_conf, zeo_options, name
):
    with pytest.raises(ValueError, match=name):
        layer.start_zeo_server(storage_conf, zeo_config(**zeo_options))
    assert layer._servers == []


def test_an_ssl_section_fails_the_start_with_its_name(layer):
    zeo_conf = "<zeo>\naddress 127.0.0.1:0\n<ssl>\ncertificate x\n</ssl>\n</zeo>"
    with pytest.raises(ValueError, match="<ssl>"):
        layer.start_zeo_server(FILESTORAGE, zeo_conf)


def test_the_store_has_the_name_and_the_blob_directory_of_the_section(layer, tmp_path):
    import ZODB.blob
    from ZEO.ClientStorage import ClientStorage

    storage_conf = (
        "<blobstorage 2>\nblob-dir blobs\n"
        "<filestorage>\npath Data.fs\n</filestorage>\n</blobstorage>\n"
    )
    addr, stop = layer.start_zeo_server(storage_conf, zeo_config(), keep=True)
    storage = ClientStorage(
        addr, storage="2", blob_dir=str(tmp_path / "cache"), wait_timeout=30
    )
    db = ZODB.DB(storage)
    try:
        conn = db.open()
        conn.root()["blob"] = ZODB.blob.Blob(b"blob data")
        transaction.commit()
    finally:
        db.close()
    stop()

    # The store is a ZOE store at the path of the section. Its blobs are
    # in the blob directory of the section.
    assert (tmp_path / "Data.fs").is_dir()
    (blob,) = (tmp_path / "blobs").rglob("*.blob")
    assert blob.read_bytes() == b"blob data"


def test_a_second_start_on_the_same_port_restarts_the_store(layer):
    from ZEO.ClientStorage import ClientStorage

    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config(), keep=True)
    storage = ClientStorage(addr, wait_timeout=30)
    oid, tid = commit(storage, b"kept")
    storage.close()
    stop()

    again, stop = layer.start_zeo_server(FILESTORAGE, port=addr[1])
    assert again == addr
    storage = ClientStorage(addr, wait_timeout=30)
    try:
        assert storage.load(oid) == (b"kept", tid)
    finally:
        storage.close()
    # Without keep, the stop removes the store.
    stop()
    assert not os.path.exists("Data.fs")


def test_a_mapping_storage_gets_a_new_store_that_the_stop_removes(layer, tmp_path):
    from ZEO.ClientStorage import ClientStorage

    addr, stop = layer.start_zeo_server(
        "<mappingstorage>\n</mappingstorage>\n", zeo_config(), keep=True
    )
    (store,) = tmp_path.glob("zoe-mapping-*")
    storage = ClientStorage(addr, wait_timeout=30)
    try:
        commit(storage, b"data")
    finally:
        storage.close()
    stop()
    assert not store.exists()


def test_zeo_options_become_options_of_zoe_serve(layer, tmp_path):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.POSException import ReadOnlyError

    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config(), keep=True)
    storage = ClientStorage(addr, wait_timeout=30)
    commit(storage, b"data")
    storage.close()
    stop()

    zeo_conf = zeo_config(
        read_only=True, invalidation_queue_size=7, client_conflict_resolution=True
    )
    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_conf)
    storage = ClientStorage(addr, read_only=True, wait_timeout=30)
    try:
        assert storage.isReadOnly()
        with pytest.raises(ReadOnlyError):
            commit(storage, b"refused")
    finally:
        storage.close()
    stop()


def test_teardown_kills_the_servers_and_restores_the_forker(tmp_path, monkeypatch):
    from ZEO import _forker
    from ZEO.tests import forker

    monkeypatch.chdir(tmp_path)
    originals = forker.start_zeo_server, _forker.start_zeo_server
    layer = zoe.testing.ZeoLayer("TEST_ZEO_LAYER")
    layer.setUp()
    try:
        assert forker.start_zeo_server == layer.start_zeo_server
        assert _forker.start_zeo_server == layer.start_zeo_server
        servers = []
        for path in ("a.fs", "b.fs"):
            forker.start_zeo_server(path=path)
            servers.append(layer._servers[-1]._process)
    finally:
        layer.tearDown()
    assert all(process.returncode is not None for process in servers)
    assert layer._servers == []
    assert (forker.start_zeo_server, _forker.start_zeo_server) == originals


def test_the_protocol_5_starts_and_another_fails_with_its_name(layer):
    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config(), protocol=b"5")
    stop()
    with pytest.raises(ValueError, match="protocol b'4'"):
        layer.start_zeo_server(FILESTORAGE, zeo_config(), protocol=b"4")


def test_an_unknown_argument_fails_the_start_with_its_name(layer):
    with pytest.raises(TypeError, match="colour"):
        layer.start_zeo_server(FILESTORAGE, zeo_config(), colour="blue")
    assert layer._servers == []


def test_the_keys_of_a_storage_section_have_any_case(layer, tmp_path):
    storage_conf = "<FileStorage>\nPATH Data.fs\nBlob-Dir blobs\n</FileStorage>\n"
    addr, stop = layer.start_zeo_server(storage_conf, zeo_config(), keep=True)
    stop()
    assert (tmp_path / "Data.fs").is_dir()


def test_a_server_that_does_not_listen_in_time_fails_the_start(
    layer, tmp_path, monkeypatch
):
    pid_file = tmp_path / "pid"
    binary = tmp_path / "silent"
    binary.write_text(f"#!/bin/sh\necho $$ > {pid_file}\nexec sleep 60\n")
    binary.chmod(0o755)
    monkeypatch.setenv("ZOE_BINARY", str(binary))
    with pytest.raises(RuntimeError, match="did not listen within 1 seconds"):
        layer.start_zeo_server(FILESTORAGE, zeo_config(), start_timeout=1)
    assert layer._servers == []
    # The start killed the process and waited for it.
    assert not os.path.exists(f"/proc/{pid_file.read_text().strip()}")


def test_teardown_restores_the_forker_when_a_kill_fails(tmp_path, monkeypatch):
    from ZEO.tests import forker

    class Broken:
        def kill(self):
            raise OSError("the kill fails")

    monkeypatch.chdir(tmp_path)
    original = forker.start_zeo_server
    layer = zoe.testing.ZeoLayer("TEST_ZEO_LAYER")
    layer.setUp()
    layer._servers.append(Broken())
    with pytest.raises(OSError, match="the kill fails"):
        layer.tearDown()
    assert forker.start_zeo_server is original
