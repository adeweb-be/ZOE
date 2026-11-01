"""Conformance: ZODB's doctest `IExternalGC.test` passes against `zoe serve`.

`just conformance` runs this module on each interpreter.

ZEO's suite runs the doctest on a `ClientStorage` whose server has a
`<filestorage>` with `pack-gc false`. Under `zoe.testing.ZEO_LAYER`, the
forker gives `pack-gc false` to the store (the key `pack-gc` of its
`[storages.NAME]` table). The doctest commits two objects, unlinks them,
packs, and finds them still there; then it deletes them with
`deleteObject`, packs, and finds them gone, blob included. It also checks
`len(storage)`: 3 objects before the deletion, and 1 after the pack. The
`length` that `zoe serve` sends after a commit and after a pack is the
object count of the store.
"""

import unittest

from suite_support import cases_and_layers

import zoe.testing

ZEO_TESTS = [
    "IExternalGC_test",
]


def test_the_doctest_iexternalgc_passes(tmp_path, monkeypatch):
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
