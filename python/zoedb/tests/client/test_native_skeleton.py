"""Acceptance test of the walking skeleton of the native client.

`zoe.zeo.ClientStorage` runs on `zoe-client` for the thinnest slice of a
ZODB client: open, `new_oid`, `load` and `loadBefore` from the server, the
two-phase commit of ZODB, the invalidations of the other clients to the DB,
`sequence(name).next()` and `close`. One connection runs one transaction at
a time. There is no cache, no blob and no reconnect. `just test` runs this
module.

`zoe.testing.native_client(address, **kw)` opens a `zoe.zeo.ClientStorage`
with the native client inside. It takes the keyword arguments of
`ZEO.ClientStorage.ClientStorage`. The client stays an instance of
`ZEO.ClientStorage.ClientStorage`, and no thread of ZEO's asyncio client
starts.

- Two DBs on two native clients: a commit through the first reaches the
  second through the invalidation, when its next transaction begins.
- `loadBefore` at the tid of the second revision gives the first revision
  and its tid range.
- `new_oid` gives distinct oids.
- A store with a stale serial raises `ConflictError` with the oid.
- `tpc_abort` after a vote releases the locks, so another client commits
  the oid.
- `sequence("a").next()` increases.
- On a free-threaded build, the GIL stays off after the import, the open, a
  commit and a load.
- Against a stock ZEO 6 server, the client opens, commits and loads, and
  uses no extension.
- When the server stops, a load raises `ClientDisconnected`.
- `zoe.zeo._NOT_BUILT` names the arguments of `ZEO.ClientStorage` whose
  feature the native client does not have yet. Each of them fails the open
  with an error that names it; none is ignored. The list shrinks as the
  features come, and is empty when the client is complete.

Each test imports ZEO itself, so that the module collects without it.
"""

import json
import os
import pickle
import subprocess
import sys
import sysconfig
import threading
import time

import pytest

import zoe.testing
from zoe.testing import ZoeServer

# The longest time that a test waits for the server or a thread.
TIMEOUT = 30
# The serial of an object that has no record yet.
ZERO = bytes(8)
FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
# The end of the name of the network thread of ZEO's asyncio client.
NETWORK_THREAD = "zeo client networking thread"

# The program that the GIL test runs in a new interpreter. It prints the
# GIL state after each step as a JSON object.
GIL_PROGRAM = """
import json
import sys

from ZODB.Connection import TransactionMetaData

states = {}
import zoe.zeo

states["import zoe.zeo"] = sys._is_gil_enabled()
import zoe.testing

states["import zoe.testing"] = sys._is_gil_enabled()
address = (sys.argv[1], int(sys.argv[2]))
storage = zoe.testing.native_client(address, storage="1", wait_timeout=30)
try:
    states["open"] = sys._is_gil_enabled()
    oid = storage.new_oid()
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, bytes(8), b"data", "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    states["commit"] = sys._is_gil_enabled()
    assert tuple(storage.load(oid)) == (b"data", tid)
    states["load"] = sys._is_gil_enabled()
finally:
    storage.close()
print(json.dumps(states))
"""


def native(address, **kw):
    """Open the native client on the storage 1 of address."""
    kw.setdefault("wait_timeout", TIMEOUT)
    return zoe.testing.native_client(address, storage="1", **kw)


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


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


def network_threads(address):
    """Return the names of the threads of ZEO's client for address."""
    port = str(address[1])
    return [
        thread.name
        for thread in threading.enumerate()
        if thread.name.endswith(NETWORK_THREAD) and port in thread.name
    ]


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


def test_the_client_is_a_zeo_client_storage_without_zeo_threads(server):
    from ZEO.ClientStorage import ClientStorage

    import zoe.zeo

    storage = native(server.address)
    try:
        assert isinstance(storage, ClientStorage)
        assert isinstance(storage, zoe.zeo.ClientStorage)
        assert not network_threads(server.address)
    finally:
        storage.close()


def test_a_commit_reaches_the_other_db_through_the_invalidation(server):
    import transaction
    import ZODB
    from persistent.mapping import PersistentMapping

    first = ZODB.DB(native(server.address))
    try:
        second = ZODB.DB(native(server.address))
        try:
            writer_tm = transaction.TransactionManager()
            reader_tm = transaction.TransactionManager()
            writer = first.open(writer_tm)
            reader = second.open(reader_tm)
            try:
                writer_tm.begin()
                writer.root()["counter"] = PersistentMapping(value=0)
                writer_tm.commit()
                created = writer.root()["counter"]._p_serial
                wait_for(
                    lambda: second.storage.lastTransaction() >= created,
                    "the second client got the first commit",
                )
                reader_tm.begin()
                counter = reader.root()["counter"]
                assert counter["value"] == 0

                writer_tm.begin()
                writer.root()["counter"]["value"] = 1
                writer_tm.commit()
                changed = writer.root()["counter"]._p_serial
                assert changed > created
                wait_for(
                    lambda: second.storage.lastTransaction() >= changed,
                    "the second client got the second commit",
                )
                # The reader holds the object from its last transaction. Only
                # the invalidation makes the next transaction load it again.
                reader_tm.begin()
                assert counter["value"] == 1
                assert counter._p_serial == changed
            finally:
                writer_tm.abort()
                reader_tm.abort()
                writer.close()
                reader.close()
        finally:
            second.close()
    finally:
        first.close()


def test_load_before_gives_the_first_revision_and_its_tid_range(server):
    storage = native(server.address)
    try:
        oid = storage.new_oid()
        first = commit(storage, [(oid, ZERO, record(1))])
        second = commit(storage, [(oid, first, record(2))])
        assert second > first
        assert tuple(storage.loadBefore(oid, second)) == (record(1), first, second)
        assert tuple(storage.load(oid)) == (record(2), second)
    finally:
        storage.close()


def test_new_oid_gives_distinct_oids(server):
    first = native(server.address)
    try:
        second = native(server.address)
        try:
            oids = []
            for _ in range(250):
                oids += [first.new_oid(), second.new_oid()]
        finally:
            second.close()
    finally:
        first.close()
    assert all(isinstance(oid, bytes) and len(oid) == 8 for oid in oids)
    assert len(set(oids)) == len(oids)


def test_a_store_with_a_stale_serial_raises_conflict_error(server):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ConflictError

    storage = native(server.address)
    try:
        oid = storage.new_oid()
        first = commit(storage, [(oid, ZERO, record(1))])
        commit(storage, [(oid, first, record(2))])
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        try:
            # The client raises at the store or at the vote.
            with pytest.raises(ConflictError) as raised:
                storage.store(oid, first, record(3), "", txn)
                storage.tpc_vote(txn)
        finally:
            storage.tpc_abort(txn)
        assert raised.value.oid == oid
    finally:
        storage.close()


def test_an_abort_after_the_vote_releases_the_locks(server):
    from ZODB.Connection import TransactionMetaData

    first = native(server.address)
    try:
        second = native(server.address)
        try:
            oid = first.new_oid()
            tid = commit(first, [(oid, ZERO, record(1))])
            txn = TransactionMetaData()
            first.tpc_begin(txn)
            try:
                first.store(oid, tid, record(2), "", txn)
                first.tpc_vote(txn)
            finally:
                first.tpc_abort(txn)
            # With the locks still held, the vote of the second client
            # fails after the lock timeout of the server.
            committed = commit(second, [(oid, tid, record(3))])
            assert committed > tid
            assert tuple(second.load(oid)) == (record(3), committed)
        finally:
            second.close()
    finally:
        first.close()


def test_a_sequence_increases(server):
    storage = native(server.address)
    try:
        sequence = storage.sequence("a")
        values = [sequence.next() for _ in range(5)]
    finally:
        storage.close()
    assert values == list(range(1, 6))


@pytest.mark.skipif(not FREE_THREADED, reason="the GIL is always on in this build")
def test_the_gil_stays_off_after_the_import_the_open_a_commit_and_a_load(server):
    # A new interpreter starts with the GIL off. PYTHON_GIL must not force
    # the result.
    environment = {k: v for k, v in os.environ.items() if k != "PYTHON_GIL"}
    host, port = server.address
    result = subprocess.run(
        [sys.executable, "-c", GIL_PROGRAM, host, str(port)],
        env=environment,
        capture_output=True,
        text=True,
        timeout=TIMEOUT,
    )
    assert result.returncode == 0, result.stderr
    states = json.loads(result.stdout)
    assert list(states) == [
        "import zoe.zeo",
        "import zoe.testing",
        "open",
        "commit",
        "load",
    ]
    assert not any(states.values()), states


def test_against_a_stock_zeo_server_the_client_works_without_extensions(tmp_path):
    import ZEO
    from ZODB.POSException import Unsupported

    native_client = zoe.testing.native_client
    path = tmp_path / "Data.fs"
    # A ZEO 6 server speaks the msgpack protocol only with this option.
    address, stop = ZEO.server(path=str(path), zeo_conf={"msgpack": True})
    try:
        # It is the server of ZEO: its store is a FileStorage file.
        assert path.is_file()
        storage = native_client(address, storage="1", wait_timeout=TIMEOUT)
        try:
            oid = storage.new_oid()
            tid = commit(storage, [(oid, ZERO, record(1))])
            assert tuple(storage.load(oid)) == (record(1), tid)
            # The server accepted no extension.
            with pytest.raises(Unsupported):
                storage.sequence("a").next()
        finally:
            storage.close()
    finally:
        stop()


def test_a_load_after_the_server_stops_raises_client_disconnected(tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = native(server.address)
        try:
            # Another client writes the object, so the load must go to the
            # server.
            writer = native(server.address)
            try:
                oid = writer.new_oid()
                commit(writer, [(oid, ZERO, record(1))])
            finally:
                writer.close()
            assert server.stop() == 0
            with pytest.raises(ClientDisconnected):
                storage.load(oid)
        finally:
            storage.close()


def test_an_argument_whose_feature_is_not_built_fails_the_open(server, tmp_path):
    import zoe.zeo

    native_client = zoe.testing.native_client
    # The open refuses a name of the list before it reads its value.
    not_built = zoe.zeo._NOT_BUILT
    assert all(isinstance(name, str) for name in not_built)
    for name in not_built:
        with pytest.raises(Exception, match=name):
            storage = native_client(
                server.address,
                storage="1",
                wait_timeout=TIMEOUT,
                **{name: str(tmp_path / name)},
            )
            storage.close()
