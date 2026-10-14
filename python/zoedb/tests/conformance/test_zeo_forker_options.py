"""Conformance: the forker of `zoe.testing` maps the server options of ZEO's suites.

`just conformance` runs this module on each interpreter.

`zoe.testing.ZEO_LAYER` makes the test forker of ZEO start `zoe serve`
(`test_zeo_harness.py`). The tests of ZEO's suites that this module runs
fail today only because the forker does not map a part of ZEO's server
configuration to `zoe serve`:

- `ZEO.tests.testConfig` binds `start_zeo_server` at import, and
  `ZEO.tests.testssl` reaches that name through `ZEOConfigTestBase`. Under
  the layer, both start `zoe serve`.
- The `<ssl>` section of `<zeo>` gives the TLS options of `zoe serve`:
  `certificate` and `key` the certificate of the server, `authenticate`
  the CA certificates of the clients. `password-function` names a Python
  function: the test process calls it and decrypts the key.
- The blob directory of a storage section can be the shared blob
  directory of a client (`shared-blob-dir` of the client): the client then
  commits its blobs with `storeBlobShared`.
- A read-only server on a store that does not exist serves a new, empty
  store, as ZEO's FileStorage creates its file.
- The `log` argument of the forker names a file that gets the log lines of
  `zoe serve`.
- `transaction-timeout` gives a timeout: a transaction that stays open
  after its vote for longer than the timeout loses its connection, and
  `tpc_finish` raises `ClientDisconnected`.
- A `<filestorage>` whose path is a FileStorage file, which the test
  process wrote, serves the content of that file as it is when the server
  starts.

The tests of ZEO run unchanged, under `ZEO_LAYER` and their own layers. A
test passes; a skip counts as a failure.
"""

import ssl
import subprocess
import time
import unittest

import pytest

import zoe.testing

TIMEOUT = 30

# The tests of ZEO's suites that pass once the forker maps these options,
# by the function that gives their suite. Each id is `test.id()`.
CONFIG_TESTS = [
    "ZEO.tests.testConfig.ZEOConfigTest.test_default_zeo_config",
    "ZEO.tests.testConfig.ZEOConfigTest.test_client_variations",
    "ZEO.tests.testConfig.ZEOConfigTest.test_blob_cache_size_check",
]

SSL_TESTS = [
    "ZEO.tests.testssl.SSLConfigTest.test_ssl_pw",
]

CONNECTION_TESTS = [
    "ZEO.tests.testConnection.FileStorageConnectionTests.checkReadOnlyServer",
    "ZEO.tests.testConnection.FileStorageConnectionTests"
    ".checkReadOnlyFallbackReadOnlyServer",
    "ZEO.tests.testConnection.MappingStorageConnectionTests.checkReadOnlyServer",
    "ZEO.tests.testConnection.MappingStorageConnectionTests"
    ".checkReadOnlyFallbackReadOnlyServer",
    "ZEO.tests.testConnection.FileStorageTimeoutTests.checkTimeoutOnAbort",
    "ZEO.tests.testConnection.FileStorageTimeoutTests.checkTimeoutOnAbortNoLock",
    "ZEO.tests.testConnection.MappingStorageTimeoutTests.checkTimeoutOnAbort",
    "ZEO.tests.testConnection.MappingStorageTimeoutTests.checkTimeoutOnAbortNoLock",
]

ZEO_TESTS = [
    "ZEO.tests.testZEO.dont_log_poskeyerrors_on_server",
    "ZEO.tests.testZEO.client_has_newer_data_than_server",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkLoadBlob",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkStoreBlob",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkStoreBlob_wrong_partition",
]

# The blob suite of ZODB on a client that shares the blob directory of the
# server, as `ZEO.tests.testZEO` builds it. `blob_transaction.txt` also
# stores with a serial given as a str: ZOE-131 makes it pass.
SHARED_BLOB_TESTS = [
    "blob_connection_txt",
    "blob_importexport_txt",
    "ZODB.tests.testblob.commit_from_wrong_partition",
    "ZODB.tests.testblob.do_not_depend_on_cwd",
    "ZODB.tests.testblob.is_blob_record",
    "ZODB.tests.testblob.loadblob_tmpstore",
    "ZODB.tests.testblob.savepoint_cleanup",
    "ZODB.tests.testblob.savepoint_commits_without_invalidations_out_of_order",
    "ZODB.tests.testblob.savepoint_isolation",
    "ZODB.tests.testblob.test_blob_file_permissions",
]


def tests_and_layers(suite, layers=()):
    """Yield each test of suite with the layers of the suites around it."""
    layer = getattr(suite, "layer", None)
    if layer is not None and layer not in layers:
        layers = layers + (layer,)
    if isinstance(suite, unittest.TestSuite):
        for item in suite:
            yield from tests_and_layers(item, layers)
    else:
        yield suite, layers


def select(suite, ids):
    """Return the tests of suite with these ids, each with its layers."""
    found = [
        (test, layers) for test, layers in tests_and_layers(suite) if test.id() in ids
    ]
    assert sorted(test.id() for test, _ in found) == sorted(ids)
    return found


def run(selected):
    """Run the tests under ZEO_LAYER and their own layers. Fail on a problem."""
    result = unittest.TestResult()
    started = []
    try:
        for test, layers in selected:
            layers = (zoe.testing.ZEO_LAYER, *layers)
            for layer in layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
                test.run(result)
            finally:
                for layer in reversed(layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [f"{test.id()}: skipped: {why}" for test, why in result.skipped]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)
    assert result.testsRun == len(selected)


@pytest.fixture(autouse=True)
def in_tmp_path(tmp_path, monkeypatch):
    """Run each test in a new current directory."""
    monkeypatch.chdir(tmp_path)


@pytest.fixture
def layer():
    """Set up ZEO_LAYER, and tear it down."""
    zoe.testing.ZEO_LAYER.setUp()
    try:
        yield zoe.testing.ZEO_LAYER
    finally:
        zoe.testing.ZEO_LAYER.tearDown()


def client(addr, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(addr, storage="1", **kw)


def commit(storage, oid, serial, data):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def test_the_server_of_testconfig_is_zoe_serve(layer):
    from ZEO.tests import testConfig

    case = testConfig.ZEOConfigTest("test_default_zeo_config")
    case.setUp()
    try:
        addr, stop = case.start_server()
        storage = client(addr)
        try:
            # It is zoe serve: its store is a blob storage, and the
            # MappingStorage of a ZEO server is not.
            interfaces = storage._info["interfaces"]
            assert ("ZODB.interfaces", "IBlobStorage") in interfaces
        finally:
            storage.close()
        stop()
    finally:
        case.tearDown()


def test_the_tests_of_testconfig_and_the_password_of_testssl_pass():
    from ZEO.tests import testConfig, testssl

    run(
        select(testConfig.test_suite(), CONFIG_TESTS)
        + select(testssl.test_suite(), SSL_TESTS)
    )


def test_the_read_only_and_timeout_tests_of_testconnection_pass():
    from ZEO.tests import testConnection

    run(select(testConnection.test_suite(), CONNECTION_TESTS))


def test_the_log_shared_blob_and_filestorage_tests_of_testzeo_pass():
    from ZEO.tests import testZEO

    run(select(testZEO.test_suite(), ZEO_TESTS))


def test_the_blob_suite_of_a_shared_blob_directory_passes():
    from ZEO.tests import testZEO
    from ZODB.tests import testblob

    suite = testblob.storage_reusable_suite(
        "ClientStorageSharedBlobs", testZEO.create_storage_shared
    )
    run(select(suite, SHARED_BLOB_TESTS))


def openssl(*args, cwd):
    subprocess.run(["openssl", *args], cwd=cwd, check=True, capture_output=True)


def test_the_ssl_section_of_zeo_gives_tls(layer, tmp_path):
    """The server speaks TLS with the certificates of the section.

    The client certificate is a new one, signed by the key of ZEO's
    `client.pem`, which the section names in `authenticate`.
    """
    from ZEO.Exceptions import ClientDisconnected
    from ZEO.tests import forker, testssl

    openssl(
        "req", "-newkey", "rsa:2048", "-nodes", "-subj", "/CN=client",
        "-keyout", "client.key", "-out", "client.csr",
        cwd=tmp_path,
    )  # fmt: skip
    (tmp_path / "client.ext").write_text(
        "basicConstraints = critical, CA:FALSE\nextendedKeyUsage = clientAuth\n"
    )
    openssl(
        "x509", "-req", "-in", "client.csr", "-CA", testssl.client_cert,
        "-CAkey", testssl.client_key, "-CAcreateserial",
        "-CAserial", str(tmp_path / "ca.srl"), "-days", "2",
        "-extfile", "client.ext", "-out", "client.pem",
        cwd=tmp_path,
    )  # fmt: skip

    addr, stop = forker.start_zeo_server(
        "<mappingstorage>\n</mappingstorage>\n", testssl.server_config
    )

    # A client without TLS gets no connection.
    with pytest.raises(ClientDisconnected):
        client(addr, wait_timeout=1)

    context = ssl.create_default_context(
        ssl.Purpose.SERVER_AUTH, cafile=testssl.server_cert
    )
    context.check_hostname = False
    context.load_cert_chain(str(tmp_path / "client.pem"), str(tmp_path / "client.key"))
    storage = client(addr, ssl=context)
    try:
        oid = storage.new_oid()
        tid = commit(storage, oid, bytes(8), b"tls")
        assert storage.load(oid) == (b"tls", tid)
    finally:
        storage.close()
    stop()


def test_a_transaction_open_past_the_transaction_timeout_loses_its_connection(
    layer,
):
    from ZEO.Exceptions import ClientDisconnected
    from ZEO.tests import forker
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import POSKeyError

    addr, stop = forker.start_zeo_server(
        "<mappingstorage>\n</mappingstorage>\n",
        forker.ZEOConfig(("127.0.0.1", 0), transaction_timeout=1),
    )
    storage = client(addr, min_disconnect_poll=0.1, max_disconnect_poll=0.5)
    try:
        oid = storage.new_oid()
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.store(oid, bytes(8), b"late", "", txn)
        storage.tpc_vote(txn)
        time.sleep(3)
        with pytest.raises(ClientDisconnected):
            storage.tpc_finish(txn)
        storage.tpc_abort(txn)
        # The client connects again, and the transaction did not commit.
        deadline = time.monotonic() + TIMEOUT
        while not storage.is_connected():
            assert time.monotonic() < deadline, "the client did not reconnect"
            time.sleep(0.05)
        with pytest.raises(POSKeyError):
            storage.load(oid)
    finally:
        storage.close()
    stop()
