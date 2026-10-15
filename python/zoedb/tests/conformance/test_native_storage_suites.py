"""Conformance: ZEO's storage tests pass with the native client.

`just conformance` runs this module on each interpreter. The run collects
two classes of `ZEO.tests.testZEO` unchanged and runs their tests:

- the tests of `FullGenericTests`, through `FileStorageTests`, its class
  with a FileStorage on the server: the ZODB mixins (`BasicStorage`,
  `PackableStorage`, `SynchronizedStorage`, `MTStorage`, `ReadOnlyStorage`,
  `RevisionStorage`, `IteratorStorage`, `ConflictResolvingStorage`) and
  ZEO's (`CommitLockVoteTests`, `ThreadTests`, `MiscZEOTests`,
  `IterationTests`);
- the tests of `FileStorageRecoveryTests` (`RecoveryStorage`), which copy
  transactions between two clients.

The tests run inside two layers. In `zoe.testing.ZEO_LAYER`, the test
forker of ZEO starts `zoe serve` in place of a ZEO server. In
`zoe.testing.NATIVE_LAYER`, `ZEO.ClientStorage.ClientStorage`, and what
ZEO's tests open through it, is the native client. A test passes, or is
skipped for undo (`supportsUndo()` is false until phase 4), or is in `OUT`
or `INSIDE` with its reason, or skips itself. `OUT` holds the tests that
`zoe serve` cannot pass, as in the run of ZEO's suites against
`zoe serve` with a stock client. `INSIDE` holds the tests that read the
inside of `ZEO.ClientStorage`, which the native client does not have.
"""

import doctest
import unittest

import zoe.testing

UNDO = "the storage does not undo: supportsUndo() is false"

# The mixins that the run must cover, by module and class name.
MIXINS = {
    ("ZODB.tests.BasicStorage", "BasicStorage"),
    ("ZODB.tests.PackableStorage", "PackableStorage"),
    ("ZODB.tests.ReadOnlyStorage", "ReadOnlyStorage"),
    ("ZODB.tests.IteratorStorage", "IteratorStorage"),
    ("ZODB.tests.RevisionStorage", "RevisionStorage"),
    ("ZEO.tests.IterationTests", "IterationTests"),
    ("ZEO.tests.CommitLockTests", "CommitLockVoteTests"),
    ("ZEO.tests.ThreadTests", "ThreadTests"),
    ("ZEO.tests.testZEO", "MiscZEOTests"),
    ("ZODB.tests.RecoveryStorage", "RecoveryStorage"),
}

# The tests that zoe serve cannot pass, and why. A name is the class that
# defines the test and its method.
OUT = {
    "RecoveryStorage.testRestoreAcrossPack": "it calls undo, and " + UNDO,
}

SERVER_OBJECT = "it calls the asyncio client of ZEO.ClientStorage (_server)"
ITERATOR_IDS = (
    "it reads the iterator ids of ZEO.ClientStorage (_iterator_ids, "
    "_iterators) and calls _call; test_native_api.py checks iterator_gc"
)

# The tests that read the inside of ZEO.ClientStorage, and why.
INSIDE = {
    "BasicStorage.test_checkCurrentSerialInTransaction": (
        "GenericTests._do_store_in_separate_thread reads _connection_generation"
    ),
    "GenericTests.checkSortKey": "it reads _storage and _server_addr",
    "CommitLockVoteTests.checkCommitLockVoteFinish": SERVER_OBJECT,
    "CommitLockVoteTests.checkCommitLockVoteAbort": SERVER_OBJECT,
    "CommitLockVoteTests.checkCommitLockVoteClose": SERVER_OBJECT,
    "IterationTests.checkIteratorGCProtocol": SERVER_OBJECT,
    "IterationTests.checkIteratorExhaustionStorage": ITERATOR_IDS,
    "IterationTests.checkIteratorGCStorageCommitting": ITERATOR_IDS,
    "IterationTests.checkIteratorGCStorageTPCAborting": ITERATOR_IDS,
    "IterationTests.checkIteratorGCStorageDisconnect": (
        "it reads _iterator_ids after it calls notify_disconnected"
    ),
}


def defining_class(test):
    """Return the class that defines the method of test."""
    method = getattr(test, "_testMethodName", "runTest")
    for cls in type(test).__mro__:
        if method in vars(cls):
            return cls
    raise AssertionError(f"no class defines {test.id()}")


def name_of(test):
    if isinstance(test, doctest.DocTestCase):
        return test.id().rsplit("/", 1)[-1].rsplit(".", 1)[-1]
    return f"{defining_class(test).__name__}.{test._testMethodName}"


def is_undo(test):
    return "undo" in name_of(test).lower() or "Undo" in type(test).__name__


def tests_of(case):
    """Return the tests of case, with both prefixes of ZODB's mixins."""
    tests = []
    for prefix in ("check", "test"):
        loader = unittest.TestLoader()
        loader.testMethodPrefix = prefix
        tests += list(loader.loadTestsFromTestCase(case))
    return tests


def storage_tests():
    """Return the tests of FullGenericTests and of FileStorageRecoveryTests."""
    from ZEO.tests import testZEO

    generic = set(testZEO.FullGenericTests.__mro__)
    tests = [
        test
        for test in tests_of(testZEO.FileStorageTests)
        if defining_class(test) in generic
    ]
    return tests + tests_of(testZEO.FileStorageRecoveryTests)


def run(tests, layers):
    """Run tests inside layers.

    Return the result, the number of tests that ran and the number of
    tests that `OUT` and `INSIDE` leave out.
    """
    result = unittest.TestResult()
    ran, left_out = 0, 0
    started = []
    try:
        for layer in layers:
            layer.setUp()
            started.append(layer)
        for test in tests:
            name = name_of(test)
            if name in OUT or name in INSIDE:
                left_out += 1
                continue
            if is_undo(test):
                result.addSkip(test, UNDO)
                continue
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
    return result, ran, left_out


def test_the_storage_tests_of_zeo_pass_with_the_native_client():
    layers = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    tests = storage_tests()

    # The run covers each mixin, and each name of OUT and INSIDE is a test.
    defined = {
        (defining_class(t).__module__, defining_class(t).__name__) for t in tests
    }
    assert MIXINS <= defined, f"the run misses {MIXINS - defined}"
    names = {name_of(test) for test in tests}
    assert set(OUT) <= names, f"OUT names no test: {set(OUT) - names}"
    assert set(INSIDE) <= names, f"INSIDE names no test: {set(INSIDE) - names}"

    result, ran, left_out = run(tests, layers)
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)
    assert ran + len(result.skipped) + left_out >= len(tests)
    assert ran > 100, f"only {ran} tests ran"
