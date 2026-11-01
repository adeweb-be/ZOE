"""Conformance: ZEO's blob tests pass with the native client.

`just conformance` runs this module on each interpreter. The run collects
the test suite of `ZEO.tests.testZEO` unchanged and runs its blob tests:

- the tests that `CommonBlobTests`, `BlobAdaptedFileStorageTests` and
  `BlobWritableCacheTests` define, in the classes `BlobAdaptedFileStorageTests`
  (a blob cache) and `BlobWritableCacheTests` (a blob directory that the
  client and the server share);
- the doctest `zeo_blob_cache.test`, which prunes a blob cache that many
  clients fill at once.

Both classes also inherit the tests of `FullGenericTests`. The run of the
rest of the storage API runs those (`test_native_storage_suites.py`).

The tests run inside two layers. In `zoe.testing.ZEO_LAYER`, the test
forker of ZEO starts `zoe serve` in place of a ZEO server. In
`zoe.testing.NATIVE_LAYER`, `ZEO.ClientStorage.ClientStorage`, and what
ZEO's tests open through it, is the native client. A test passes, or is
in `INSIDE` with its reason, or skips itself.
`INSIDE` holds the tests that read the inside of `ZEO.ClientStorage`,
which the native client does not have.
"""

import doctest
import unittest

from suite_support import cases_and_layers, defining_class, name_of

import zoe.testing

# The classes whose tests the run takes, and the classes that define them.
CLASSES = {"BlobAdaptedFileStorageTests", "BlobWritableCacheTests"}
DEFINED_BY = {
    "CommonBlobTests",
    "BlobAdaptedFileStorageTests",
    "BlobWritableCacheTests",
}
# The doctests that the run takes. The name of a doctest file has "_" in
# place of its last ".".
DOCTESTS = {"zeo_blob_cache_test"}

# The tests that read the inside of ZEO.ClientStorage, and why. A name is
# the class that defines the test and its method, or the name of a doctest.
# The planner fills it from a run inside the native layer.
INSIDE = {}


def chosen(test):
    if isinstance(test, doctest.DocTestCase):
        return name_of(test, strict=True) in DOCTESTS
    return (
        type(test).__name__ in CLASSES and defining_class(test).__name__ in DEFINED_BY
    )


def blob_tests(layers):
    """Return the blob tests of ZEO's suite, each with its layers."""
    from ZEO.tests import testZEO

    return [
        (test, test_layers)
        for test, test_layers in cases_and_layers(testZEO.test_suite(), layers)
        if chosen(test)
    ]


def run(tests):
    """Run tests, each inside its layers.

    Return the result, the number of tests that ran and the number of
    tests that `INSIDE` leaves out.
    """
    result = unittest.TestResult()
    started, ran, left_out = [], 0, 0
    try:
        for test, layers in tests:
            name = name_of(test, strict=True)
            if name in INSIDE:
                left_out += 1
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
    return result, ran, left_out


def test_the_blob_tests_of_zeo_pass_with_the_native_client():
    layers = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    tests = blob_tests(layers)

    # The selection finds the tests of both classes and the doctest.
    found = {type(test).__name__ for test, _ in tests}
    names = {name_of(test, strict=True) for test, _ in tests}
    assert found >= CLASSES, f"the run misses {CLASSES - found}"
    assert names >= DOCTESTS, f"the run misses {DOCTESTS - names}"
    assert set(INSIDE) <= names, f"INSIDE names no test: {set(INSIDE) - names}"

    result, ran, left_out = run(tests)
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)
    assert ran + len(result.skipped) + left_out >= len(tests)
    assert ran > 10, f"only {ran} tests ran"
