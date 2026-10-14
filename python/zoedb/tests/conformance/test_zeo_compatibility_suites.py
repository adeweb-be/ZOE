r"""Conformance: tests of ZEO's suites that fail on zoe serve itself.

`just conformance` runs this module on each interpreter.

`zoe.testing.ZEO_LAYER` makes the test forker of ZEO start `zoe serve`
(`test_zeo_harness.py`). The tests of ZEO's suites that this module runs
need no change to the forker. They fail today because `zoe serve` answers
differently from ZEO's server:

- `ThreadTests` and `blob_transaction.txt` store with the serial
  `'\0' * 8`, a str. ZEO's server takes it; `zoe serve` fails the vote
  with `TypeError: an oid or a tid is not bytes`.
- `history(None, None)` raises `TypeError` for the size, where ZEO's
  server raises `POSKeyError` for the oid.
- The client sends `iterator_gc` without waiting, then `iterator_next` for
  the same iterator. The calls of one connection run concurrently on
  `zoe serve`, so `iterator_next` can run first and not raise `KeyError`.
- After a frame that is not a call, the server closes the connection, and
  the client does not connect again in time for its next transaction.

The tests of ZEO run unchanged, under `ZEO_LAYER` and their own layers. A
test passes; a skip counts as a failure. The tests that fail only now and
then run several times.
"""

import unittest

import zoe.testing

# Each id is `test.id()`.
ZEO_TESTS = [
    "ZEO.tests.testZEO.MappingStorageTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.MappingStorageTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.ZRPCConnectionTests.checkExceptionLogsAtError",
]

# The blob suite of ZODB on a client that does not share the blob directory
# of the server, as `ZEO.tests.testZEO` builds it.
NON_SHARED_BLOB_TESTS = [
    "blob_transaction_txt",
]

# These fail only now and then: each runs RUNS times.
ITERATOR_TESTS = [
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkIteratorGCStorageCommitting",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkIteratorGCStorageTPCAborting",
]

BAD_MESSAGE_TESTS = [
    "ZEO.tests.testConnection.FileStorageConnectionTests.checkBadMessage1",
    "ZEO.tests.testConnection.FileStorageConnectionTests.checkBadMessage2",
    "ZEO.tests.testConnection.MappingStorageConnectionTests.checkBadMessage1",
    "ZEO.tests.testConnection.MappingStorageConnectionTests.checkBadMessage2",
]

RUNS = 5


def cases_and_layers(suite, layers=()):
    """Yield each test of suite with the layers of the suites around it."""
    layer = getattr(suite, "layer", None)
    if layer is not None and layer not in layers:
        layers = layers + (layer,)
    if isinstance(suite, unittest.TestSuite):
        for item in suite:
            yield from cases_and_layers(item, layers)
    else:
        yield suite, layers


def select(suite, ids):
    """Return the tests of suite with these ids, each with its layers."""
    found = [
        (test, layers) for test, layers in cases_and_layers(suite) if test.id() in ids
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


def test_the_thread_and_history_tests_of_testzeo_pass(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    run(select(testZEO.test_suite(), ZEO_TESTS))


def test_the_blob_transactions_of_a_client_without_a_shared_blob_directory_pass(
    tmp_path, monkeypatch
):
    from ZEO.tests import testZEO
    from ZODB.tests import testblob

    monkeypatch.chdir(tmp_path)
    suite = testblob.storage_reusable_suite(
        "ClientStorageNonSharedBlobs", testZEO.ServerManagingClientStorage
    )
    run(select(suite, NON_SHARED_BLOB_TESTS))


def test_the_iterator_gc_tests_pass_each_time(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    run(select(testZEO.test_suite(), ITERATOR_TESTS) * RUNS)


def test_the_bad_message_tests_of_testconnection_pass_each_time(tmp_path, monkeypatch):
    from ZEO.tests import testConnection

    monkeypatch.chdir(tmp_path)
    run(select(testConnection.test_suite(), BAD_MESSAGE_TESTS) * RUNS)
