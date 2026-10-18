"""Unit tests of `zoe.testing`."""

import importlib
import os
import stat
import time

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
        (FILESTORAGE, {"pid_filename": "zeo.pid"}, "pid-filename"),
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


def ssl_zeo_config(ssl):
    return f"<zeo>\naddress 127.0.0.1:0\n<ssl>\n{ssl}\n</ssl>\n</zeo>"


@pytest.mark.parametrize(
    "ssl, name",
    [
        ("authenticate DYNAMIC", "authenticate DYNAMIC in <ssl>"),
        ("authenticate {here}", r"authenticate /\S+ in <ssl>"),
        ("authenticate {cert}\nciphers ALL", "ciphers ALL in <ssl>"),
        ("", "<ssl> section of <zeo> has no authenticate"),
    ],
)
def test_an_ssl_key_that_zoe_serve_lacks_fails_the_start_with_its_name(
    layer, ssl, name
):
    from ZEO.tests import testssl

    ssl = f"certificate {testssl.server_cert}\nkey {testssl.server_key}\n" + ssl
    ssl = ssl.format(cert=testssl.client_cert, here=testssl.here)
    with pytest.raises(ValueError, match=name):
        layer.start_zeo_server(FILESTORAGE, ssl_zeo_config(ssl))
    assert layer._servers == []


def test_the_password_function_decrypts_the_key_for_the_start_only(layer, tmp_path):
    from ZEO.tests import testssl

    function = "ZEO.tests.testssl.pwfunc"
    key = zoe.testing._decrypted_key(testssl.serverpw_key, function)
    assert os.path.dirname(key) == os.path.realpath(tmp_path)
    assert stat.S_IMODE(os.stat(key).st_mode) == 0o600
    with open(key) as file:
        assert file.read().startswith("-----BEGIN PRIVATE KEY-----")
    os.remove(key)

    ssl = (
        f"certificate {testssl.serverpw_cert}\nkey {testssl.serverpw_key}\n"
        f"authenticate {testssl.client_cert}\npassword-function {function}"
    )
    addr, stop = layer.start_zeo_server(FILESTORAGE, ssl_zeo_config(ssl))
    # The start removes the decrypted key: zoe serve reads it at its start.
    assert list(tmp_path.glob("zoe-tls-key*")) == []
    stop()


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
    from ZEO.tests import forker, testConfig

    monkeypatch.chdir(tmp_path)
    modules = forker, _forker, testConfig
    originals = tuple(module.start_zeo_server for module in modules)
    layer = zoe.testing.ZeoLayer("TEST_ZEO_LAYER")
    layer.setUp()
    try:
        for module in modules:
            assert module.start_zeo_server == layer.start_zeo_server
        servers = []
        for path in ("a.fs", "b.fs"):
            forker.start_zeo_server(path=path)
            servers.append(layer._servers[-1]._process)
    finally:
        layer.tearDown()
    assert all(process.returncode is not None for process in servers)
    assert layer._servers == []
    assert tuple(module.start_zeo_server for module in modules) == originals


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
    # The start killed the process and waited for it. Signal 0 checks that
    # the process exists and sends nothing.
    with pytest.raises(ProcessLookupError):
        os.kill(int(pid_file.read_text()), 0)


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


def test_the_transaction_timeout_closes_a_voted_transaction_in_the_log_file(
    layer, tmp_path
):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.Connection import TransactionMetaData

    addr, stop = layer.start_zeo_server(
        FILESTORAGE, {"transaction_timeout": 1}, log="server.log"
    )
    args = layer._servers[-1].args
    assert "--transaction-timeout=1" in args
    assert not any(arg.startswith("--idle-timeout") for arg in args)
    storage = ClientStorage(addr, wait_timeout=30)
    try:
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.tpc_vote(txn)
        # zoe serve writes its log lines to the file of the argument log.
        deadline = time.monotonic() + 30
        log = tmp_path / "server.log"
        while "transaction timeout after the vote" not in log.read_text():
            assert time.monotonic() < deadline, "the connection did not close"
            time.sleep(0.1)
        storage.tpc_abort(txn)
        # The client connects again without a voted transaction: the server
        # does not close it again.
        time.sleep(2)
        assert log.read_text().count("transaction timeout after the vote") == 1
    finally:
        storage.close()
    stop()


@pytest.mark.parametrize(
    "eventlog, name",
    [
        ("level DEBUG", "level DEBUG in <eventlog>"),
        ("<syslog>\n</syslog>", "<syslog> section of <eventlog>"),
        ("<logfile>\npath a.log\nformat x\n</logfile>", "format x in <logfile>"),
    ],
)
def test_an_eventlog_that_zoe_serve_lacks_fails_the_start_with_its_name(
    layer, eventlog, name
):
    storage_conf = f"<eventlog>\n{eventlog}\n</eventlog>\n{FILESTORAGE}"
    with pytest.raises(ValueError, match=name):
        layer.start_zeo_server(storage_conf, zeo_config())
    assert layer._servers == []


def test_a_read_only_server_on_a_missing_store_serves_a_new_store(layer):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.POSException import POSKeyError

    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config(read_only=True))
    storage = ClientStorage(addr, read_only=True, wait_timeout=30)
    try:
        with pytest.raises(POSKeyError):
            storage.load(bytes(8))
    finally:
        storage.close()
    stop()

    # A read-only storage section does not create its store.
    storage_conf = "<filestorage>\npath Data.fs\nread-only true\n</filestorage>"
    with pytest.raises(RuntimeError, match="stopped with"):
        layer.start_zeo_server(storage_conf, zeo_config(read_only=True))


def test_a_filestorage_file_serves_its_content_at_each_start(layer, tmp_path):
    from ZEO.ClientStorage import ClientStorage

    db = ZODB.DB("Data.fs")
    with db.transaction() as conn:
        conn.root()["value"] = "in the file"
    db.close()

    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config(), keep=True)
    db = ZODB.DB(ClientStorage(addr, wait_timeout=30))
    try:
        with db.transaction() as conn:
            assert conn.root()["value"] == "in the file"
            conn.root()["value"] = "in the store"
    finally:
        db.close()
    stop()
    assert (tmp_path / "Data.fs.zoe").is_dir()

    # The next start serves the file as it is, without the new commit.
    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config())
    db = ZODB.DB(ClientStorage(addr, wait_timeout=30))
    try:
        with db.transaction() as conn:
            assert conn.root()["value"] == "in the file"
    finally:
        db.close()
    stop()
    # Without keep, the stop removes the store and keeps the file.
    assert not (tmp_path / "Data.fs.zoe").exists()
    assert (tmp_path / "Data.fs").is_file()


def test_a_filestorage_file_with_a_blob_directory_fails_the_start(layer, tmp_path):
    (tmp_path / "Data.fs").write_bytes(b"FS30")
    storage_conf = "<filestorage>\npath Data.fs\nblob-dir blobs\n</filestorage>"
    with pytest.raises(ValueError, match="blob-dir .* with the FileStorage file"):
        layer.start_zeo_server(storage_conf, zeo_config())


def test_a_client_shares_the_blob_directory_of_the_section(layer, tmp_path):
    import ZODB.blob
    from ZEO.ClientStorage import ClientStorage

    storage_conf = (
        "<blobstorage>\nblob-dir blobs\n"
        "<filestorage>\npath Data.fs\n</filestorage>\n</blobstorage>\n"
    )
    addr, stop = layer.start_zeo_server(storage_conf, zeo_config())
    assert "--shared-blob-dir=true" in layer._servers[-1].args
    storage = ClientStorage(
        addr, blob_dir=str(tmp_path / "blobs"), shared_blob_dir=True, wait_timeout=30
    )
    db = ZODB.DB(storage)
    try:
        conn = db.open()
        conn.root()["blob"] = ZODB.blob.Blob(b"shared")
        transaction.commit()
        with conn.root()["blob"].open() as file:
            assert file.read() == b"shared"
    finally:
        db.close()
    stop()

    # Without a blob directory in a section, the server shares none.
    addr, stop = layer.start_zeo_server(FILESTORAGE, zeo_config())
    assert "--shared-blob-dir=true" not in layer._servers[-1].args
    stop()


def test_a_subclass_of_the_zeo_client_opens_it_after_the_native_layer(tmp_path):
    import ZEO.ClientStorage
    from ZEO.tests.ConnectionTests import TestClientStorage

    import zoe.zeo

    def open_client(server):
        return TestClientStorage(
            server.address,
            client="1",
            var=str(tmp_path),
            wait=1,
            wait_timeout=30,
            min_disconnect_poll=0.1,
        )

    bases = TestClientStorage.__bases__
    layer = zoe.testing.NATIVE_LAYER
    with zoe.testing.ZoeServer(str(tmp_path / "store")) as server:
        layer.setUp()
        try:
            storage = open_client(server)
            try:
                assert isinstance(storage, zoe.zeo._NativeClientStorage)
            finally:
                storage.close()
        finally:
            layer.tearDown()
        assert TestClientStorage.__bases__ == bases
        assert "__new__" not in vars(TestClientStorage)

        storage = open_client(server)
        try:
            assert type(storage).__mro__[1] is ZEO.ClientStorage.ClientStorage
            assert not isinstance(storage, zoe.zeo.ClientStorage)
        finally:
            storage.close()


def test_the_fork_server_that_a_zeo_test_starts_serves_a_later_test(
    tmp_path, monkeypatch
):
    import multiprocessing
    import shutil
    import tempfile

    def run_process():
        process = multiprocessing.get_context("forkserver").Process(target=os.getpid)
        process.start()
        process.join(30)
        assert process.exitcode == 0

    layer = zoe.testing.ZeoLayer("TEST_ZEO_LAYER")
    layer.setUp()
    try:
        # A ZEO test makes a test directory the temporary directory, then
        # removes it. The first process starts the fork server.
        directory = tmp_path / "test"
        directory.mkdir()
        monkeypatch.setattr(tempfile, "tempdir", str(directory))
        run_process()
        shutil.rmtree(directory)
        monkeypatch.undo()
        run_process()
    finally:
        layer.tearDown()
