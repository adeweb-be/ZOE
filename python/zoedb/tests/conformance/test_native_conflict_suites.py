"""Conformance: the conflict resolution tests of ZEO and ZODB pass with the
native client against `zoe serve`.

`just conformance` runs this module on each interpreter.

The run sets up two layers of `zoe.testing`. In `ZEO_LAYER`, ZEO's test
forker starts `zoe serve` (`test_zeo_harness.py` describes it). In
`NATIVE_LAYER`, `ZEO.ClientStorage.ClientStorage` is the native client, also
in the test modules that bound the class at their import.

- `ClientConflictResolutionTests` of `ZEO.tests.testZEO` runs unchanged:
  the tests of ZODB's `ConflictResolvingStorage` against a server that
  leaves the resolution to the client.
- In `FileStorageTests` of `ZEO.tests.testZEO`, the tests of
  `ConflictResolvingStorage` run, and the tests of
  `ConflictResolvingTransUndoStorage` are skipped for undo, as in
  `test_zeo_harness.py`.

No network thread of ZEO's own client starts during the run, so the tests
exercise the native client.
"""

import contextlib
import unittest

import zoe.testing

UNDO = "the storage does not undo: supportsUndo() is false"

CONFLICTS = ("ZODB.tests.ConflictResolution", "ConflictResolvingStorage")
UNDO_CONFLICTS = ("ZODB.tests.ConflictResolution", "ConflictResolvingTransUndoStorage")


def defining_class(test):
    """Return the class that defines the method of test."""
    method = test._testMethodName
    for cls in type(test).__mro__:
        if method in vars(cls):
            return cls
    raise AssertionError(f"no class defines {test.id()}")


def mixin_of(test):
    cls = defining_class(test)
    return cls.__module__, cls.__name__


def flatten(suite):
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            yield from flatten(item)
        else:
            yield item


def loaded(case):
    """Return the tests of case, with the prefixes `check` and `test`."""
    loader = unittest.TestLoader()
    suite = unittest.TestSuite()
    for prefix in ("check", "test"):
        loader.testMethodPrefix = prefix
        suite.addTests(loader.loadTestsFromTestCase(case))
    return list(flatten(suite))


@contextlib.contextmanager
def zeo_client_threads():
    """Record each network thread of ZEO's own client that starts."""
    from ZEO.asyncio.client import ClientThread

    started = []
    init = ClientThread.__init__

    def __init__(self, *args, **kw):
        started.append(type(self).__name__)
        init(self, *args, **kw)

    ClientThread.__init__ = __init__
    try:
        yield started
    finally:
        ClientThread.__init__ = init


def run(tests, skips):
    """Run tests inside the layers. Skip each test of skips with its reason."""
    result = unittest.TestResult()
    layers = [zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER]
    started = []
    try:
        for layer in layers:
            layer.setUp()
            started.append(layer)
        for test in tests:
            if test in skips:
                result.addSkip(test, skips[test])
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


def assert_passed(result):
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)


def test_client_conflict_resolution_tests_of_zeo_pass(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    tests = loaded(testZEO.ClientConflictResolutionTests)
    assert {mixin_of(test) for test in tests} == {CONFLICTS}
    with zeo_client_threads() as threads:
        result = run(tests, {})
    assert_passed(result)
    assert threads == [], "ZEO's own client ran"
    assert result.testsRun == len(tests)
    assert result.testsRun >= 5


def test_conflict_resolution_mixins_of_zodb_pass(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    tests = [
        test
        for test in loaded(testZEO.FileStorageTests)
        if mixin_of(test) in (CONFLICTS, UNDO_CONFLICTS)
    ]
    assert {mixin_of(test) for test in tests} == {CONFLICTS, UNDO_CONFLICTS}
    skips = {test: UNDO for test in tests if mixin_of(test) == UNDO_CONFLICTS}
    with zeo_client_threads() as threads:
        result = run(tests, skips)
    assert_passed(result)
    assert threads == [], "ZEO's own client ran"
    assert len(result.skipped) == len(skips) >= 2
    assert result.testsRun == len(tests) - len(skips)
    assert result.testsRun >= 5
