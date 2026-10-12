"""Test support that runs the test suites of ZODB against `zoe.Storage`.

This module needs the test extra of ZODB.

- `STORAGE_LAYER` is a layer of zope.testrunner. Between its `testSetUp`
  and its `testTearDown`, the default storage of ZODB is a `zoe.Storage` in
  a new directory: `ZODB.DB(None)`, `ZODB.MappingStorage.DB()` and
  `ZODB.tests.util.DB()` open one. A test that names a storage class gets
  that class.
- `StorageTestCase` is the base class of the storage suites of ZODB.
- `ZoeServer` runs the `zoe` binary as `zoe serve`, for the tests of the
  server. `ZOE_BINARY` gives the path of the binary.
"""

import itertools
import os
import pathlib
import shutil
import signal
import subprocess
import tempfile

import transaction
import ZODB
import ZODB.MappingStorage
import ZODB.tests.util
from ZODB.tests import StorageTestBase

import zoe


def _default_db(*args, **kw):
    """Open a database on the default storage.

    `ZODB.MappingStorage.DB` runs this code in its module, where `ZODB` is
    the package of ZODB.
    """
    return ZODB.DB(None, *args, **kw)


class StorageLayer:
    """A layer that makes a `zoe.Storage` the default storage of ZODB.

    zope.testrunner names a layer by `__name__` and `__bases__`.
    """

    def __init__(self, name):
        self.__name__ = name
        self.__bases__ = ()
        self._directory = None
        self._names = None
        self._storages = []
        self._patches = []

    def __repr__(self):
        return f"<{type(self).__module__}.{self.__name__}>"

    def setUp(self):
        self._directory = tempfile.mkdtemp(prefix="zoe-layer-")
        self._names = itertools.count()

    def tearDown(self):
        # Abort the current transaction and remove the connections that the
        # tests left open from the transaction manager. Then close the
        # stores. A second close does nothing.
        transaction.abort()
        transaction.manager.clearSynchs()
        while self._storages:
            self._storages.pop().close()
        shutil.rmtree(self._directory, ignore_errors=True)
        self._directory = None

    def testSetUp(self):
        init = ZODB.DB.__init__
        layer = self

        def __init__(db, storage, *args, **kw):
            if storage is None:
                storage = layer.open_storage()
            init(db, storage, *args, **kw)

        self._patch(ZODB.DB, "__init__", __init__)
        # A module or a doctest can bind the function of ZODB, as
        # `from ZODB.tests.util import DB` does. So the function itself
        # gets the code of `_default_db`.
        self._patch(ZODB.MappingStorage.DB, "__code__", _default_db.__code__)

    def testTearDown(self):
        # A test can leave a connection open, and a later test can commit
        # through it. So the stores stay open until tearDown.
        while self._patches:
            target, name, value = self._patches.pop()
            setattr(target, name, value)

    def open_storage(self):
        """Open a `zoe.Storage` in a new directory of the layer."""
        path = os.path.join(self._directory, f"store-{next(self._names)}")
        storage = zoe.Storage(path)
        self._storages.append(storage)
        return storage

    def _patch(self, target, name, value):
        self._patches.append((target, name, getattr(target, name)))
        setattr(target, name, value)


STORAGE_LAYER = StorageLayer("STORAGE_LAYER")


class StorageTestCase(StorageTestBase.StorageTestBase):
    """The base class of the storage suites of ZODB.

    `setUp` opens a `zoe.Storage` in a new directory as `self._storage`.
    `open(read_only=False)` opens the same store again.
    """

    # The iterator gives the extension of a transaction as the store keeps
    # it.
    use_extension_bytes = True

    def setUp(self):
        # The base class makes a new temporary directory the current
        # directory.
        StorageTestBase.StorageTestBase.setUp(self)
        self._path = os.path.abspath("store")
        self.open()

    def open(self, read_only=False):
        self._storage = zoe.Storage(self._path, read_only=read_only)


class ZoeServer:
    """A `zoe serve` process for a test.

    It runs `zoe serve --path path --listen 127.0.0.1:0` with the extra
    arguments `args`, and without `--path` when `path` is None. Only the
    variables of `env` reach it as `ZOE_*`. As a context manager, it
    starts the server and kills it at the end.
    """

    def __init__(self, path=None, args=(), env=None):
        self.path = path
        self.args = list(args)
        self.env = dict(env or {})
        self.address = None
        self._process = None

    @staticmethod
    def _binary():
        """Return the path of the `zoe` binary.

        `ZOE_BINARY` gives it. Else the debug build of the repository. The
        console script `zoe` of the wheel has no server.
        """
        binary = os.environ.get("ZOE_BINARY")
        if binary:
            return binary
        root = pathlib.Path(__file__).resolve().parents[4]
        target = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
        return str(target / "debug" / "zoe")

    def start(self):
        """Start the server. Return once it accepts connections.

        A later start listens on the same port, so a client reconnects.
        """
        if self._process is not None:
            raise RuntimeError("the server runs")
        listen = "127.0.0.1:0"
        if self.address is not None:
            listen = f"{self.address[0]}:{self.address[1]}"
        command = [self._binary(), "serve"]
        if self.path is not None:
            command += ["--path", str(self.path)]
        command += ["--listen", listen, *self.args]
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        environ.update(self.env)
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
        )
        for line in process.stdout:
            if line.startswith("listening "):
                host, port = line.split()[1].rsplit(":", 1)
                self.address = (host.strip("[]"), int(port))
                self._process = process
                return self
        code = process.wait()
        raise RuntimeError(f"zoe serve stopped with {code} before it listened")

    def kill(self):
        """Kill the server with SIGKILL and wait for it."""
        process, self._process = self._process, None
        if process is not None:
            process.kill()
            process.wait()
            process.stdout.close()

    def stop(self, timeout=30):
        """Send SIGTERM, wait at most `timeout` seconds, return the exit code."""
        process, self._process = self._process, None
        if process is None:
            raise RuntimeError("the server does not run")
        process.send_signal(signal.SIGTERM)
        try:
            return process.wait(timeout)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()
            raise
        finally:
            process.stdout.close()

    def __enter__(self):
        return self.start()

    def __exit__(self, *exc_info):
        self.kill()
