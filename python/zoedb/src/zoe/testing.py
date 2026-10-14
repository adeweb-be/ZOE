"""Test support that runs the test suites of ZODB against `zoe.Storage`.

This module needs the test extra of ZODB.

- `STORAGE_LAYER` is a layer of zope.testrunner. Between its `testSetUp`
  and its `testTearDown`, the default storage of ZODB is a `zoe.Storage` in
  a new directory: `ZODB.DB(None)`, `ZODB.MappingStorage.DB()` and
  `ZODB.tests.util.DB()` open one. A test that names a storage class gets
  that class.
- `StorageTestCase` is the base class of the storage suites of ZODB.
- `ZoeServer` runs the `zoe` binary as `zoe serve`, for the tests of the
  server. `ZoeServer.binary()` gives the path of the binary, from
  `ZOE_BINARY` or the debug build of the repository. The kernel kills the
  server when the thread that started it ends, so a test process that
  dies leaves no server. Linux only.
- `ZEO_LAYER` is a layer of zope.testrunner. Between its `setUp` and its
  `tearDown`, the test forker of ZEO starts `zoe serve` in place of a ZEO
  server. It needs ZEO.
"""

import ctypes
import io
import itertools
import json
import os
import pathlib
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading

import transaction
import ZConfig.datatypes
import ZConfig.schemaless
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

    The server never outlives the process that started it. The kernel
    sends SIGKILL to the server when its parent ends (`PR_SET_PDEATHSIG`
    of Linux). The signal follows the thread that started the server, not
    the process: a server started from a thread ends when that thread
    ends.
    """

    def __init__(self, path=None, args=(), env=None):
        self.path = path
        self.args = list(args)
        self.env = dict(env or {})
        self.address = None
        self._process = None

    @staticmethod
    def binary():
        """Return the path of the `zoe` binary that the tests run.

        `ZOE_BINARY` gives it. Else the debug build of the repository. The
        console script `zoe` of the wheel has no server.
        """
        binary = os.environ.get("ZOE_BINARY")
        if binary:
            return binary
        root = pathlib.Path(__file__).resolve().parents[4]
        target = pathlib.Path(os.environ.get("CARGO_TARGET_DIR", root / "target"))
        return str(target / "debug" / "zoe")

    def start(self, timeout=None):
        """Start the server. Return once it accepts connections.

        A later start listens on the same port, so a client reconnects.
        A server that does not listen within `timeout` seconds is killed,
        and the start fails.
        """
        if self._process is not None:
            raise RuntimeError("the server runs")
        listen = "127.0.0.1:0"
        if self.address is not None:
            listen = f"{self.address[0]}:{self.address[1]}"
        command = [self.binary(), "serve"]
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
            preexec_fn=_die_with_parent(),
        )
        # The kill closes stdout, so the read below ends.
        expired = threading.Event()

        def expire():
            expired.set()
            process.kill()

        timer = threading.Timer(timeout, expire) if timeout else None
        if timer is not None:
            timer.start()
        address = None
        try:
            for line in process.stdout:
                if line.startswith("listening "):
                    host, port = line.split()[1].rsplit(":", 1)
                    address = (host.strip("[]"), int(port))
                    break
        finally:
            if timer is not None:
                timer.cancel()
                timer.join()
        if address is not None and not expired.is_set():
            self.address = address
            self._process = process
            return self
        code = process.wait()
        process.stdout.close()
        if expired.is_set():
            raise RuntimeError(f"zoe serve did not listen within {timeout} seconds")
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


# The option of prctl(2) that sets the parent-death signal.
_PR_SET_PDEATHSIG = 1


def _die_with_parent():
    """Return the function that `subprocess` runs in the child.

    The function asks the kernel to send SIGKILL to the child when its
    parent ends. Then it checks that the parent is still the process that
    started the child. When the parent ended during the fork, the function
    fails, and the child does not run the server.

    The function runs between the fork and the exec, where other threads
    can hold locks. So this function looks up `prctl` before the fork, and
    the child does no other work.
    """
    prctl = ctypes.CDLL(None, use_errno=True).prctl
    prctl.argtypes = (ctypes.c_int, ctypes.c_ulong)
    prctl.restype = ctypes.c_int
    option, kill = _PR_SET_PDEATHSIG, int(signal.SIGKILL)
    parent = os.getpid()
    getppid = os.getppid

    def die_with_parent():
        if prctl(option, kill) != 0:
            raise OSError(ctypes.get_errno(), "prctl(PR_SET_PDEATHSIG) failed")
        if getppid() != parent:
            raise RuntimeError("the process that started zoe serve ended")

    return die_with_parent


def _no_option(what):
    """Return the error for an option of ZEO that `zoe serve` lacks."""
    return ValueError(f"zoe serve has no option for {what}")


def _address(value):
    """Return the address `(host, port)` of the ZEO key `address`.

    An empty host is 127.0.0.1. A host name becomes its IPv4 address,
    because `zoe serve --listen` takes an IP address.
    """
    host, colon, port = value.rpartition(":")
    if not colon or "/" in value:
        raise _no_option(f"address {value}: zoe serve listens on TCP only")
    host = host.strip("[]") or "127.0.0.1"
    try:
        socket.inet_pton(socket.AF_INET6 if ":" in host else socket.AF_INET, host)
    except OSError:
        host = socket.gethostbyname(host)
    return host, int(port)


def _zeo_options(section):
    """Return the address and the `zoe serve` arguments of a <zeo> section.

    Raise `ValueError` that names the first option that `zoe serve` lacks.
    """
    for sub in section.sections:
        raise _no_option(f"the <{sub.type}> section of <zeo>")
    address = None
    args = []
    for key, values in section.items():
        key, value = key.lower(), values[-1]
        if key == "address":
            address = _address(value)
        elif key == "invalidation-queue-size":
            args += ["--invalidation-queue-size", value]
        elif key == "client-conflict-resolution":
            # zoe serve has no such option. It resolves the simple
            # conflicts itself and sends the others to the client in the
            # vote reply. With true, a ZEO server resolves nothing. With
            # false, it resolves with its own classes and fails the vote
            # when it cannot. When the client and the server have the same
            # classes, as in ZEO's tests, the result is the same.
            ZConfig.datatypes.asBoolean(value)
        elif key == "msgpack" and ZConfig.datatypes.asBoolean(value):
            # zoe serve speaks only the msgpack protocol.
            pass
        elif key == "read-only":
            read_only = ZConfig.datatypes.asBoolean(value)
            args.append(f"--read-only={str(read_only).lower()}")
        else:
            raise _no_option(f"{key} {value}")
    if address is None:
        raise ValueError("the <zeo> section has no address")
    return address, args


class _Store:
    """The ZOE store of a storage section of ZEO's configuration."""

    def __init__(self, name, path, blob_dir=None, read_only=False, temporary=False):
        self.name = name
        self.path = path
        self.blob_dir = blob_dir
        self.read_only = read_only
        # A store in place of a mapping storage goes at each stop.
        self.temporary = temporary

    def toml(self):
        """Return the [storages.NAME] table of the store."""
        # A JSON string is a basic string of TOML.
        lines = [
            f"[storages.{json.dumps(self.name)}]",
            f"path = {json.dumps(self.path)}",
        ]
        if self.blob_dir is not None:
            lines.append(f"blob-dir = {json.dumps(self.blob_dir)}")
        if self.read_only:
            lines.append("read-only = true")
        return "\n".join(lines) + "\n"


def _store(section):
    """Return the `_Store` of a storage section.

    The store has the name of the section, or 1 for a section without a
    name, as ZEO does. A <filestorage> becomes a store at its path, a
    <mappingstorage> a store in a new directory. A <blobstorage> around
    either one gives its blob directory. Relative paths are relative to
    the current directory. Raise `ValueError` that names the first key or
    section that `zoe serve` lacks.
    """
    name = section.name or "1"
    blob_dir = None
    base = section
    if section.type == "blobstorage":
        for key, values in section.items():
            if key.lower() != "blob-dir":
                raise _no_option(f"{key} {values[-1]} in <blobstorage>")
            blob_dir = os.path.abspath(values[-1])
        if len(section.sections) != 1:
            raise ValueError("a <blobstorage> section holds one storage section")
        base = section.sections[0]
    if base.sections:
        raise _no_option(f"the <{base.sections[0].type}> section of <{base.type}>")
    if base.type == "mappingstorage":
        for key, values in base.items():
            raise _no_option(f"{key} {values[-1]} in <mappingstorage>")
        path = tempfile.mkdtemp(prefix="zoe-mapping-", dir=os.getcwd())
        return _Store(name, path, blob_dir, temporary=True)
    if base.type != "filestorage":
        raise _no_option(f"<{base.type}>: it serves <filestorage> or <mappingstorage>")
    path = None
    create = False
    read_only = False
    for key, values in base.items():
        key, value = key.lower(), values[-1]
        if key == "path":
            path = os.path.abspath(value)
        elif key == "blob-dir":
            blob_dir = os.path.abspath(value)
        elif key == "create":
            create = ZConfig.datatypes.asBoolean(value)
        elif key == "read-only":
            read_only = ZConfig.datatypes.asBoolean(value)
        else:
            raise _no_option(f"{key} {value} in <filestorage>")
    if path is None:
        raise ValueError("a <filestorage> section has no path")
    if create:
        # A FileStorage with create true starts empty.
        shutil.rmtree(path, ignore_errors=True)
    return _Store(name, path, blob_dir, read_only)


class ZeoLayer:
    """A layer that makes the test forker of ZEO start `zoe serve`.

    `setUp` replaces `start_zeo_server` in `ZEO.tests.forker` and in
    `ZEO._forker`. The `start_server` of `ZEO.tests.forker.setUp` calls
    the first one. `tearDown` kills every server that the layer started
    and puts back the functions of ZEO.

    A module that binds the function at import, as `ZEO.tests.testConfig`
    does with `from .forker import start_zeo_server`, keeps the server of
    ZEO.

    zope.testrunner names a layer by `__name__` and `__bases__`.
    """

    def __init__(self, name):
        self.__name__ = name
        self.__bases__ = ()
        self._servers = []
        self._patches = []

    def __repr__(self):
        return f"<{type(self).__module__}.{self.__name__}>"

    def setUp(self):
        from ZEO import _forker
        from ZEO.tests import forker

        for module in (forker, _forker):
            self._patches.append((module, module.start_zeo_server))
            module.start_zeo_server = self.start_zeo_server

    def tearDown(self):
        try:
            while self._servers:
                self._servers.pop().kill()
        finally:
            while self._patches:
                module, function = self._patches.pop()
                module.start_zeo_server = function

    def start_zeo_server(
        self,
        storage_conf=None,
        zeo_conf=None,
        port=None,
        keep=False,
        path="Data.fs",
        protocol=None,
        blob_dir=None,
        suicide=True,
        debug=False,
        threaded=False,
        start_timeout=33,
        name=None,
        log=None,
        show_config=False,
    ):
        """Start `zoe serve` as ZEO's `start_zeo_server` starts a server.

        The arguments are those of ZEO's function. `suicide`, `debug`,
        `threaded`, `name` and `log` control the process of a ZEO server.
        They do not apply to `zoe serve`, which logs to stderr. A server
        that does not listen within `start_timeout` seconds is killed, and
        the start fails. Return the address and a stop function. The stop
        function takes the argument `stop_timeout`. Without `keep`, it
        removes the stores of the server.
        """
        from ZEO._forker import ZEOConfig

        # ZEO gives the version of the protocol without its prefix.
        if protocol is not None and protocol not in (b"5", b"M5"):
            raise _no_option(f"the protocol {protocol!r}: it speaks only M5")
        if not storage_conf:
            storage_conf = f"<filestorage>\npath {path}\n</filestorage>"
        if blob_dir:
            storage_conf = (
                f"<blobstorage>\nblob-dir {blob_dir}\n{storage_conf}\n</blobstorage>"
            )
        if zeo_conf is None or isinstance(zeo_conf, dict):
            if port is None:
                port = 0
            address = ("127.0.0.1", port) if isinstance(port, int) else port
            config = ZEOConfig(address)
            if zeo_conf:
                config.__dict__.update(zeo_conf)
            zeo_conf = config
        text = f"{zeo_conf}\n\n{storage_conf}"
        if show_config:
            print(text)

        config = ZConfig.schemaless.loadConfigFile(io.StringIO(text))
        for key, values in config.items():
            raise _no_option(f"{key} {values[-1]}")
        zeo = [s for s in config.sections if s.type == "zeo"]
        if len(zeo) != 1:
            raise ValueError("the configuration needs one <zeo> section")
        address, args = _zeo_options(zeo[0])
        # zoe serve logs to stderr: an <eventlog> section does not apply.
        sections = [s for s in config.sections if s.type not in ("zeo", "eventlog")]
        if not sections:
            raise ValueError("the configuration has no storage section")
        stores = [_store(section) for section in sections]

        descriptor, file = tempfile.mkstemp(".toml", prefix="zoe-forker", dir=".")
        with os.fdopen(descriptor, "w") as out:
            out.write("".join(store.toml() for store in stores))
        server = ZoeServer(args=["--config", os.path.abspath(file), *args])
        server.address = address
        try:
            server.start(start_timeout)
        finally:
            # zoe serve reads the file before it listens.
            os.remove(file)
        self._servers.append(server)

        def stop(stop_timeout=99):
            if server in self._servers:
                self._servers.remove(server)
                code = server.stop(stop_timeout)
                if code:
                    print(f"WARNING zoe serve stopped with {code}", file=sys.stderr)
            for store in stores:
                if store.temporary or not keep:
                    shutil.rmtree(store.path, ignore_errors=True)

        return server.address, stop


ZEO_LAYER = ZeoLayer("ZEO_LAYER")
