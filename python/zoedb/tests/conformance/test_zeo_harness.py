"""Conformance harness: ZEO's own test suites run against `zoe serve`.

`just conformance` runs this module on each interpreter.

`zoe.testing.ZEO_LAYER` is a layer of zope.testrunner. Between its `setUp`
and its `tearDown`, the test forker of ZEO starts `zoe serve` in place of a
ZEO server: `ZEO.tests.forker.start_zeo_server`, `ZEO._forker.start_zeo_server`
and the `start_server` that `ZEO.tests.forker.setUp` gives to the doctests.

- The storage section of the test (`<filestorage NAME>`,
  `<mappingstorage NAME>`, either one inside `<blobstorage>`) becomes a new
  ZOE store in the current directory of the test, served under the same
  name, with the blob directory of the section.
- The options of the ZEO section that `zoe serve` has become its options:
  the address, `invalidation-queue-size`, `client-conflict-resolution`
  and the `<ssl>` section. An option that `zoe serve` does not have fails
  the start with an error that names it.
- `start_zeo_server` returns the address and a stop function, as ZEO's
  does. A second start with the same `port` restarts on that port.
- `tearDown` kills every server that the layer started, and restores the
  forker of ZEO.

This harness runs the tests that the storage mixins of `FileStorageTests`
define (BasicStorage, SynchronizedStorage, MTStorage, RevisionStorage) and
ZEO's own `MiscZEOTests`, collected unchanged. The gate of phase 2 runs the
whole suite (`test_zeo_suites.py`).
"""

import unittest

import zoe.testing

# The mixins whose tests this harness runs, by module and class name.
MIXINS = {
    ("ZODB.tests.BasicStorage", "BasicStorage"),
    ("ZODB.tests.Synchronization", "SynchronizedStorage"),
    ("ZODB.tests.MTStorage", "MTStorage"),
    ("ZODB.tests.RevisionStorage", "RevisionStorage"),
    ("ZEO.tests.testZEO", "MiscZEOTests"),
}

# The tests of these mixins that the harness does not run, and why.
SKIPS = {}


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


def test_the_forker_of_zeo_starts_zoe_serve():
    from ZEO.ClientStorage import ClientStorage
    from ZEO.tests import forker
    from ZODB.Connection import TransactionMetaData

    original = forker.start_zeo_server
    layer = zoe.testing.ZEO_LAYER
    assert isinstance(layer.__name__, str)
    assert isinstance(layer.__bases__, tuple)

    layer.setUp()
    try:
        assert forker.start_zeo_server is not original
        addr, stop = forker.start_zeo_server(
            "<filestorage 1>\npath Data.fs\n</filestorage>\n",
            forker.ZEOConfig(("127.0.0.1", 0)),
        )
        storage = ClientStorage(addr, wait_timeout=30)
        try:
            # It is zoe serve: the store has the format of ZOE.
            assert storage._info["supportsUndo"] is True
            oid = storage.new_oid()
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.store(oid, bytes(8), b"data", "", txn)
            storage.tpc_vote(txn)
            tid = storage.tpc_finish(txn)
            assert storage.load(oid) == (b"data", tid)
        finally:
            storage.close()
        stop()
    finally:
        layer.tearDown()
    assert forker.start_zeo_server is original


def test_the_storage_mixins_of_zeo_pass_against_zoe_serve():
    from ZEO.tests import testZEO

    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for prefix in ("check", "test"):
        loader.testMethodPrefix = prefix
        suite.addTests(loader.loadTestsFromTestCase(testZEO.FileStorageTests))
    tests = list(selected(suite))
    defined = {
        (defining_class(t).__module__, defining_class(t).__name__) for t in tests
    }
    assert defined == MIXINS, f"the selection misses {MIXINS - defined}"

    result = unittest.TestResult()
    layer = zoe.testing.ZEO_LAYER
    layer.setUp()
    try:
        for test in tests:
            name = f"{defining_class(test).__name__}.{test._testMethodName}"
            if name in SKIPS:
                result.addSkip(test, SKIPS[name])
                continue
            if hasattr(layer, "testSetUp"):
                layer.testSetUp()
            try:
                test.run(result)
            finally:
                if hasattr(layer, "testTearDown"):
                    layer.testTearDown()
    finally:
        layer.tearDown()

    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    assert not problems, "\n\n".join(problems)
    assert result.testsRun + len(result.skipped) >= len(tests)
    assert result.testsRun > 20
