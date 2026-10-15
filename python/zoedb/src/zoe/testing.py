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
- `native_client(address, **kw)` opens a `zoe.zeo.ClientStorage` with the
  native client inside. It needs ZEO.
- `NATIVE_LAYER` is a layer of zope.testrunner. Between its `setUp` and its
  `tearDown`, `ZEO.ClientStorage.ClientStorage` and the `<zoeclient>`
  section open the native client. It needs ZEO.
"""

import ctypes
import importlib
import io
import itertools
import json
import math
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
    variables of `env` reach it as `ZOE_*`. The server writes its log
    lines to stderr. With `log`, they go to the end of the file `log`. As
    a context manager, it starts the server and kills it at the end.

    `address` is `(host, port)`, or the path of a unix domain socket as a
    string. An address that is set before the start is the `--listen` of
    the server.

    The server never outlives the process that started it. The kernel
    sends SIGKILL to the server when its parent ends (`PR_SET_PDEATHSIG`
    of Linux). The signal follows the thread that started the server, not
    the process: a server started from a thread ends when that thread
    ends.
    """

    def __init__(self, path=None, args=(), env=None, log=None):
        self.path = path
        self.args = list(args)
        self.env = dict(env or {})
        self.log = log
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

        A later start listens on the same address, so a client
        reconnects. A server that does not listen within `timeout` seconds
        is killed, and the start fails.
        """
        if self._process is not None:
            raise RuntimeError("the server runs")
        listen = "127.0.0.1:0"
        if isinstance(self.address, str):
            listen = self.address
        elif self.address is not None:
            listen = f"{self.address[0]}:{self.address[1]}"
        command = [self.binary(), "serve"]
        if self.path is not None:
            command += ["--path", str(self.path)]
        command += ["--listen", listen, *self.args]
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        environ.update(self.env)
        log = None if self.log is None else open(self.log, "a")
        try:
            process = subprocess.Popen(
                command,
                stdin=subprocess.DEVNULL,
                stdout=subprocess.PIPE,
                stderr=log,
                env=environ,
                text=True,
                preexec_fn=_die_with_parent(),
            )
        finally:
            # The server has its own descriptor of the file.
            if log is not None:
                log.close()
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
                    address = _listening(line.removeprefix("listening "))
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


def _listening(value):
    """Return the address of the line `listening ADDRESS` of `zoe serve`.

    An address with a `/` is the path of a unix domain socket, and stays a
    string. Another address becomes `(host, port)`.
    """
    value = value.rstrip("\n")
    if "/" in value:
        return value
    host, port = value.rsplit(":", 1)
    return host.strip("[]"), int(port)


def _address(value):
    """Return the address of the ZEO key `address`.

    A value with a `/` is the path of a unix domain socket, as for ZEO. It
    stays a string, and `zoe serve --listen` takes it as it is, so a
    relative path is relative to the current directory. Another value
    becomes `(host, port)`. An empty host is 127.0.0.1. A host name
    becomes its IPv4 address, because `zoe serve --listen` takes an IP
    address.
    """
    if "/" in value:
        return value
    host, colon, port = value.rpartition(":")
    if not colon:
        raise _no_option(f"address {value}: give HOST:PORT, or a path with a /")
    host = host.strip("[]") or "127.0.0.1"
    try:
        socket.inet_pton(socket.AF_INET6 if ":" in host else socket.AF_INET, host)
    except OSError:
        host = socket.gethostbyname(host)
    return host, int(port)


def _zeo_options(section):
    """Return the options of a <zeo> section.

    Return the address, the `zoe serve` arguments, true for a read-only
    server, and the `_Tls` of the <ssl> section or None. Raise
    `ValueError` that names the first key or section that `zoe serve`
    lacks.
    """
    tls = None
    for sub in section.sections:
        if sub.type != "ssl" or tls is not None:
            raise _no_option(f"the <{sub.type}> section of <zeo>")
        tls = _Tls(sub)
    address = None
    args = []
    read_only = False
    for key, values in section.items():
        key, value = key.lower(), values[-1]
        if key == "address":
            address = _address(value)
        elif key == "invalidation-queue-size":
            args += ["--invalidation-queue-size", value]
        elif key == "transaction-timeout":
            # zoe serve closes a connection that sends no frame for longer
            # than --idle-timeout, and aborts its open transaction. So a
            # transaction that stays open after its vote past the timeout
            # loses its connection, as with ZEO. The timeout also closes an
            # idle connection without a transaction.
            args.append(f"--idle-timeout={int(value)}")
        elif key == "invalidation-age":
            # ZEO takes a float. zoe serve takes whole seconds, so round up.
            # A negative age replays nothing in ZEO, as 0 does here.
            seconds = max(0, math.ceil(float(value)))
            args += ["--invalidation-age", str(seconds)]
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
    return address, args, read_only, tls


class _Tls:
    """The <ssl> section of <zeo>: the TLS options of `zoe serve`.

    `certificate` becomes `--tls-cert`, `key` `--tls-key`, and
    `authenticate`, a file of CA certificates, `--tls-client-ca`. Without
    `key`, the file of `certificate` holds the key, as for ZEO.
    `password-function` is the dotted name of a function that returns the
    password of the key. Raise `ValueError` that names a key that
    `zoe serve` lacks: `authenticate` as a directory or `DYNAMIC`, or
    another key.
    """

    def __init__(self, section):
        self.certificate = None
        self.key = None
        self.authenticate = None
        self.password_function = None
        for key, values in section.items():
            key, value = key.lower(), values[-1]
            if key == "certificate":
                self.certificate = os.path.abspath(value)
            elif key == "key":
                self.key = os.path.abspath(value)
            elif key == "authenticate":
                # zoe serve takes only a file of CA certificates.
                if value == "DYNAMIC" or os.path.isdir(value):
                    raise _no_option(f"authenticate {value} in <ssl>")
                self.authenticate = os.path.abspath(value)
            elif key == "password-function":
                self.password_function = value
            else:
                raise _no_option(f"{key} {value} in <ssl>")
        for key in ("certificate", "authenticate"):
            if getattr(self, key) is None:
                raise ValueError(f"the <ssl> section of <zeo> has no {key}")
        if self.key is None:
            self.key = self.certificate

    def server_args(self):
        """Return the `zoe serve` arguments, and the files that they name.

        zoe serve reads the files at its start only. With a password
        function, the test process calls the function, as a ZEO server
        does, and decrypts the key into a new file. The caller removes the
        new files after the start.
        """
        key, files = self.key, []
        if self.password_function is not None:
            key = _decrypted_key(self.key, self.password_function)
            files.append(key)
        args = ["--tls-cert", self.certificate, "--tls-key", key]
        args += ["--tls-client-ca", self.authenticate]
        return args, files


def _decrypted_key(path, password_function):
    """Write the key of the file `path`, decrypted, to a new file.

    `password_function` is the dotted name of a function that returns the
    password. The `openssl` command decrypts the key, because zoe serve
    reads only a key that is not encrypted. The new file is in the current
    directory, and only its owner reads it. Return its path.
    """
    module, _, name = password_function.rpartition(".")
    password = getattr(importlib.import_module(module), name)()
    if isinstance(password, str):
        password = password.encode()
    result = subprocess.run(
        ["openssl", "pkey", "-in", path, "-passin", "stdin"],
        input=bytes(password) + b"\n",
        capture_output=True,
        check=False,
    )
    if result.returncode:
        error = result.stderr.decode(errors="replace").strip()
        raise RuntimeError(f"openssl did not decrypt {path}: {error}")
    # mkstemp creates the file with the mode 0600.
    descriptor, key = tempfile.mkstemp(".pem", prefix="zoe-tls-key", dir=".")
    with os.fdopen(descriptor, "wb") as out:
        out.write(result.stdout)
    return os.path.abspath(key)


def _log_file(section):
    """Return the path of the log file of an <eventlog> section, or None.

    The key `path` of its <logfile> section names the file. zoe serve logs
    at the level INFO and above, so the key `level` must be INFO. Raise
    `ValueError` that names another key or section.
    """
    for key, values in section.items():
        key, value = key.lower(), values[-1]
        if key != "level" or value.lower() != "info":
            raise _no_option(f"{key} {value} in <eventlog>")
    path = None
    for index, sub in enumerate(section.sections):
        if sub.type != "logfile" or index:
            raise _no_option(f"the <{sub.type}> section of <eventlog>")
        for key, values in sub.items():
            if key.lower() != "path":
                raise _no_option(f"{key} {values[-1]} in <logfile>")
            path = os.path.abspath(values[-1])
    return path


class _Store:
    """The ZOE store of a storage section of ZEO's configuration."""

    def __init__(
        self,
        name,
        path,
        blob_dir=None,
        read_only=False,
        temporary=False,
        data_fs=None,
        pack_gc=True,
    ):
        self.name = name
        self.path = path
        self.blob_dir = blob_dir
        self.read_only = read_only
        # False packs the store without garbage collection.
        self.pack_gc = pack_gc
        # A store in place of a mapping storage goes at each stop.
        self.temporary = temporary
        # The FileStorage file that the store gets its content from.
        self.data_fs = data_fs

    def make_ready(self, read_only_server):
        """Make the store ready for `zoe serve`.

        A store of a FileStorage file gets the content of the file as it is
        now. A read-only server needs an existing store, so a store that
        does not exist becomes a new, empty store, as ZEO's FileStorage
        creates its file. A read-only section does not create its store,
        as for ZEO.
        """
        if self.data_fs is not None:
            _convert(self.data_fs, self.path)
        elif read_only_server and not self.read_only and not _is_store(self.path):
            zoe.Storage(self.path, blob_dir=self.blob_dir).close()

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
        if not self.pack_gc:
            lines.append("pack-gc = false")
        return "\n".join(lines) + "\n"


def _is_store(path):
    """Return true when the directory `path` exists and is not empty."""
    return os.path.isdir(path) and bool(os.listdir(path))


def _convert(data_fs, path):
    """Convert the FileStorage file `data_fs` into a new store in `path`.

    The store that is in `path` goes first. `zoe convert` does the work.
    """
    shutil.rmtree(path, ignore_errors=True)
    command = [ZoeServer.binary(), "convert", "--path", path, "--from-data-fs", data_fs]
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    result = subprocess.run(
        command,
        stdin=subprocess.DEVNULL,
        capture_output=True,
        text=True,
        env=environ,
        check=False,
    )
    if result.returncode:
        error = result.stderr.strip()
        raise RuntimeError(f"zoe convert stopped with {result.returncode}: {error}")


def _store(section):
    """Return the `_Store` of a storage section.

    The store has the name of the section, or 1 for a section without a
    name, as ZEO does. A <filestorage> becomes a store at its path, with its
    `pack-gc`, a <mappingstorage> a store in a new directory. A
    <filestorage> whose path is a FileStorage file becomes a store beside
    the file, at the path with the suffix `.zoe`. A <blobstorage> around
    either one gives its blob directory. Relative paths are relative to the
    current directory. Raise `ValueError` that names the first key or
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
    pack_gc = True
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
        elif key == "pack-gc":
            pack_gc = ZConfig.datatypes.asBoolean(value)
        else:
            raise _no_option(f"{key} {value} in <filestorage>")
    if path is None:
        raise ValueError("a <filestorage> section has no path")
    if create:
        # A FileStorage with create true starts empty.
        if os.path.isfile(path):
            os.remove(path)
        else:
            shutil.rmtree(path, ignore_errors=True)
    elif os.path.isfile(path):
        # The conversion copies no blob.
        if blob_dir is not None:
            raise _no_option(f"blob-dir {blob_dir} with the FileStorage file {path}")
        return _Store(
            name, path + ".zoe", read_only=read_only, data_fs=path, pack_gc=pack_gc
        )
    return _Store(name, path, blob_dir, read_only, pack_gc=pack_gc)


class ZeoLayer:
    """A layer that makes the test forker of ZEO start `zoe serve`.

    `setUp` replaces `start_zeo_server` in `ZEO._forker` and in each module
    of ZEO that binds it at import: `ZEO.tests.forker`, and
    `ZEO.tests.testConfig`, which `setUp` imports first. The `start_server`
    of `ZEO.tests.forker.setUp` calls the function of `ZEO.tests.forker`.
    `tearDown` kills every server that the layer started and puts back each
    function that `setUp` replaced.

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

        # The import binds the function of ZEO in testConfig before the
        # replacement.
        importlib.import_module("ZEO.tests.testConfig")
        function = _forker.start_zeo_server
        for name, module in list(sys.modules.items()):
            if name != "ZEO" and not name.startswith("ZEO."):
                continue
            if getattr(module, "__dict__", {}).get("start_zeo_server") is function:
                self._patches.append((module, function))
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
        `threaded` and `name` control the process of a ZEO server. They do
        not apply to `zoe serve`. As for ZEO, `log` adds an <eventlog>
        section when `zeo_conf` is None or a dict. A server that does not
        listen within `start_timeout` seconds is killed, and the start
        fails. Return the address and a stop function. The stop function
        takes the argument `stop_timeout`. Without `keep`, it removes the
        stores of the server.

        The <zeo> section gives the options of `zoe serve` (see
        `_zeo_options`), and each storage section a store (see `_store`).
        When a storage section has a blob directory, a client can share
        it: the server accepts `storeBlobShared` (`--shared-blob-dir`). A
        client that does not share it still uploads its blobs. The <logfile>
        of an <eventlog> section names the file that gets the log lines of
        `zoe serve`. Without it, they go to stderr.
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
            config = ZEOConfig(address, log=log)
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
        address, args, read_only, tls = _zeo_options(zeo[0])
        eventlog = [s for s in config.sections if s.type == "eventlog"]
        if len(eventlog) > 1:
            raise ValueError("the configuration has more than one <eventlog> section")
        log_file = _log_file(eventlog[0]) if eventlog else None
        sections = [s for s in config.sections if s.type not in ("zeo", "eventlog")]
        if not sections:
            raise ValueError("the configuration has no storage section")
        stores = [_store(section) for section in sections]
        if any(store.blob_dir is not None for store in stores):
            args.append("--shared-blob-dir=true")
        for store in stores:
            store.make_ready(read_only)

        descriptor, file = tempfile.mkstemp(".toml", prefix="zoe-forker", dir=".")
        with os.fdopen(descriptor, "w") as out:
            out.write("".join(store.toml() for store in stores))
        # zoe serve reads these files before it listens.
        files = [os.path.abspath(file)]
        try:
            if tls is not None:
                tls_args, tls_files = tls.server_args()
                args += tls_args
                files += tls_files
            server = ZoeServer(args=["--config", files[0], *args], log=log_file)
            server.address = address
            server.start(start_timeout)
        finally:
            for leftover in files:
                os.remove(leftover)
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


def native_client(address, **kw):
    """Open a `zoe.zeo.ClientStorage` with the native client inside.

    It takes the keyword arguments of `ZEO.ClientStorage.ClientStorage`.
    """
    import zoe.zeo

    return zoe.zeo.ClientStorage(address, native=True, **kw)


class NativeLayer:
    """A layer that makes `ZEO.ClientStorage.ClientStorage` and the
    `<zoeclient>` section open the native client.

    `setUp` puts the class of the native client in place of the class of
    ZEO in each loaded module that binds it under the name `ClientStorage`:
    `ZEO.ClientStorage` itself, and a test module that imported the class
    at its own import. `ZEO.client`, `ZEO.DB` and the `<zeoclient>` section
    of ZODB read the attribute of `ZEO.ClientStorage` when they run, so they
    open the native client too. A class of a loaded module that a test
    module derives from the class of ZEO, as `TestClientStorage` of
    `ZEO.tests.ConnectionTests`, gets the class of the native client as
    its base in place of the class of ZEO. The `<zoeclient>` section opens
    the native client whatever its key `native`. `tearDown` puts back the
    class of ZEO, the bases of each class and the open of `<zoeclient>`.

    zope.testrunner names a layer by `__name__` and `__bases__`.
    """

    def __init__(self, name):
        self.__name__ = name
        self.__bases__ = ()
        self._patches = []

    def __repr__(self):
        return f"<{type(self).__module__}.{self.__name__}>"

    def setUp(self):
        import ZEO.ClientStorage

        import zoe.config
        import zoe.zeo

        original = ZEO.ClientStorage.ClientStorage
        native = zoe.zeo._NativeClientStorage
        for module in list(sys.modules.values()):
            names = getattr(module, "__dict__", {})
            if names.get("ClientStorage") is original:
                self._patch(module, "ClientStorage", native)
            for value in list(names.values()):
                # The native class derives from the class of ZEO through
                # zoe.zeo.ClientStorage: these classes keep their bases.
                if (
                    isinstance(value, type)
                    and original in value.__bases__
                    and not issubclass(native, value)
                ):
                    bases = tuple(
                        native if base is original else base for base in value.__bases__
                    )
                    self._patch(value, "__bases__", bases)
        section = zoe.config.ZoeClient
        self._patch(section, "open", lambda client: client._open_client(native=True))

    def _patch(self, target, name, value):
        self._patches.append((target, name, getattr(target, name)))
        setattr(target, name, value)

    def tearDown(self):
        while self._patches:
            target, name, original = self._patches.pop()
            setattr(target, name, original)


NATIVE_LAYER = NativeLayer("NATIVE_LAYER")
