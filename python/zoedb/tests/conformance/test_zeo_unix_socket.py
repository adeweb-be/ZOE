"""Conformance: ZEO's doctest `unix_domain_sockets` passes against `zoe serve`.

`just conformance` runs this module on each interpreter.

The doctest starts a server with `start_server(port='./sock')`. Under
`zoe.testing.ZEO_LAYER`, the forker gives a ZEO address that is a path to
`zoe serve --listen`, and returns that path as the address of the server.
The test then connects a stock client to the path and commits.
"""

import unittest

from suite_support import cases_and_layers

import zoe.testing

ZEO_TESTS = [
    "ZEO.tests.testZEO.unix_domain_sockets",
]


def test_the_doctest_unix_domain_sockets_passes(tmp_path, monkeypatch):
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
