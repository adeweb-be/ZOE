"""Conformance: the test suites of ZEO pass against `zoe serve`.

This is the gate "ZEO tests green" of phase 2 (F2, F9, F13, F22, F27, N2).
`just conformance` runs it on each interpreter, under
`zoe.testing.ZEO_LAYER`: ZEO's test forker starts `zoe serve` in place of
a ZEO server (`test_zeo_harness.py` describes it).

The run collects the test suites of ZEO unchanged, from the modules in
`MODULES`, and runs every test. A test passes, or is skipped for undo
(`supportsUndo()` is false until phase 3), or is in `OUT` with its
reason, or skips itself, as ZEO's suites do on some platforms. `OUT`
holds the tests that drive ZEO's own server objects in the test process,
run `runzeo` or a ZEO 4 server, read the logs of ZEO's server, or run a
Python storage inside the server, which a server without Python cannot do.
It also holds the tests that call undo under a name without "undo". ZOE's
own acceptance tests cover the behaviour of most of them, as `OUT` says.

Before the card of this gate moves to Ready, the planner checks `OUT`
against a run of the harness. The builder never changes it: a test that
fails is a bug, and a test that cannot pass is a question for the human.
"""

import doctest
import importlib
import unittest

import zoe.testing

MODULES = [
    "ZEO.tests.testZEO",
    "ZEO.tests.testConnection",
    "ZEO.tests.testConfig",
    "ZEO.tests.testssl",
    "ZEO.tests.test_cache",
    "ZEO.tests.test_marshal",
    "ZEO.tests.test_sync",
    "ZEO.tests.testTransactionBuffer",
]

OWN_SERVER = "it drives ZEO's own server objects in the test process"

# The modules that the run does not collect, and why.
OUT_MODULES = {
    "ZEO.tests.testZEO2": OWN_SERVER,
    "ZEO.tests.testConversionSupport": OWN_SERVER,
    "ZEO.tests.test_client_credentials": (
        "it patches ZEOStorage.register of ZEO's server"
    ),
    "ZEO.tests.test_client_side_conflict_resolution": (
        OWN_SERVER + "; test_conflict_resolution_over_zeo.py covers the protocol"
    ),
    "ZEO.tests.testZEOServer": "it tests the ZEOServer class of runzeo",
    "ZEO.tests.testZEOOptions": "it tests the options of runzeo",
}

UNDO = "the storage does not undo: supportsUndo() is false"

# The tests of the collected modules that the run does not run, and why. A
# name is the class that defines the test and its method, or the name of a
# doctest. The name of a doctest file has "_" in place of its last ".".
OUT = {
    "protocols_test": (
        "it starts a ZEO 4 server; zoe serve speaks ZEO 6 msgpack only (ADR 0003)"
    ),
    "test_runzeo_msgpack_support": (
        "it expects runzeo to speak the pickle protocol Z5 by default; "
        "zoe serve speaks ZEO 6 msgpack only (ADR 0003)"
    ),
    "dynamic_server_ports_test": "it runs ZEO's threaded server in the test process",
    "zeo-fan-out_test": (
        "it serves a <zeoclient> storage: a ZEO client inside ZEO's server process"
    ),
    "multiple_storages_invalidation_queue_is_not_insane": (
        OWN_SERVER + "; test_named_storages.py covers it"
    ),
    "getInvalidationsAfterServerRestart": OWN_SERVER
    + "; test_invalidations.py covers it",
    "SyncTests.test_server_sync": (
        "it patches ping on ZEO's server object in the test process"
    ),
    "runzeo_without_configfile": "it runs runzeo",
    "runzeo_logrotate_on_sigusr2": "it runs runzeo",
    "client_labels": "it reads the log lines of ZEO's server",
    "TimeoutTests.checkTimeout": (
        "it reads the log lines of ZEO's server; "
        "TimeoutTests.checkTimeoutAfterVote covers the timeout"
    ),
    "FileStorageTests.checkInterfaceFromRemoteStorage": (
        "it expects the interfaces of the FileStorage of ZEO's server process; "
        "test_reads_over_zeo.py checks those of ZOE"
    ),
    "FileStorageLoadDelayedTests": (
        "it wraps the storage of the server in a Python storage"
    ),
    "RecoveryStorage.testRestoreAcrossPack": "it calls undo, and " + UNDO,
}


def name_of(test):
    if isinstance(test, doctest.DocTestCase):
        return test.id().rsplit("/", 1)[-1].rsplit(".", 1)[-1]
    method = getattr(test, "_testMethodName", "runTest")
    for cls in type(test).__mro__:
        if method in vars(cls):
            return f"{cls.__name__}.{method}"
    return test.id()


def class_of(test):
    return type(test).__name__


def is_undo(test):
    name = name_of(test)
    return "undo" in name.lower() or "Undo" in class_of(test)


def walk(suite, layers=()):
    layer = getattr(suite, "layer", None)
    if layer is not None and layer not in layers:
        layers = layers + (layer,)
    if isinstance(suite, unittest.TestSuite):
        for item in suite:
            yield from walk(item, layers)
    else:
        yield suite, layers


def suite_of(module_name):
    module = importlib.import_module(module_name)
    if hasattr(module, "test_suite"):
        return module.test_suite()
    return unittest.defaultTestLoader.loadTestsFromModule(module)


def run(module_name):
    result = unittest.TestResult()
    started, ran = [], 0
    try:
        for test, layers in walk(suite_of(module_name), (zoe.testing.ZEO_LAYER,)):
            name = name_of(test)
            if name in OUT or class_of(test) in OUT:
                continue
            if is_undo(test):
                result.addSkip(test, UNDO)
                continue
            for layer in layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
                test.run(result)
                ran += 1
            finally:
                for layer in reversed(layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    return result, ran


def test_no_module_is_left_out_without_a_reason():
    import pkgutil

    import ZEO.tests

    found = {
        f"ZEO.tests.{info.name}"
        for info in pkgutil.iter_modules(ZEO.tests.__path__)
        if info.name.startswith("test")
    }
    assert found <= set(MODULES) | set(OUT_MODULES), (
        found - set(MODULES) - set(OUT_MODULES)
    )


def test_the_suites_of_zeo_pass_against_zoe_serve():
    problems, total = [], 0
    for module_name in MODULES:
        result, ran = run(module_name)
        total += ran
        problems += [
            f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
        ]
        problems += [
            f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
        ]
    assert not problems, "\n\n".join(problems)
    assert total > 500, f"only {total} tests ran"
