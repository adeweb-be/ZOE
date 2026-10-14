"""Conformance: the race tests of ZODB pass on the native client.

`just conformance` runs this module on each interpreter.

Inside `zoe.testing.NATIVE_LAYER` and `zoe.testing.ZEO_LAYER`, the race
tests of `ZODB.tests.racetest.RaceTests`, a base of `BasicStorage`, run on
the native client against `zoe serve`:

- `test_race_loadopen_vs_local_invalidate`: one thread commits and one
  thread checks, through one client.
- `test_race_load_vs_external_invalidate`: eight workers, each on its own
  client, check and commit.

The client runs with its default settings, where several transactions of
one process run at once.

racetest sets the rounds of each check in the method itself: 500 rounds of
the local check, 8 workers of 100 rounds for the external check. A test
cannot change them without a change to ZODB. So the harness runs each test
ten times, each time on a new server and new clients: ten times the rounds
in all.

The test case is the one of `test_native_harness.py`: ZEO's
`FileStorageTests` with a `setUp` that opens the client with the address,
the storage name and `wait_timeout` only.
"""

import unittest

import zoe.testing

# The longest time that a client waits for its connection.
TIMEOUT = 30
# The race tests that the harness runs.
RACES = (
    "test_race_loadopen_vs_local_invalidate",
    "test_race_load_vs_external_invalidate",
)
# The runs of each race test.
REPEATS = 10


def native_case():
    """Return ZEO's `FileStorageTests` with the setUp of the harness.

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


def test_the_race_tests_pass_with_ten_times_their_rounds():
    from ZODB.tests.BasicStorage import BasicStorage
    from ZODB.tests.racetest import RaceTests

    # The tests are those of ZODB, and BasicStorage has them.
    assert issubclass(BasicStorage, RaceTests)
    for name in RACES:
        assert name in vars(RaceTests), name

    layers = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    case = native_case()
    result = unittest.TestResult()
    started = []
    try:
        for layer in layers:
            layer.setUp()
            started.append(layer)
        for name in RACES:
            for _ in range(REPEATS):
                for layer in layers:
                    if hasattr(layer, "testSetUp"):
                        layer.testSetUp()
                try:
                    case(name).run(result)
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
    assert not problems, "\n\n".join(problems)
    assert not result.skipped, result.skipped
    assert result.testsRun == len(RACES) * REPEATS
