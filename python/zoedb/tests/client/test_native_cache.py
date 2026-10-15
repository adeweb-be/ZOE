"""Acceptance test of the cache of the native client.

Loads of the native client hit a cache in the client process, with the
semantics of `ZEO.cache.ClientCache`. The argument `cache_size` bounds it,
and leaves `zoe.zeo._NOT_BUILT`. `just test` runs this module.

The test counts the loads that reach the server. `zoe serve --admin-listen`
gives `zoe_loads_total`, the number of `loadBefore` calls of a storage, on
`/metrics` of its admin port. The test reads it before and after each step.

- A second load of an object does not reach the server.
- After two commits of an object, `loadBefore` at the second tid gives the
  first revision. A second such call does not reach the server.
- A commit of the client puts its data in the cache. A load after the
  finish does not reach the server.
- A commit of another client ends the current entry of each of its objects
  before `lastTransaction()` gives its tid. The next load gets the new
  data. The entry that ended still answers `loadBefore`.
- `storage._cache.clear()` empties the cache, so the next loads reach the
  server. zodbshootout calls it.
- With `cache_size` of 100 KB, after loads of 1,000 records of 1 KB, the
  cache holds at most 100 KB: a second pass in the reverse order gets at
  most 100 hits, and at least 50.
- A record larger than half of the cache is never cached, and loads
  right. A record of a quarter of the cache is cached.
- A `ConflictError` of a vote drops the entry of its object, so the next
  load reaches the server.
- Threads load a few objects at the snapshot of `lastTransaction()` while
  another client commits new revisions of them. Each load gives the
  revision that the last commit before the snapshot wrote. A current
  reply that arrived before a commit, and that the client stores after it
  delivered that commit, would give an older revision.
- On a free-threaded build, 16 threads load the same 100 objects together
  while another client commits new revisions of them. Each load gives the
  data of the snapshot of its thread.

The tests open the native client with `zoe.testing.native_client`.
"""

import http.client
import os
import subprocess
import sysconfig
import threading
import time

import pytest
from ZODB.utils import p64, u64

import zoe.testing
from zoe.testing import ZoeServer

# The longest time that a test waits for the server or a thread.
TIMEOUT = 30
# The serial of an object that has no record yet.
ZERO = bytes(8)
KB = 1024
FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
# The sample of the loadBefore calls of the storage 1.
LOADS = 'zoe_loads_total{storage="1"} '
# The size of the cache, and the number of records, of the size test.
CACHE_SIZE = 100 * KB
RECORDS = 1000
# The readers, the objects and the commits of the concurrent test.
READERS = 16
OBJECTS = 100
GENERATIONS = 50
# The readers, the objects and the commits of the snapshot test.
SNAPSHOT_READERS = 8
HOT_OBJECTS = 4
HOT_COMMITS = 200


class AdminServer:
    """A `zoe serve` process with its admin port, for a test.

    It runs `zoe serve --path path --listen 127.0.0.1:0 --admin-listen
    127.0.0.1:0`. The server writes `admin <address>` and then
    `listening <address>` to stdout. `address` is the ZEO address, `admin`
    the admin address. As a context manager, it starts the server and kills
    it at the end. The server dies with the thread that started it, as with
    `ZoeServer`.
    """

    def __init__(self, path):
        self.path = path
        self.address = None
        self.admin = None
        self._process = None

    def __enter__(self):
        command = [
            ZoeServer.binary(),
            "serve",
            "--path",
            str(self.path),
            "--listen",
            "127.0.0.1:0",
            "--admin-listen",
            "127.0.0.1:0",
        ]
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        self._process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
            preexec_fn=zoe.testing._die_with_parent(),
        )
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, self._process.kill)
        timer.start()
        try:
            for line in self._process.stdout:
                word, _, value = line.strip().partition(" ")
                if word not in ("admin", "listening"):
                    continue
                host, port = value.rsplit(":", 1)
                address = (host.strip("[]"), int(port))
                if word == "admin":
                    self.admin = address
                else:
                    self.address = address
                    break
        finally:
            timer.cancel()
            timer.join()
        if self.address is None or self.admin is None:
            self.__exit__()
            raise RuntimeError("zoe serve did not write its two addresses")
        return self

    def __exit__(self, *exc_info):
        process, self._process = self._process, None
        if process is not None:
            process.kill()
            process.wait()
            process.stdout.close()


def loads(server):
    """Return the number of loadBefore calls of the storage 1 of server."""
    connection = http.client.HTTPConnection(*server.admin, timeout=TIMEOUT)
    try:
        connection.request("GET", "/metrics")
        reply = connection.getresponse()
        text = reply.read().decode()
    finally:
        connection.close()
    assert reply.status == 200, text
    for line in text.splitlines():
        if line.startswith(LOADS):
            return int(float(line[len(LOADS) :]))
    raise AssertionError(f"no zoe_loads_total of the storage 1 in:\n{text}")


def counted(server, action):
    """Run action. Return its result and its loads that reached the server."""
    before = loads(server)
    result = action()
    return result, loads(server) - before


def native(address, **kw):
    """Open the native client on the storage 1 of address."""
    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.testing.native_client(address, storage="1", **kw)


def commit(storage, stores):
    """Commit (oid, serial, data) items through storage. Return the tid."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        storage.tpc_vote(txn)
        return storage.tpc_finish(txn)
    except BaseException:
        storage.tpc_abort(txn)
        raise


def wait_for(condition, what):
    """Poll condition until it is true. Fail after TIMEOUT seconds."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        if time.monotonic() > deadline:
            raise AssertionError(f"{what} did not happen in {TIMEOUT} seconds")
        time.sleep(0.01)


def seen(storage, tid):
    """Wait until storage gave tid as its last transaction."""
    wait_for(lambda: storage.lastTransaction() >= tid, "the client got the commit")


def load(storage, oid):
    return tuple(storage.load(oid))


def load_before(storage, oid, tid):
    return tuple(storage.loadBefore(oid, tid))


@pytest.fixture
def server(tmp_path):
    with AdminServer(tmp_path / "store") as server:
        yield server


@pytest.fixture
def clients(server):
    """Return the client under test and a second client that writes."""
    storage = native(server.address)
    try:
        writer = native(server.address)
        try:
            yield storage, writer
        finally:
            writer.close()
    finally:
        storage.close()


def test_a_second_load_does_not_reach_the_server(server, clients):
    storage, writer = clients
    oid = writer.new_oid()
    tid = commit(writer, [(oid, ZERO, b"first")])
    seen(storage, tid)
    assert counted(server, lambda: load(storage, oid)) == ((b"first", tid), 1)
    assert counted(server, lambda: load(storage, oid)) == ((b"first", tid), 0)


def test_a_second_load_before_does_not_reach_the_server(server, clients):
    storage, writer = clients
    oid = writer.new_oid()
    first = commit(writer, [(oid, ZERO, b"first")])
    second = commit(writer, [(oid, first, b"second")])
    seen(storage, second)
    expected = (b"first", first, second)
    assert counted(server, lambda: load_before(storage, oid, second)) == (expected, 1)
    assert counted(server, lambda: load_before(storage, oid, second)) == (expected, 0)


def test_a_commit_of_the_client_puts_its_data_in_the_cache(server, clients):
    storage, _ = clients
    oid = storage.new_oid()
    tid = commit(storage, [(oid, ZERO, b"own")])
    assert counted(server, lambda: load(storage, oid)) == ((b"own", tid), 0)


def test_a_commit_of_another_client_ends_the_current_entries(server, clients):
    storage, writer = clients
    oids = [writer.new_oid() for _ in range(2)]
    first = commit(writer, [(oid, ZERO, b"first %d" % i) for i, oid in enumerate(oids)])
    seen(storage, first)
    for oid in oids:
        load(storage, oid)
    second = commit(
        writer, [(oid, first, b"second %d" % i) for i, oid in enumerate(oids)]
    )
    # The client ends the entries before lastTransaction() gives the tid. So
    # the loads right after it get the new data from the server.
    seen(storage, second)
    current = [(b"second %d" % i, second) for i in range(len(oids))]
    assert counted(server, lambda: [load(storage, oid) for oid in oids]) == (
        current,
        len(oids),
    )
    # The entries end at the tid: the client keeps them.
    ended = [(b"first %d" % i, first, second) for i in range(len(oids))]
    assert counted(
        server, lambda: [load_before(storage, oid, second) for oid in oids]
    ) == (ended, 0)


def test_clear_empties_the_cache(server, clients):
    storage, writer = clients
    oid = writer.new_oid()
    tid = commit(writer, [(oid, ZERO, b"data")])
    seen(storage, tid)
    assert counted(server, lambda: load(storage, oid)) == ((b"data", tid), 1)
    assert counted(server, lambda: load(storage, oid)) == ((b"data", tid), 0)
    storage._cache.clear()
    assert counted(server, lambda: load(storage, oid)) == ((b"data", tid), 1)


def test_the_cache_holds_at_most_its_size(server):
    def data(index):
        return index.to_bytes(4, "big") * (KB // 4)

    storage = native(server.address, cache_size=CACHE_SIZE)
    try:
        writer = native(server.address)
        try:
            oids = [writer.new_oid() for _ in range(RECORDS)]
            for start in range(0, RECORDS, 100):
                tid = commit(
                    writer,
                    [(oids[i], ZERO, data(i)) for i in range(start, start + 100)],
                )
        finally:
            writer.close()
        seen(storage, tid)
        _, misses = counted(server, lambda: [load(storage, oid) for oid in oids])
        assert misses == RECORDS
        # The newest records stay in the cache, so the reverse order meets
        # them first.
        backwards = list(reversed(range(RECORDS)))
        loaded, misses = counted(
            server, lambda: [load(storage, oids[i])[0] for i in backwards]
        )
        assert loaded == [data(i) for i in backwards]
        hits = RECORDS - misses
        assert hits <= CACHE_SIZE // KB, f"{hits} records of 1 KB hit the cache"
        # The heads of the entries take a part of the ring, not half of it.
        assert hits >= CACHE_SIZE // KB // 2, f"only {hits} records hit the cache"
    finally:
        storage.close()


def test_a_record_larger_than_half_of_the_cache_is_never_cached(server):
    large = bytes(range(256)) * ((CACHE_SIZE // 2 + KB) // 256)
    quarter = bytes(range(256)) * (CACHE_SIZE // 4 // 256)
    storage = native(server.address, cache_size=CACHE_SIZE)
    try:
        writer = native(server.address)
        try:
            oid, small = writer.new_oid(), writer.new_oid()
            tid = commit(writer, [(oid, ZERO, large), (small, ZERO, quarter)])
        finally:
            writer.close()
        seen(storage, tid)
        for _ in range(2):
            result, misses = counted(server, lambda: load(storage, oid))
            assert result == (large, tid)
            assert misses == 1
        # A record of a quarter of the cache goes in.
        assert counted(server, lambda: load(storage, small)) == ((quarter, tid), 1)
        assert counted(server, lambda: load(storage, small)) == ((quarter, tid), 0)
        # A commit of the client does not cache a large record either.
        own = storage.new_oid()
        own_tid = commit(storage, [(own, ZERO, large)])
        result, misses = counted(server, lambda: load(storage, own))
        assert result == (large, own_tid)
        assert misses == 1
    finally:
        storage.close()


def test_cache_size_is_built(server):
    import zoe.zeo

    assert "cache_size" not in zoe.zeo._NOT_BUILT
    storage = native(server.address, cache_size=CACHE_SIZE)
    try:
        assert callable(storage._cache.clear)
    finally:
        storage.close()


def test_a_conflict_drops_the_entry_of_its_object(server, clients):
    from ZODB.POSException import ConflictError

    storage, writer = clients
    oid = writer.new_oid()
    first = commit(writer, [(oid, ZERO, b"first")])
    second = commit(writer, [(oid, first, b"second")])
    seen(storage, second)
    assert counted(server, lambda: load(storage, oid)) == ((b"second", second), 1)
    assert counted(server, lambda: load(storage, oid)) == ((b"second", second), 0)
    # The data is not a pickle, so no resolution can save the conflict.
    with pytest.raises(ConflictError):
        commit(storage, [(oid, first, b"stale")])
    assert counted(server, lambda: load(storage, oid)) == ((b"second", second), 1)


def test_a_load_at_a_snapshot_gives_the_revision_of_the_snapshot(server, clients):
    storage, writer = clients
    oids = [writer.new_oid() for _ in range(HOT_OBJECTS)]
    # The tid of each commit of the writer, in order. Each commit writes
    # every object, so the revision of a snapshot S is the one of the last
    # tid up to S.
    tids = []

    def write(generation):
        serial = tids[-1] if tids else ZERO
        stores = [
            (oid, serial, b"%d:%d" % (generation, i)) for i, oid in enumerate(oids)
        ]
        tids.append(commit(writer, stores))

    write(0)
    seen(storage, tids[0])
    done = threading.Event()
    errors = []
    loads_ = []

    def read():
        try:
            while not done.is_set():
                last = storage.lastTransaction()
                before = p64(u64(last) + 1)
                for index, oid in enumerate(oids):
                    record, start, _ = load_before(storage, oid, before)
                    loads_.append((last, index, record, start))
        except BaseException as error:
            errors.append(error)

    threads = [
        threading.Thread(target=read, daemon=True) for _ in range(SNAPSHOT_READERS)
    ]
    for thread in threads:
        thread.start()
    try:
        for generation in range(1, HOT_COMMITS + 1):
            write(generation)
    finally:
        done.set()
        for thread in threads:
            thread.join(TIMEOUT)
    assert not [thread for thread in threads if thread.is_alive()]
    assert not errors, errors
    assert loads_
    for last, index, record, start in loads_:
        generation = max(g for g, tid in enumerate(tids) if tid <= last)
        expected = (b"%d:%d" % (generation, index), tids[generation])
        assert (record, start) == expected, (
            f"object {index} at {last.hex()}: {record!r} of {start.hex()}"
        )


@pytest.mark.skipif(not FREE_THREADED, reason="the check needs a free-threaded build")
def test_threads_load_the_data_of_their_snapshot(server, clients):
    storage, writer = clients
    oids = [writer.new_oid() for _ in range(OBJECTS)]

    def data(generation, index):
        return b"%d:%d" % (generation, index)

    serials = [ZERO]
    # The tid of each generation, in order.
    tids = []

    def write(generation):
        stores = [
            (oid, serials[0], data(generation, index)) for index, oid in enumerate(oids)
        ]
        serials[0] = commit(writer, stores)
        tids.append(serials[0])

    write(0)
    seen(storage, serials[0])
    barrier = threading.Barrier(READERS + 1, timeout=TIMEOUT)
    done = threading.Event()
    errors = []
    rounds = []

    def check(results, before):
        # One commit writes every object, so a snapshot holds one
        # generation, with one start tid.
        starts = {start for _, start, _ in results}
        assert len(starts) == 1, f"{len(starts)} start tids in one snapshot"
        generation = int(results[0][0].split(b":")[0])
        for index, (record, start, end) in enumerate(results):
            assert record == data(generation, index), (index, record)
            assert start < before, index
            assert end is None or end >= before, index
        return generation

    def read():
        try:
            barrier.wait()
            while True:
                last = done.is_set()
                before = p64(u64(storage.lastTransaction()) + 1)
                results = [load_before(storage, oid, before) for oid in oids]
                rounds.append((before, check(results, before)))
                if last:
                    break
        except BaseException as error:
            errors.append(error)
            barrier.abort()

    threads = [threading.Thread(target=read, daemon=True) for _ in range(READERS)]
    for thread in threads:
        thread.start()
    try:
        barrier.wait()
        for generation in range(1, GENERATIONS + 1):
            write(generation)
    except threading.BrokenBarrierError:
        # A reader failed before the start. errors holds the cause.
        pass
    finally:
        done.set()
        for thread in threads:
            thread.join(TIMEOUT)
    assert not [thread for thread in threads if thread.is_alive()]
    assert not errors, errors
    assert len(rounds) >= READERS
    # The generation of each snapshot is the last one that committed
    # before it.
    for before, generation in rounds:
        expected = max(g for g, tid in enumerate(tids) if tid < before)
        assert generation == expected, (before.hex(), generation, expected)
