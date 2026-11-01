"""Acceptance test of the hit path of the native client.

`just test` runs it.

- `loadBefore` of a connected client calls the native client directly:
  it makes no call of `_with_server`, on a hit and on a miss.
- Without a server, `loadBefore` behaves as `_with_server` does: it waits
  for a server and loads again; it raises `ClientDisconnected` with the
  message of the wait when no server comes; a closed client, a call that
  loses its server and any other error raise the error of the native
  client at once, unchanged.
- After `os.fork()`, each load of the child on a client of its parent
  raises the `RuntimeError` of the native client, on a hit and on a miss.
  The child drops such a client without its connections. A client that
  the child opens loads, and a grandchild refuses it in its turn. The
  parent still loads.
- No load of the parent calls `os.getpid`. On Linux, a seccomp filter on
  the loading thread shows that a load makes no `getpid` system call.

The unit tests give the native client of the test to the storage. The other
tests run against `zoe serve`.
"""

import os
import platform
import signal
import subprocess
import sys
import textwrap
import threading
import time

import pytest
from support import TIMEOUT, ZERO, commit, native, wait_for
from ZEO.Exceptions import ClientDisconnected
from ZODB.POSException import POSKeyError
from ZODB.utils import maxtid, p64

import zoe.zeo

# The record that the native client of the test loads.
RECORD = (b"data", p64(1), None)
OID = p64(7)
# The error of the native client in a child of `os.fork()`.
FORKED = "RuntimeError: the client was opened in another process, before a fork"


def fail(*args, **kw):
    raise AssertionError("loadBefore called _with_server")


# The unit tests.


class Native:
    """A native client of the test. Each `load_before` takes the next
    answer: an exception raises, another value returns."""

    def __init__(self, *answers, connected=True):
        self.answers = list(answers)
        self.connected = connected
        self.loads = 0

    def load_before(self, oid, tid):
        assert (oid, tid) == (OID, maxtid)
        self.loads += 1
        answer = self.answers.pop(0)
        if isinstance(answer, BaseException):
            raise answer
        return answer

    def is_connected(self):
        return self.connected


def storage(native, timeout=TIMEOUT):
    """Return a native storage around `native`, without an open. Its
    longest wait for a server is `timeout`."""
    result = object.__new__(zoe.zeo._NativeClientStorage)
    result.__name__ = "test"
    result._native = native
    result._closed = False
    result._server = zoe.zeo._NativeServer(result, timeout)
    result._operational = threading.Event()
    return result


def test_a_load_of_a_connected_client_calls_only_the_native_client():
    native = Native(RECORD)
    s = storage(native)
    s._with_server = fail
    assert s.loadBefore(OID, maxtid) == RECORD
    assert native.loads == 1


def test_a_load_without_a_server_waits_for_one_and_loads_again():
    native = Native(ClientDisconnected("lost"), RECORD, connected=False)
    s = storage(native)
    threading.Timer(0.1, lambda: setattr(native, "connected", True)).start()
    assert s.loadBefore(OID, maxtid) == RECORD
    assert native.loads == 2


def test_a_load_without_a_server_raises_after_the_wait():
    native = Native(ClientDisconnected("lost"), connected=False)
    s = storage(native, timeout=0.2)
    start = time.monotonic()
    with pytest.raises(ClientDisconnected, match="^the client has no server$"):
        s.loadBefore(OID, maxtid)
    assert time.monotonic() - start >= 0.2
    assert native.loads == 1


def test_a_load_of_a_closed_client_raises_its_error_at_once():
    error = ClientDisconnected("the client is closed")
    native = Native(error, connected=False)
    s = storage(native)
    s._closed = True
    start = time.monotonic()
    with pytest.raises(ClientDisconnected) as raised:
        s.loadBefore(OID, maxtid)
    assert raised.value is error
    assert time.monotonic() - start < 1
    assert native.loads == 1


def test_a_load_that_loses_its_server_raises_its_error():
    # The client is connected again when the error comes back.
    error = ClientDisconnected("lost")
    native = Native(error)
    with pytest.raises(ClientDisconnected) as raised:
        storage(native).loadBefore(OID, maxtid)
    assert raised.value is error
    assert native.loads == 1


@pytest.mark.parametrize("error", [POSKeyError(OID), RuntimeError("forked")])
def test_another_error_of_a_load_raises_unchanged(error):
    native = Native(error, connected=False)
    with pytest.raises(type(error)) as raised:
        storage(native).loadBefore(OID, maxtid)
    assert raised.value is error
    assert native.loads == 1


# The tests against `zoe serve`.


@pytest.fixture
def client(server):
    """A native client of `server`, with an object `a` in its cache and an
    object `b` that its cache does not hold. Yield the client, the oids
    and the tid of their commit."""
    storage = native(server)
    try:
        a, b = storage.new_oid(), storage.new_oid()
        tid = commit(storage, [(a, ZERO, b"a"), (b, ZERO, b"b")])
        storage._cache.clear()
        assert storage.loadBefore(a, maxtid) == (b"a", tid, None)
        yield storage, a, b, tid
    finally:
        storage.close()


def in_child(work):
    """Run `work()` in a child of `os.fork()`. Return `returned VALUE`
    with the repr of its result, or the name and the text of the exception
    that it raised."""
    read, write = os.pipe()
    pid = os.fork()
    if pid == 0:
        try:
            os.close(read)
            try:
                text = f"returned {work()!r}"
            except BaseException as error:
                text = f"{type(error).__name__}: {error}"
            os.write(write, text.encode())
        finally:
            os._exit(0)
    os.close(write)
    deadline = time.monotonic() + TIMEOUT
    while os.waitpid(pid, os.WNOHANG) == (0, 0):
        if time.monotonic() > deadline:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
            os.close(read)
            raise AssertionError(f"the child did not end in {TIMEOUT} seconds")
        time.sleep(0.01)
    with os.fdopen(read, "rb") as pipe:
        return pipe.read().decode()


def test_load_before_of_a_connected_client_makes_no_call_of_with_server(
    client, monkeypatch
):
    storage, a, b, tid = client
    monkeypatch.setattr(storage, "_with_server", fail)
    # A miss, then two hits.
    assert storage.loadBefore(b, maxtid) == (b"b", tid, None)
    assert storage.loadBefore(b, maxtid) == (b"b", tid, None)
    assert storage.loadBefore(a, maxtid) == (b"a", tid, None)
    assert storage.loadBefore(a, tid) is None


def test_a_closed_client_raises_client_disconnected(client):
    storage, a, b, tid = client
    storage.close()
    start = time.monotonic()
    with pytest.raises(ClientDisconnected, match="the client is closed"):
        storage.loadBefore(a, maxtid)
    assert time.monotonic() - start < TIMEOUT / 2


def test_a_load_waits_for_a_server_that_comes_back(server):
    storage = native(server, disconnect_poll=0.1)
    try:
        oid = storage.new_oid()
        tid = commit(storage, [(oid, ZERO, b"back")])
        storage._cache.clear()
        assert server.stop(TIMEOUT) == 0
        wait_for(lambda: not storage.is_connected())
        loaded = []
        thread = threading.Thread(
            target=lambda: loaded.append(storage.loadBefore(oid, maxtid))
        )
        thread.start()
        # The load waits for a server. The server starts on the same
        # address in this thread, which it outlives.
        time.sleep(0.3)
        assert loaded == []
        server.start(TIMEOUT)
        thread.join(TIMEOUT)
        assert loaded == [(b"back", tid, None)]
    finally:
        storage.close()


def test_after_a_fork_a_load_of_the_child_raises_as_today(client):
    storage, a, b, tid = client

    def loads():
        results = []
        for load in (
            lambda: storage.loadBefore(a, maxtid),
            lambda: storage.loadBefore(b, maxtid),
            lambda: storage.load(a),
        ):
            try:
                results.append(f"returned {load()!r}")
            except BaseException as error:
                results.append(f"{type(error).__name__}: {error}")
        return results

    assert in_child(loads) == f"returned {[FORKED] * 3!r}"
    # The parent still loads.
    assert storage.loadBefore(a, maxtid) == (b"a", tid, None)
    assert storage.loadBefore(b, maxtid) == (b"b", tid, None)


def test_the_child_drops_a_client_of_its_parent_without_its_connections(server, client):
    from zoe import _zoe

    storage, a, b, tid = client
    held = [
        _zoe.Client(
            [tuple(server.address)],
            "1",
            read_only=False,
            read_only_fallback=False,
            extensions=[],
            wait_timeout=float(TIMEOUT),
            cache_size=1 << 20,
        )
    ]

    def drop():
        connected = held[0].is_connected()
        try:
            held[0].load_before(a, maxtid)
        except RuntimeError as error:
            refused = str(error)
        held.clear()
        return connected, refused

    try:
        assert in_child(drop) == (
            "returned (False, "
            "'the client was opened in another process, before a fork')"
        )
        assert held[0].is_connected()
        assert held[0].load_before(a, maxtid) == (b"a", tid, None)
    finally:
        held[0].close()


def test_a_client_that_the_child_opens_loads_and_a_grandchild_refuses_it(
    server, client
):
    storage, a, b, tid = client

    def open_in_child():
        own = native(server)
        try:
            loaded = own.loadBefore(a, maxtid)
            refused = in_child(lambda: own.loadBefore(a, maxtid))
            again = own.loadBefore(b, maxtid)
            return loaded, refused, again
        finally:
            own.close()

    expected = ((b"a", tid, None), FORKED, (b"b", tid, None))
    assert in_child(open_in_child) == f"returned {expected!r}"


def test_no_load_of_the_parent_calls_os_getpid(client, monkeypatch):
    storage, a, b, tid = client
    calls = []
    getpid = os.getpid

    def counted():
        calls.append(None)
        return getpid()

    monkeypatch.setattr(os, "getpid", counted)
    assert storage.loadBefore(b, maxtid) == (b"b", tid, None)
    assert storage.loadBefore(a, maxtid) == (b"a", tid, None)
    assert storage.loadBefore(b, maxtid) == (b"b", tid, None)
    assert calls == []


# The number of the getpid system call on each machine.
GETPID = {"x86_64": 39, "aarch64": 172}

# A process that loads with a seccomp filter on its main thread. The filter
# makes each getpid system call of that thread fail with EPERM, so a load
# that checks the process with getpid raises. The process prints
# `unsupported` when the kernel refuses the filter, or when getpid does not
# reach it.
FILTERED = """
import ctypes
import os
import sys

from ZODB.utils import maxtid

import zoe.testing

host, port, oid, getpid = sys.argv[1:]
oid = bytes.fromhex(oid)
storage = zoe.testing.native_client((host, int(port)), storage="1", wait_timeout=30)


class Instruction(ctypes.Structure):
    _fields_ = [
        ("code", ctypes.c_ushort),
        ("jt", ctypes.c_ubyte),
        ("jf", ctypes.c_ubyte),
        ("k", ctypes.c_uint32),
    ]


class Program(ctypes.Structure):
    _fields_ = [("len", ctypes.c_ushort), ("filter", ctypes.POINTER(Instruction))]


program = (Instruction * 4)(
    # Load the number of the system call.
    Instruction(0x20, 0, 0, 0),
    # getpid fails with EPERM. Every other system call runs.
    Instruction(0x15, 0, 1, int(getpid)),
    Instruction(0x06, 0, 0, 0x00050000 | 1),
    Instruction(0x06, 0, 0, 0x7FFF0000),
)
compiled = Program(len(program), program)
libc = ctypes.CDLL(None, use_errno=True)
pid = os.getpid()
ulong = ctypes.c_ulong
if (
    libc.prctl(38, ulong(1), ulong(0), ulong(0), ulong(0)) != 0
    or libc.prctl(22, ulong(2), ctypes.byref(compiled), ulong(0), ulong(0)) != 0
    or os.getpid() == pid
):
    print("unsupported", flush=True)
    os._exit(0)
try:
    # A miss, then a hit.
    for _ in range(2):
        print(repr(storage.loadBefore(oid, maxtid)), flush=True)
except BaseException as error:
    print(f"{type(error).__name__}: {error}", flush=True)
    os._exit(1)
# The close checks the process with os.getpid.
os._exit(0)
"""


@pytest.mark.skipif(
    sys.platform != "linux" or platform.machine() not in GETPID,
    reason="seccomp is a filter of Linux",
)
def test_a_load_of_the_parent_makes_no_getpid_system_call(server, client):
    storage, a, b, tid = client
    host, port = server.address
    done = subprocess.run(
        [
            sys.executable,
            "-c",
            textwrap.dedent(FILTERED),
            host,
            str(port),
            a.hex(),
            str(GETPID[platform.machine()]),
        ],
        capture_output=True,
        text=True,
        timeout=TIMEOUT * 2,
    )
    if done.stdout.strip() == "unsupported":
        pytest.skip("the kernel gives no seccomp filter on getpid")
    assert done.returncode == 0, done.stdout + done.stderr
    assert done.stdout.splitlines() == [repr((b"a", tid, None))] * 2
