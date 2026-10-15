"""Conformance: ZEO's tests of TLS and of unix domain sockets pass with the
native client against `zoe serve`.

`just conformance` runs this module on each interpreter.

The run sets up two layers of `zoe.testing`. In `ZEO_LAYER`, ZEO's test
forker starts `zoe serve`: the `<ssl>` section of `<zeo>` gives its TLS
options, and an address with a `/` is the path of a unix domain socket
(`test_zeo_harness.py` describes the layer). In `NATIVE_LAYER`,
`ZEO.ClientStorage.ClientStorage` is the native client. So the
`<clientstorage>` sections of `ZEO.tests.testssl` and `ZEO.connection` in
the doctest open the native client.

The run collects `ZEO.tests.testssl` unchanged and the doctest
`unix_domain_sockets` of `ZEO.tests.testZEO`, and runs them inside the two
layers and the layers of their suites. A test passes, or is in `OUT` with
its reason. `OUT` holds the tests that the answer to the question about
`ssl=` contexts excludes: ZEO's `<ssl>` section of `<clientstorage>`
gives the client an `ssl.SSLContext`, not the paths of its files. The
answer: the native client takes only the files of the `<ssl>` section of
`<zoeclient>`, and an `ssl` context fails the open. No network thread of
ZEO's own client starts during the run, so the tests exercise the native
client.
"""

import contextlib
import doctest
import unittest

import zoe.testing

# The tests that the run does not run, and why. A name is the class that
# defines the test and its method, or the name of a doctest. The answer to
# the question about `ssl=` contexts fills it.
OUT = {
    "SSLConfigTest.test_ssl_basic": (
        "its <clientstorage> gives the client an ssl context: the native "
        "client takes only the files of the <ssl> section of <zoeclient>"
    ),
    "SSLConfigTest.test_ssl_hostname_check": (
        "its <clientstorage> gives the client an ssl context: the native "
        "client takes only the files of the <ssl> section of <zoeclient>"
    ),
}

# The tests of ZEO that this run must hold.
EXPECTED = {
    "SSLConfigTest.test_ssl_basic",
    "SSLConfigTest.test_ssl_hostname_check",
    "SSLConfigTest.test_ssl_pw",
    "unix_domain_sockets",
}


def name_of(test):
    if isinstance(test, doctest.DocTestCase):
        return test.id().rsplit("/", 1)[-1].rsplit(".", 1)[-1]
    method = getattr(test, "_testMethodName", "runTest")
    for cls in type(test).__mro__:
        if method in vars(cls):
            return f"{cls.__name__}.{method}"
    return test.id()


def class_of(test):
    return type(test).__name__


def walk(suite, layers=()):
    """Yield each test of suite with the layers of the suites around it."""
    layer = getattr(suite, "layer", None)
    if layer is not None and layer not in layers:
        layers = layers + (layer,)
    if isinstance(suite, unittest.TestSuite):
        for item in suite:
            yield from walk(item, layers)
    else:
        yield suite, layers


def collected():
    """Return the tests of the run, each with its layers."""
    from ZEO.tests import testssl, testZEO

    base = (zoe.testing.ZEO_LAYER, zoe.testing.NATIVE_LAYER)
    tests = list(walk(testssl.test_suite(), base))
    tests += [
        (test, layers)
        for test, layers in walk(testZEO.test_suite(), base)
        if name_of(test) == "unix_domain_sockets"
    ]
    return tests


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


def run(tests):
    """Run tests inside their layers. Return the result and the names run."""
    result = unittest.TestResult()
    started, ran = [], []
    try:
        for test, layers in tests:
            name = name_of(test)
            if name in OUT or class_of(test) in OUT:
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
                ran.append(name)
            finally:
                for layer in reversed(layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    return result, ran


def test_the_tls_and_unix_socket_tests_of_zeo_pass_with_the_native_client(
    tmp_path, monkeypatch
):
    # The layers of ZEO's suites make their directories in the current one.
    monkeypatch.chdir(tmp_path)
    tests = collected()
    names = {name_of(test) for test, _ in tests}
    assert EXPECTED <= names, EXPECTED - names
    assert set(OUT) <= names | {class_of(test) for test, _ in tests}

    with zeo_client_threads() as threads:
        result, ran = run(tests)

    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)
    assert threads == [], "ZEO's own client ran"
    assert (EXPECTED - set(OUT)) <= set(ran)
