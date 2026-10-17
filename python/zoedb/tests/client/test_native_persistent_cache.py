"""Acceptance test of the persistent cache of the native client.

`just test` runs this module. With `client` (and `var` for its directory),
the cache of the native client lives in one file, mapped in memory, with
compressed entries. The name of the file follows ZEO's,
`<client>-<storage>` and an extension, in `var`. The file survives a
restart of the process. At open, the client verifies it against the
server, as after a reconnect:

- an up-to-date file serves every load, and no load reaches the server;
- after commits of other clients, only the oids of those commits reach
  the server;
- after more commits than `--invalidation-queue-size`, the client drops
  the file, and every oid reaches the server again.

Every entry carries a checksum: an entry whose check fails is dropped,
never served, and the other entries still serve. A file cut short is
dropped. A client process killed while it writes entries leaves a file
that serves only right data. A second process that opens the same file
fails as ZEO does, with `zc.lockfile.LockError`. The file never grows past
`cache_size` and a header.

The server counts each `loadBefore` in `zoe_loads_total` on the admin port
(`--admin-listen`). The records are random bytes, so a compressor keeps
them as they are, and a test finds the data of an entry in the file. The
tests open the native client through `zoe.testing.native_client`.
"""

import os
import subprocess
import sys
import threading
import urllib.request

import pytest
from ZODB.Connection import TransactionMetaData
from ZODB.utils import p64

import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30
ZERO = bytes(8)
CLIENT = "c"
# The size of each record, and the number of records of most tests.
RECORD = 1024
COUNT = 20
# The largest header that the test lets the file have.
HEADER = 64 * 1024

# A client process that loads its records again and again. Its cache is
# smaller than the records, so each load writes an entry. It prints
# "loaded" after the first pass.
LOADER = """\
import sys

from ZODB.utils import p64

import zoe.testing

host, port, var, cache_size, count = sys.argv[1:]
storage = zoe.testing.native_client(
    (host, int(port)),
    storage="1",
    wait_timeout=30,
    client="c",
    var=var,
    cache_size=int(cache_size),
)
oids = [p64(n) for n in range(1, int(count) + 1)]
for oid in oids:
    storage.load(oid)
print("loaded", flush=True)
while True:
    for oid in oids:
        storage.load(oid)
"""

# A client process that holds the cache file open until its stdin closes.
HOLDER = """\
import sys

import zoe.testing

host, port, var = sys.argv[1:]
storage = zoe.testing.native_client(
    (host, int(port)), storage="1", wait_timeout=30, client="c", var=var
)
print("open", flush=True)
sys.stdin.read()
storage.close()
"""


class AdminServer(ZoeServer):
    """`zoe serve` with an admin port on 127.0.0.1.

    The server writes the line `admin ADDRESS` before the line `listening
    ADDRESS`. `ZoeServer.start` reads past the first line, so this class
    starts the process itself. `admin` is the address of the admin port.
    """

    def __init__(self, path, args=()):
        super().__init__(path, args=["--admin-listen", "127.0.0.1:0", *args])
        self.admin = None

    def start(self, timeout=TIMEOUT):
        command = [self.binary(), "serve", "--path", str(self.path)]
        command += ["--listen", "127.0.0.1:0", *self.args]
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
            # On Linux, the kernel kills the server when this thread ends,
            # as for ZoeServer.
            preexec_fn=zoe.testing._die_with_parent(),
        )
        timer = threading.Timer(timeout, process.kill)
        timer.start()
        try:
            for line in process.stdout:
                word, _, value = line.strip().partition(" ")
                if word == "admin":
                    self.admin = value
                elif word == "listening":
                    host, port = value.rsplit(":", 1)
                    self.address = (host.strip("[]"), int(port))
                    self._process = process
                    return self
        finally:
            timer.cancel()
        process.wait()
        process.stdout.close()
        raise RuntimeError("zoe serve stopped before it listened")

    def loads(self):
        """Return the number of `loadBefore` calls of the storage 1."""
        url = f"http://{self.admin}/metrics"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
            text = reply.read().decode()
        prefix = 'zoe_loads_total{storage="1"} '
        for line in text.splitlines():
            if line.startswith(prefix):
                return int(float(line[len(prefix) :]))
        raise AssertionError("/metrics has no zoe_loads_total for the storage 1")


def native(server, var, **kw):
    """Open a native client with the persistent cache of `CLIENT` in `var`."""
    return zoe.testing.native_client(
        server.address,
        storage="1",
        wait_timeout=TIMEOUT,
        client=CLIENT,
        var=str(var),
        **kw,
    )


def records(count=COUNT):
    """Return `count` records of random bytes, by oid."""
    return {p64(n): os.urandom(RECORD) for n in range(1, count + 1)}


def commit(server, changes, serials):
    """Commit `changes` (oid: data) through a stock ZEO client.

    `serials` gives the current serial of each oid, and gets the new ones.
    """
    from ZEO.ClientStorage import ClientStorage

    writer = ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
    try:
        txn = TransactionMetaData()
        writer.tpc_begin(txn)
        for oid, data in changes.items():
            writer.store(oid, serials.get(oid, ZERO), data, "", txn)
        writer.tpc_vote(txn)
        tid = writer.tpc_finish(txn)
    finally:
        writer.close()
    serials.update(dict.fromkeys(changes, tid))
    return tid


def reached(server, storage, expected):
    """Load each oid of `expected` and check its data.

    Return the number of loads that reached the server.
    """
    before = server.loads()
    for oid, data in expected.items():
        assert storage.load(oid)[0] == data, f"wrong data for {oid.hex()}"
    return server.loads() - before


def fill(server, var, expected, **kw):
    """Open a native client, load each oid of `expected`, and close it."""
    storage = native(server, var, **kw)
    try:
        assert reached(server, storage, expected) == len(expected)
    finally:
        storage.close()


def cache_file(var):
    """Return the cache file of `CLIENT` for the storage 1 in `var`."""
    files = [
        path
        for path in var.iterdir()
        if path.name.startswith(f"{CLIENT}-1.") and not path.name.endswith(".lock")
    ]
    assert len(files) == 1, f"{var} holds {sorted(p.name for p in files)}"
    return files[0]


def place(content, data):
    """Return the offset of the middle of `data` in `content`.

    A compressor keeps random bytes as they are, so the data of an entry is
    in the file once.
    """
    window = data[RECORD // 4 : RECORD // 4 + 64]
    offset = content.find(window)
    assert offset >= 0, "the file does not hold the bytes of an entry as they are"
    assert content.find(window, offset + 1) < 0, "the bytes of an entry come twice"
    return offset


@pytest.fixture
def var(tmp_path):
    path = tmp_path / "var"
    path.mkdir()
    return path


def test_a_reopened_client_loads_from_an_up_to_date_file(tmp_path, var):
    expected, serials = records(), {}
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)
        fill(server, var, expected)

        storage = native(server, var)
        try:
            assert reached(server, storage, expected) == 0
        finally:
            storage.close()


def test_a_reopened_client_reaches_the_server_for_the_changed_oids_only(tmp_path, var):
    expected, serials = records(), {}
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)
        fill(server, var, expected)

        # Another client commits while the first is closed.
        changes = {oid: os.urandom(RECORD) for oid in list(expected)[:5]}
        commit(server, changes, serials)
        expected.update(changes)

        storage = native(server, var)
        try:
            assert reached(server, storage, expected) == len(changes)
        finally:
            storage.close()


def test_more_commits_than_the_invalidation_queue_drop_the_file(tmp_path, var):
    expected, serials = records(), {}
    args = ["--invalidation-queue-size", "3"]
    with AdminServer(tmp_path / "store", args) as server:
        commit(server, expected, serials)
        fill(server, var, expected)

        # Five commits while the client is closed: getInvalidations gives
        # no answer for more than three.
        for oid in list(expected)[:5]:
            change = {oid: os.urandom(RECORD)}
            commit(server, change, serials)
            expected.update(change)

        storage = native(server, var)
        try:
            assert reached(server, storage, expected) == len(expected)
        finally:
            storage.close()


def test_flipped_bytes_in_one_entry_drop_that_entry_only(tmp_path, var):
    expected, serials = records(), {}
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)
        fill(server, var, expected)

        path = cache_file(var)
        content = bytearray(path.read_bytes())
        victim = list(expected)[7]
        offset = place(bytes(content), expected[victim])
        for index in range(offset + 16, offset + 24):
            content[index] ^= 0xFF
        path.write_bytes(bytes(content))

        storage = native(server, var)
        try:
            # The entry of the victim is never served: its load reaches the
            # server and gives the right data. The other entries serve.
            assert reached(server, storage, expected) == 1
        finally:
            storage.close()


def test_a_file_cut_short_is_dropped_and_the_client_opens(tmp_path, var):
    expected, serials = records(), {}
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)
        fill(server, var, expected)

        # Cut the file inside the first entry that it holds.
        path = cache_file(var)
        content = path.read_bytes()
        first = min(place(content, data) for data in expected.values())
        with open(path, "r+b") as f:
            f.truncate(first + 32)

        storage = native(server, var)
        try:
            assert reached(server, storage, expected) == len(expected)
        finally:
            storage.close()


def test_a_client_killed_while_it_writes_leaves_only_right_entries(tmp_path, var):
    expected, serials = records(400), {}
    cache_size = 100 * 1024
    script = tmp_path / "loader.py"
    script.write_text(LOADER)
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)

        host, port = server.address
        arguments = [host, str(port), str(var), str(cache_size), str(len(expected))]
        child = subprocess.Popen(
            [sys.executable, str(script), *arguments],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            text=True,
        )
        timer = threading.Timer(TIMEOUT, child.kill)
        timer.start()
        try:
            assert child.stdout.readline().strip() == "loaded"
        finally:
            timer.cancel()
            # The child loads in a loop, so the kill comes while it writes
            # entries.
            child.kill()
            child.wait()
            child.stdout.close()

        storage = native(server, var, cache_size=cache_size)
        try:
            for oid, data in expected.items():
                assert storage.load(oid)[0] == data, f"wrong data for {oid.hex()}"
        finally:
            storage.close()


def test_a_second_process_on_the_same_file_fails_as_zeo_does(tmp_path, var):
    import zc.lockfile

    expected, serials = records(), {}
    script = tmp_path / "holder.py"
    script.write_text(HOLDER)
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)

        host, port = server.address
        child = subprocess.Popen(
            [sys.executable, str(script), host, str(port), str(var)],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            text=True,
        )
        timer = threading.Timer(TIMEOUT, child.kill)
        timer.start()
        try:
            assert child.stdout.readline().strip() == "open"
            with pytest.raises(zc.lockfile.LockError):
                native(server, var)
        finally:
            timer.cancel()
            child.stdin.close()
            try:
                child.wait(TIMEOUT)
            finally:
                child.kill()
                child.wait()
                child.stdout.close()


def test_the_file_never_grows_past_the_cache_size_and_a_header(tmp_path, var):
    expected, serials = records(1000), {}
    cache_size = 100 * 1024
    with AdminServer(tmp_path / "store") as server:
        commit(server, expected, serials)

        storage = native(server, var, cache_size=cache_size)
        try:
            oids = list(expected)
            for start in range(0, len(oids), 100):
                for oid in oids[start : start + 100]:
                    assert storage.load(oid)[0] == expected[oid]
                size = cache_file(var).stat().st_size
                assert size <= cache_size + HEADER, size
        finally:
            storage.close()
    size = cache_file(var).stat().st_size
    assert size <= cache_size + HEADER, size
