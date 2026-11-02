"""Conformance: the test suites of ZEO pass with the native client.

This is the gate of phase 3, "ZEO client tests green against the native
client" (F28). `just conformance` runs it on each interpreter.

The run collects the test suites of ZEO unchanged, from the modules of
`test_zeo_suites.py`, the gate of phase 2. The ZEO tests that the cards of
phase 3 name are in these modules (`NAMED_CLASSES`, `NAMED_DOCTESTS`). Each
test runs inside `zoe.testing.NATIVE_LAYER`, where
`ZEO.ClientStorage.ClientStorage`, and what ZEO's tests open through it, is
the native client. The run goes twice:

- against `zoe serve`: inside `zoe.testing.ZEO_LAYER` too, the test forker
  of ZEO starts `zoe serve` in place of a ZEO server. The `OUT` and the
  `OUT_MODULES` of `test_zeo_suites.py` hold the tests that `zoe serve`
  cannot pass.
- against a stock ZEO 6 server, which ZEO's own forker starts. zoe-client
  speaks the msgpack protocol M5, and a ZEO server speaks the pickle
  protocol Z5 unless its configuration sets `msgpack`. So this run sets
  the variable `ZEO_MSGPACK`, as the runs of ZEO's own tests with msgpack
  do, and the servers of the forker speak M5. Which protocols the native
  client speaks is a question of the card of its walking skeleton.

A test passes, or is in `OUT` or
`OUT_MODULES` of this module with its reason, or skips itself. They hold
the tests that read the inside of `ZEO.ClientStorage`, each with the ZOE
test that covers its behaviour. Before the card of this gate moves to
Ready, the planner checks `OUT` against a run of the native layer. The
builder never changes it: a test that fails is a bug, and a test that
cannot pass is a question for the human.

A test of `PREFETCH_OFF` runs with the prefetch of references of the
native client off, with its reason: it counts the entries of the client
cache, which the prefetch changes.

The gate also checks that the native inside is the default of
`zoe.zeo.ClientStorage` and of `<zoeclient>`.
"""

import contextlib
import os
import threading
import unittest

from suite_support import cases_and_layers, class_of, name_of

import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30

# The modules of test_zeo_suites.py that the run does not collect, and why.
OUT_MODULES = {
    "ZEO.tests.test_cache": (
        "it tests the class ZEO.cache.ClientCache, which the native client "
        "does not use; test_native_cache.py and test_native_persistent_cache.py "
        "cover the cache of the native client"
    ),
    "ZEO.tests.testTransactionBuffer": (
        "it tests the class ZEO.TransactionBuffer.TransactionBuffer, which the "
        "native client does not use; test_native_skeleton.py and "
        "test_native_concurrent.py cover its transactions"
    ),
}

# The tests of the collected modules that the run does not run, and why. A
# name is the class that defines the test and its method, or the name of a
# doctest. The name of a doctest file has "_" in place of its last ".". The
# planner fills it from a run inside the native layer.
TLS_FILES = (
    "it gives an ssl context, or the self-signed server.pem of ZEO's tests; "
    "the native client takes TLS only from the files of the <ssl> section, "
    "and its certificates must chain to a CA; "
    "test_native_transports.py covers TLS"
)
POOL = (
    "it assumes one connection for each client; the native client has a "
    "listener and a pool of connections that commit (ADR 0036); "
    "test_native_concurrent.py covers the pool"
)
ZOEC = (
    "it reads the .zec file of ZEO or uses ZEO.cache; the native client "
    "keeps its cache in a .zoec file of ZOE (ADR 0034); "
    "test_native_persistent_cache.py and "
    "test_native_persistent_cache_units.py cover it"
)
VERIFIED_CACHE = (
    "it loads from a cache file while no server is connected; the native "
    "client serves a cache file only after its verification against a "
    "server (ADR 0034); test_native_persistent_cache.py covers the file"
)
ASYNCIO = "it reads the asyncio inside of ZEO's client, which the native client has not"
OUT = {
    # 1. TLS from files only.
    "FileStorageSSLTests": TLS_FILES,
    "SSLConnectionTests": TLS_FILES,
    "quick_close_doesnt_kill_server": TLS_FILES,
    "SSLConfigTest.test_ssl_basic": TLS_FILES,
    "SSLConfigTest.test_ssl_hostname_check": TLS_FILES,
    # 2. The pool of connections.
    "CommitLockVoteTests.checkCommitLockVoteAbort": (
        POOL + "; it calls vote on the listener, which runs no transaction"
    ),
    "CommitLockVoteTests.checkCommitLockVoteClose": (
        POOL + "; it calls vote on the listener, which runs no transaction"
    ),
    "CommitLockVoteTests.checkCommitLockVoteFinish": (
        POOL + "; it calls vote on the listener, which runs no transaction"
    ),
    "test_ruok": POOL + "; it expects 'connections': 1",
    "test_server_status": POOL + "; it expects 'connections': 1",
    "TimeoutTests.checkTimeoutAfterVote": (
        POOL + "; a stock ZEO server closes only the connection of the "
        "transaction at its timeout, and the listener stays connected; "
        "the rest of the test passes on the native client"
    ),
    # 3. The .zoec file.
    "InvqTests.checkQuickVerificationWith2Clients": ZOEC,
    "client-config_test": ZOEC,
    "ZEOConfigTest.test_client_variations": ZOEC,
    "invalidate_client_cache_entry_on_server_commit_error": ZOEC,
    # 4. A cache file serves only after its verification.
    "ConnectionTests.checkBasicPersistence": VERIFIED_CACHE,
    "ConnectionTests.checkDisconnectedCacheWorks": VERIFIED_CACHE,
    "ConnectionTests.checkDisconnectedCacheFails": VERIFIED_CACHE,
    # 6. The asyncio inside of ZEO's client.
    "ConnectionTests.checkBadMessage1": (
        ASYNCIO + ": it writes through _server.client.protocol on "
        "_server.loop; frames.rs of zoe-server covers a bad frame, and "
        "test_native_reconnect.py the new connection"
    ),
    "ConnectionTests.checkBadMessage2": (
        ASYNCIO + ": it writes through _server.client.protocol on "
        "_server.loop; frames.rs of zoe-server covers a bad frame, and "
        "test_native_reconnect.py the new connection"
    ),
    "ZRPCConnectionTests.checkCatastrophicClientLoopFailure": (
        ASYNCIO + ": it stops _server.loop and joins _server.thread; "
        "runtime.rs of zoe-client covers the tasks of a connection"
    ),
    "ZRPCConnectionTests.checkConnectionInvalidationOnReconnect": (
        ASYNCIO + ": it calls connection_lost of _server.client.protocol; "
        "test_native_reconnect.py covers the invalidations at a reconnect"
    ),
    "tpc_finish_error": (
        ASYNCIO + ": it changes the TransactionBuffer of _check_trans; "
        "test_native_cache.py covers the cache at a commit"
    ),
    "Test_convenience_functions.test_ZEO_client_convenience": (
        ASYNCIO + ": it gives a mock of ZEO's client thread as "
        "_client_factory; test_native_harness.py opens the native client "
        "through ZEO.client"
    ),
    # The order of the imports of the gate.
    "test_runzeo_msgpack_support": (
        "ZEO.tests.testZEO defines it at its import, for the value of "
        "ZEO_MSGPACK then; the run against zoe serve imports the module "
        "first, without ZEO_MSGPACK, so the run against a stock server gets "
        "the Z5 version and a server that speaks M5; the OUT of "
        "test_zeo_suites.py holds it for zoe serve too"
    ),
    "client_asyncore_thread_has_name": (
        ASYNCIO + ": it expects the networking thread of ZEO; "
        "test_the_native_inside_is_the_default checks that it does not "
        "start, and test_native_skeleton.py the threads of the native client"
    ),
}

# The tests that the run against a stock ZEO server does not run, and why.
# The run against zoe serve runs them.
OUT_ZEO = {
    "MultiprocessingTests.test_work_with_multiprocessing": (
        "on 3.14 the start method of multiprocessing is forkserver; the run "
        "against zoe serve starts the fork server in this test, while "
        "tempfile.tempdir is a test directory that the test removes, so the "
        "second run finds no socket of the fork server; the run against "
        "zoe serve runs the test"
    ),
}

# The tests that run with the prefetch of references of the native client
# off, and why. The other tests run with it on, its default.
PREFETCH_OFF = {
    "drop_cache_rather_than_verify_txt": (
        "it counts the entries of the client cache after the open of a "
        "database, which loads the root, and expects the root alone; the "
        "native client also prefetches the objects that the root refers to; "
        "test_native_prefetch_references.py covers the prefetch"
    ),
}

# The variable that turns the prefetch of references off.
PREFETCH_TWIN = "ZOE_PREFETCH_REFERENCES"

# The classes of ZEO's tests that the cards of phase 3 name. The tests of
# FullGenericTests run in its subclasses.
NAMED_CLASSES = {
    "FullGenericTests",
    "BlobAdaptedFileStorageTests",
    "BlobWritableCacheTests",
    "FileStorageRecoveryTests",
    "FileStorageSSLTests",
    "ZRPCConnectionTests",
    "ClientConflictResolutionTests",
    "IterationTests",
}

# The doctests of ZEO that the cards of phase 3 name.
NAMED_DOCTESTS = {
    "zeo_blob_cache_test",
    "drop_cache_rather_than_verify_txt",
    "invalidation-age_txt",
    "unix_domain_sockets",
    "iterator_sane_after_reconnect",
}

# The thread of ZEO's asyncio client.
ZEO_THREAD = "zeo client networking thread"


def phase_2():
    """Return the module of the gate of phase 2, test_zeo_suites.py.

    It is in the directory of this module, and gives MODULES, OUT,
    OUT_MODULES and `suite_of`.
    """
    import test_zeo_suites

    return test_zeo_suites


def modules():
    return [name for name in phase_2().MODULES if name not in OUT_MODULES]


@contextlib.contextmanager
def prefetch_for(name):
    """Turn the prefetch of references off while the test `name` of
    PREFETCH_OFF runs. Do nothing for another test."""
    if name not in PREFETCH_OFF:
        yield
        return
    old = os.environ.get(PREFETCH_TWIN)
    os.environ[PREFETCH_TWIN] = "off"
    try:
        yield
    finally:
        if old is None:
            os.environ.pop(PREFETCH_TWIN, None)
        else:
            os.environ[PREFETCH_TWIN] = old


def run(module_name, layers, out):
    """Run the tests of a module inside layers, except those of out.

    Return the result and the number of tests that ran.
    """
    suites = phase_2()
    result = unittest.TestResult()
    started, ran = [], 0
    try:
        for test, test_layers in cases_and_layers(suites.suite_of(module_name), layers):
            name = name_of(test)
            if name in out or class_of(test) in out:
                continue
            for layer in test_layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in test_layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
                with prefetch_for(name):
                    test.run(result)
                ran += 1
            finally:
                for layer in reversed(test_layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    return result, ran


def run_all(layers, out, floor):
    """Run every collected module. Fail with the list of the problems, or
    when fewer than `floor` tests ran."""
    problems, total = [], 0
    for module_name in modules():
        result, ran = run(module_name, layers, out)
        total += ran
        problems += [
            f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
        ]
        problems += [
            f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
        ]
    assert not problems, "\n\n".join(problems)
    assert total >= floor, f"only {total} tests ran"


def test_the_named_tests_and_the_out_modules_are_in_the_suites():
    suites = phase_2()
    assert set(OUT_MODULES) <= set(suites.MODULES)

    classes, names = set(), set()
    for module_name in modules():
        for test, _ in cases_and_layers(suites.suite_of(module_name)):
            classes.update(cls.__name__ for cls in type(test).__mro__)
            names.add(name_of(test))
    assert NAMED_CLASSES <= classes, f"the run misses {NAMED_CLASSES - classes}"
    assert NAMED_DOCTESTS <= names, f"the run misses {NAMED_DOCTESTS - names}"
    assert set(OUT) <= names | classes, f"OUT names no test: {set(OUT) - names}"
    assert set(PREFETCH_OFF) <= names, (
        f"PREFETCH_OFF names no test: {set(PREFETCH_OFF) - names}"
    )


def test_the_native_inside_is_the_default(tmp_path):
    from ZODB.config import storageFromString

    import zoe.zeo

    before = set(threading.enumerate())
    opened = []
    with ZoeServer(str(tmp_path / "store")) as server:
        host, port = server.address
        try:
            opened.append(
                zoe.zeo.ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
            )
            opened.append(
                storageFromString(
                    f"""
                    %import zoe
                    <zoeclient>
                      server {host}:{port}
                      storage 1
                      wait-timeout {TIMEOUT}
                    </zoeclient>
                    """
                )
            )
            started = [
                thread.name
                for thread in threading.enumerate()
                if thread not in before and thread.name.endswith(ZEO_THREAD)
            ]
            assert started == [], started
        finally:
            for storage in opened:
                storage.close()


def test_the_suites_of_zeo_pass_with_the_native_client_against_zoe_serve():
    layers = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    run_all(layers, {**phase_2().OUT, **OUT}, 334)


def test_the_suites_of_zeo_pass_with_the_native_client_against_zeo(monkeypatch):
    # runzeo speaks M5 when ZEO_MSGPACK is set and its configuration does
    # not set msgpack.
    monkeypatch.setenv("ZEO_MSGPACK", "1")
    run_all((zoe.testing.NATIVE_LAYER,), {**OUT, **OUT_ZEO}, 423)
