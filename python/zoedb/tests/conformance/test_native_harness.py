"""Conformance harness of the native client: ZODB's storage mixins run on it.

`just conformance` runs this module on each interpreter.

`zoe.testing.NATIVE_LAYER` is a layer of zope.testrunner. Between its
`setUp` and its `tearDown`, `ZEO.ClientStorage.ClientStorage` opens a
`zoe.zeo.ClientStorage` with the native client inside. So `ZEO.client`,
`ZEO.DB` and the `<zeoclient>` section of ZODB open the native client too,
and no thread of ZEO's asyncio client starts. `tearDown` puts back the class
of ZEO.

The harness runs the tests that these mixins define: `BasicStorage`, the
race tests of ZODB (`RaceTests`, a base of `BasicStorage`),
`SynchronizedStorage`, `MTStorage` and `RevisionStorage`. They run inside
`NATIVE_LAYER` and `zoe.testing.ZEO_LAYER`, so the client is the native
client and the server is `zoe serve`. The undo tests are skipped, as in
`test_zeo_harness.py`.

The test case is ZEO's `FileStorageTests` with two changes:

- `setUp` opens the client with the address, the storage name and
  `wait_timeout` only. The `setUp` of ZEO also gives a blob cache directory
  and the options of reconnects, and the walking skeleton of the native
  client has neither.
- `_new_storage_client` opens a second client on the same address. The
  method of ZEO reads `_addr`, an attribute of ZEO's client.
"""

import threading
import unittest

import zoe.testing

# The longest time that a client waits for its connection.
TIMEOUT = 30
# The end of the name of the network thread of ZEO's asyncio client.
NETWORK_THREAD = "zeo client networking thread"

# The mixins whose tests this harness runs, by module and class name.
MIXINS = {
    ("ZODB.tests.BasicStorage", "BasicStorage"),
    ("ZODB.tests.racetest", "RaceTests"),
    ("ZODB.tests.Synchronization", "SynchronizedStorage"),
    ("ZODB.tests.MTStorage", "MTStorage"),
    ("ZODB.tests.RevisionStorage", "RevisionStorage"),
}

UNDO = "the storage does not undo: supportsUndo() is false"
RECONNECT = "it opens and closes many clients, a test of reconnects"

# The tests of these mixins that the harness does not run, and why.
SKIPS = {
    "RevisionStorage.checkLoadBeforeUndo": UNDO,
    "RevisionStorage.testLoadBeforeUndo": UNDO,
    "RaceTests.test_race_external_invalidate_vs_disconnect": RECONNECT,
}


def defining_class(test):
    """Return the class that defines the method of test."""
    method = test._testMethodName
    for cls in type(test).__mro__:
        if method in vars(cls):
            return cls
    raise AssertionError(f"no class defines {test.id()}")


def selected(suite):
    """Yield the tests of suite that one of MIXINS defines."""
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from selected(item)
            continue
        cls = defining_class(item)
        if (cls.__module__, cls.__name__) in MIXINS:
            yield item


def network_threads(port):
    """Return the names of the threads of ZEO's client for port."""
    return [
        thread.name
        for thread in threading.enumerate()
        if thread.name.endswith(NETWORK_THREAD) and str(port) in thread.name
    ]


def native_case():
    """Return ZEO's `FileStorageTests` with the setUp of this harness.

    The class is made here, not at the top of the module: pytest collects
    each `TestCase` class of a test module, and runs it without the layers.
    """
    import ZEO.ClientStorage
    from ZEO.tests import forker, testZEO
    from ZODB.tests import StorageTestBase

    class NativeFileStorageTests(testZEO.FileStorageTests):
        def setUp(self):
            # The base class makes a new temporary directory the current
            # directory.
            StorageTestBase.StorageTestBase.setUp(self)
            address, stop = forker.start_zeo_server(
                self.getConfig(), self.getZEOConfig()
            )
            self._servers = [stop]
            self._server_address = address
            self._storage = self._open_client()

        def _new_storage_client(self):
            return self._open_client()

        def _open_client(self):
            # The layer makes this class open the native client.
            client = ZEO.ClientStorage.ClientStorage(
                self._server_address, storage="1", wait_timeout=TIMEOUT
            )
            client.registerDB(testZEO.DummyDB())
            return client

    return NativeFileStorageTests


def run_in_layers(tests, layers):
    """Run tests inside layers. Return the result."""
    result = unittest.TestResult()
    started = []
    try:
        for layer in layers:
            layer.setUp()
            started.append(layer)
        for test in tests:
            name = f"{defining_class(test).__name__}.{test._testMethodName}"
            if name in SKIPS:
                result.addSkip(test, SKIPS[name])
                continue
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
    return result


def test_the_native_layer_swaps_the_client_class_and_restores_it(tmp_path):
    import ZEO
    import ZEO.ClientStorage
    from ZODB.config import storageFromString

    import zoe.zeo
    from zoe.testing import ZoeServer

    layer = zoe.testing.NATIVE_LAYER
    assert isinstance(layer.__name__, str)
    assert isinstance(layer.__bases__, tuple)
    original = ZEO.ClientStorage.ClientStorage

    with ZoeServer(str(tmp_path / "store")) as server:
        host, port = server.address
        openers = {
            "ZEO.ClientStorage.ClientStorage": lambda: ZEO.ClientStorage.ClientStorage(
                server.address, storage="1", wait_timeout=TIMEOUT
            ),
            "ZEO.client": lambda: ZEO.client(
                server.address, storage="1", wait_timeout=TIMEOUT
            ),
            "<zeoclient>": lambda: storageFromString(
                f"<zeoclient>\nserver {host}:{port}\nstorage 1\n</zeoclient>\n"
            ),
        }
        layer.setUp()
        try:
            assert ZEO.ClientStorage.ClientStorage is not original
            for how, open_client in openers.items():
                storage = open_client()
                try:
                    assert isinstance(storage, original), how
                    assert isinstance(storage, zoe.zeo.ClientStorage), how
                    assert not network_threads(port), how
                finally:
                    storage.close()
        finally:
            layer.tearDown()
    assert ZEO.ClientStorage.ClientStorage is original


def test_the_storage_mixins_pass_on_the_native_client():
    layers = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    case = native_case()
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for prefix in ("check", "test"):
        loader.testMethodPrefix = prefix
        suite.addTests(loader.loadTestsFromTestCase(case))
    tests = list(selected(suite))
    defined = {
        (defining_class(t).__module__, defining_class(t).__name__) for t in tests
    }
    assert defined == MIXINS, f"the selection misses {MIXINS - defined}"

    result = run_in_layers(tests, layers)

    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    assert not problems, "\n\n".join(problems)
    assert result.testsRun + len(result.skipped) >= len(tests)
    assert result.testsRun > 20
