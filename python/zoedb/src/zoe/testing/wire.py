"""Record the ZEO wire between a stock ZEO server and its clients.

- `stock_server(directory, **options)` runs a stock ZEO server with msgpack
  in a child process and yields its address.
- `Recorder(address)` is a TCP proxy. It decodes each frame that passes.
"""

import contextlib
import dataclasses
import os
import socket
import struct
import subprocess
import sys
import threading

import msgpack

# The child process runs the server code of runzeo. It writes the address
# on its standard output and stops when its standard input closes.
_CHILD = """
import sys, threading
from ZEO import runzeo
options = runzeo.ZEOOptions()
options.realize(["-C", sys.argv[1]])
server = runzeo.ZEOServer(options)
server.open_storages()
server.create_server()
host, port = server.server.acceptor.addr[:2]
print(host, port, flush=True)
threading.Thread(target=server.server.loop, daemon=True).start()
sys.stdin.read()
server.server.close()
"""

_ZEO_KEYS = {
    "client_conflict_resolution",
    "read_only",
    "invalidation_queue_size",
    "invalidation_age",
    "transaction_timeout",
}


@contextlib.contextmanager
def stock_server(directory, **options):
    """Run a stock ZEO server on a FileStorage in `directory`.

    The server uses msgpack. `blob_dir` sets the blob directory of the
    FileStorage. `shared_blob_dir` changes nothing on the server: only the
    client sets it.
    """
    directory = os.fspath(directory)
    os.makedirs(directory, exist_ok=True)
    blob_dir = options.pop("blob_dir", os.path.join(directory, "blobs"))
    options.pop("shared_blob_dir", None)
    unknown = set(options) - _ZEO_KEYS
    if unknown:
        raise TypeError(f"unknown server options {sorted(unknown)}")
    lines = ["<zeo>", "address 127.0.0.1:0", "msgpack true"]
    for key, value in options.items():
        if isinstance(value, bool):
            value = "true" if value else "false"
        lines.append(f"{key.replace('_', '-')} {value}")
    lines.append("</zeo>")
    lines += [
        "<filestorage>",
        f"path {os.path.join(directory, 'Data.fs')}",
        f"blob-dir {blob_dir}",
        "</filestorage>",
    ]
    config = os.path.join(directory, "zeo.conf")
    with open(config, "w") as out:
        out.write("\n".join(lines) + "\n")
    child = subprocess.Popen(
        [sys.executable, "-I", "-c", _CHILD, config],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        text=True,
    )
    try:
        line = child.stdout.readline().split()
        if len(line) != 2:
            raise RuntimeError("the stock ZEO server did not start")
        yield (line[0], int(line[1]))
    finally:
        child.stdin.close()
        try:
            child.wait(timeout=30)
        except subprocess.TimeoutExpired:
            child.kill()
            child.wait()


@dataclasses.dataclass
class Message:
    """One frame, decoded as the stock ZEO code decodes it."""

    direction: str
    connection: int
    id: object
    flags: object
    method: object
    args: object
    raw: bytes


def _decode(raw):
    # The decoder of ZEO: `unpackb(data, raw=False, use_list=False)`.
    try:
        value = msgpack.unpackb(raw, raw=False, use_list=False)
    except Exception:
        return None, None, None, None
    if isinstance(value, tuple) and len(value) == 4:
        return value
    return None, None, None, None


def _receive(sock, size):
    data = bytearray()
    while len(data) < size:
        chunk = sock.recv(size - len(data))
        if not chunk:
            return None
        data += chunk
    return bytes(data)


class Recorder:
    """A TCP proxy to `address` that records each frame.

    A frame is a 4-byte big-endian length and a payload. The first payload
    of each side is the protocol. The other payloads are msgpack.
    """

    def __init__(self, address):
        self.upstream = address
        self._messages = []
        self._lock = threading.Lock()
        self._sockets = []
        self._threads = []
        self._connections = 0
        self._listener = None
        self.address = None

    @property
    def messages(self):
        with self._lock:
            return list(self._messages)

    def __enter__(self):
        self._listener = socket.create_server(("127.0.0.1", 0))
        self.address = self._listener.getsockname()[:2]
        self._start(self._accept)
        return self

    def __exit__(self, *exc):
        self._listener.close()
        with self._lock:
            sockets = list(self._sockets)
        for sock in sockets:
            with contextlib.suppress(OSError):
                sock.shutdown(socket.SHUT_RDWR)
            sock.close()
        for thread in self._threads:
            thread.join(10)

    def _start(self, target, *args):
        thread = threading.Thread(target=target, args=args, daemon=True)
        self._threads.append(thread)
        thread.start()

    def _accept(self):
        while True:
            try:
                client, _ = self._listener.accept()
            except OSError:
                return
            server = socket.create_connection(self.upstream)
            with self._lock:
                self._connections += 1
                number = self._connections
                self._sockets += [client, server]
            self._start(self._pump, client, server, "client", number)
            self._start(self._pump, server, client, "server", number)

    def _pump(self, source, target, direction, number):
        first = True
        try:
            while True:
                header = _receive(source, 4)
                if header is None:
                    break
                raw = _receive(source, struct.unpack(">I", header)[0])
                if raw is None:
                    break
                if first:
                    parts = (None, None, None, None)
                    first = False
                else:
                    parts = _decode(raw)
                message = Message(direction, number, *parts, raw)
                # Record before forwarding. Then each frame that an end
                # receives is in the list before the end can act on it.
                with self._lock:
                    self._messages.append(message)
                target.sendall(header + raw)
        except OSError:
            pass
        finally:
            with contextlib.suppress(OSError):
                target.shutdown(socket.SHUT_WR)
