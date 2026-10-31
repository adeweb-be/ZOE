"""The helpers that the conformance runs of ZODB's and ZEO's suites share.

pytest puts the directory of each test module on `sys.path`, so a module of
this directory imports this module as `suite_support`.
"""

import contextlib
import doctest
import unittest

import zoe.testing


def defining_class(test):
    """Return the class that defines the method of test."""
    method = getattr(test, "_testMethodName", "runTest")
    for cls in type(test).__mro__:
        if method in vars(cls):
            return cls
    raise AssertionError(f"no class defines {test.id()}")


def name_of(test, strict=False):
    """Return the name of test in the lists of a run.

    A test method has the name of the class that defines it and its own
    name. A doctest has the end of its id, without a directory and without
    the part after its last ".". When no class defines the method, the
    name is the id of the test, or `strict` raises `AssertionError`.
    """
    if isinstance(test, doctest.DocTestCase):
        return test.id().rsplit("/", 1)[-1].rsplit(".", 1)[-1]
    if strict:
        return f"{defining_class(test).__name__}.{test._testMethodName}"
    method = getattr(test, "_testMethodName", "runTest")
    for cls in type(test).__mro__:
        if method in vars(cls):
            return f"{cls.__name__}.{method}"
    return test.id()


def class_of(test):
    """Return the name of the class of test."""
    return type(test).__name__


def cases_and_layers(suite, layers=()):
    """Yield each test of suite with the layers of the suites around it,
    the outer layer first."""
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


def run_under_zeo_layer(selected):
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
