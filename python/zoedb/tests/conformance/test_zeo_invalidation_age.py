"""Conformance: ZEO's doctest `invalidation-age.txt` passes against `zoe serve`.

`just conformance` runs this module on each interpreter.

The doctest starts a server with `invalidation_queue_size=5`, then again
with `invalidation_age=100`, and checks that a client with a persistent
cache does a quick verification after nine transactions. Under
`zoe.testing.ZEO_LAYER`, the forker gives `invalidation-age` to
`zoe serve --invalidation-age`.
"""

import unittest

from suite_support import cases_and_layers

import zoe.testing

ZEO_TESTS = [
    "invalidation-age_txt",
]


def test_the_doctest_invalidation_age_passes(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    selected = [
        (test, layers)
        for test, layers in cases_and_layers(testZEO.test_suite())
        if test.id() in ZEO_TESTS
    ]
    assert [test.id() for test, _ in selected] == ZEO_TESTS

    result = unittest.TestResult()
    started = []
    try:
        for test, layers in selected:
            layers = (zoe.testing.ZEO_LAYER, *layers)
            for layer in layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            test.run(result)
    finally:
        for layer in reversed(started):
            layer.tearDown()
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [f"{test.id()}: skipped: {why}" for test, why in result.skipped]
    assert not problems, "\n\n".join(problems)
    assert result.testsRun == 1
