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

A test passes, or is skipped for undo (phase 4), or is in `OUT` or
`OUT_MODULES` of this module with its reason, or skips itself. They hold
the tests that read the inside of `ZEO.ClientStorage`, each with the ZOE
test that covers its behaviour. Before the card of this gate moves to
Ready, the planner checks `OUT` against a run of the native layer. The
builder never changes it: a test that fails is a bug, and a test that
cannot pass is a question for the human.

The gate also checks that the native inside is the default of
`zoe.zeo.ClientStorage` and of `<zoeclient>`, and that `zoe.zeo._NOT_BUILT`,
the list of the arguments of `ZEO.ClientStorage` whose feature the native
client does not have, is empty.
"""

import threading
import unittest

import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30
UNDO = "undo comes in phase 4"

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
OUT = {}

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
    OUT_MODULES and the code that walks a suite.
    """
    import test_zeo_suites

    return test_zeo_suites


def modules():
    return [name for name in phase_2().MODULES if name not in OUT_MODULES]


def run(module_name, layers, out):
    """Run the tests of a module inside layers, except those of out.

    Return the result and the number of tests that ran.
    """
    suites = phase_2()
    result = unittest.TestResult()
    started, ran = [], 0
    try:
        for test, test_layers in suites.walk(suites.suite_of(module_name), layers):
            name = suites.name_of(test)
            if name in out or suites.class_of(test) in out:
                continue
            if suites.is_undo(test):
                result.addSkip(test, UNDO)
                continue
            for layer in test_layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in test_layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
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


def run_all(layers, out):
    """Run every collected module. Fail with the list of the problems."""
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
    assert total > 450, f"only {total} tests ran"


def test_the_named_tests_and_the_out_modules_are_in_the_suites():
    suites = phase_2()
    assert set(OUT_MODULES) <= set(suites.MODULES)

    classes, names = set(), set()
    for module_name in modules():
        for test, _ in suites.walk(suites.suite_of(module_name)):
            classes.update(cls.__name__ for cls in type(test).__mro__)
            names.add(suites.name_of(test))
    assert NAMED_CLASSES <= classes, f"the run misses {NAMED_CLASSES - classes}"
    assert NAMED_DOCTESTS <= names, f"the run misses {NAMED_DOCTESTS - names}"
    assert set(OUT) <= names | classes, f"OUT names no test: {set(OUT) - names}"


def test_every_argument_of_zeo_client_storage_is_built():
    import zoe.zeo

    assert not list(zoe.zeo._NOT_BUILT), zoe.zeo._NOT_BUILT


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
    run_all(layers, {**phase_2().OUT, **OUT})


def test_the_suites_of_zeo_pass_with_the_native_client_against_zeo(monkeypatch):
    # runzeo speaks M5 when ZEO_MSGPACK is set and its configuration does
    # not set msgpack.
    monkeypatch.setenv("ZEO_MSGPACK", "1")
    run_all((zoe.testing.NATIVE_LAYER,), OUT)
