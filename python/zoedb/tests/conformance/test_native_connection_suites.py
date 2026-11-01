"""Conformance: ZODB's race test of a disconnect passes with the native
client against `zoe serve`.

`just conformance` runs this module on each interpreter.

The run sets up two layers of `zoe.testing`. In `ZEO_LAYER`, ZEO's test
forker starts `zoe serve` (`test_zeo_harness.py` describes the layer). In
`NATIVE_LAYER`, `ZEO.ClientStorage.ClientStorage` is the native client,
also in the test modules that bound the class at their import.

The run collects ZODB's race test `test_race_external_invalidate_vs_disconnect`
(`ZODB.tests.racetest`, through `BasicStorage`) in `FileStorageTests` of
`ZEO.tests.testZEO`, unchanged, and runs it inside the two layers. It must
pass. No network thread of ZEO's own client starts during the run, so the
test exercises the native client. ZEO's connection suites need the
persistent cache of the client; the gate of the native client runs them.
"""

import unittest

from suite_support import cases_and_layers, name_of, zeo_client_threads

import zoe.testing

RACE = "RaceTests.test_race_external_invalidate_vs_disconnect"


def race_suite():
    """Return the race test in `FileStorageTests`, with the prefix `test`."""
    from ZEO.tests import testZEO

    loader = unittest.TestLoader()
    loader.testMethodPrefix = "test"
    suite = loader.loadTestsFromTestCase(testZEO.FileStorageTests)
    return unittest.TestSuite(
        test for test, _ in cases_and_layers(suite) if name_of(test) == RACE
    )


def collected():
    """Return the tests of the run, each with its layers."""
    base = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    return list(cases_and_layers(race_suite(), base))


def run(tests):
    """Run tests inside their layers. Return the result and the names run."""
    result = unittest.TestResult()
    started, ran = [], []
    try:
        for test, layers in tests:
            for layer in layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
                test.run(result)
                ran.append(name_of(test))
            finally:
                for layer in reversed(layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    return result, ran


def test_the_race_of_a_disconnect_passes_with_the_native_client(tmp_path, monkeypatch):
    # The layers of ZEO's suites make their directories in the current one.
    monkeypatch.chdir(tmp_path)
    tests = collected()
    assert [name_of(test) for test, _ in tests] == [RACE]

    with zeo_client_threads() as threads:
        result, ran = run(tests)

    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [f"{test.id()}: skipped: {why}" for test, why in result.skipped]
    assert not problems, "\n\n".join(problems)
    assert threads == [], "ZEO's own client ran"
    assert ran == [RACE]
