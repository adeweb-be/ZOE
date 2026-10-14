"""Acceptance test of the walking skeleton of `zoe serve`.

An unmodified `ZEO.ClientStorage` registers with `zoe serve`, loads,
stores, commits and reloads through it, alone and under a `ZODB.DB`. The
store keeps one format in both modes: a store written in-process serves as
it is, and a store written through the server opens in-process. An
acknowledged commit survives a SIGKILL of the server at any point: the
kill loop runs `ZOE_KILL_LOOP_RUNS` rounds (5 by default) with the seed
`ZOE_KILL_LOOP_SEED`.

`zoe.testing.ZoeServer(path=None, args=(), env=None)` runs the `zoe` binary
as `zoe serve --path path --listen 127.0.0.1:0` with the extra arguments
`args`:

- `start()` starts the server and returns once it listens. A later
  `start()` uses the same port, so a client reconnects.
- `address` is `(host, port)` of the ZEO port.
- `kill()` sends SIGKILL and waits. `stop(timeout=30)` sends SIGTERM,
  waits and returns the exit code.
- As a context manager, it starts the server and kills it at the end.

Each test imports ZODB and ZEO itself, so that the module collects before
they are installed.
"""

import os
import pickle
import random
import threading
import time

import pytest

import zoe
from zoe.testing import ZoeServer

# The tid and the serial of an object that has no record yet.
ZERO = bytes(8)
# The longest time that a test waits for the server or a thread.
TIMEOUT = 30
# The rounds of the kill loop and the seed of their timing.
KILL_RUNS = int(os.environ.get("ZOE_KILL_LOOP_RUNS", "5"))
KILL_SEED = int(os.environ.get("ZOE_KILL_LOOP_SEED", "1"))


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def client(server, **kw):
    """Return a stock ClientStorage connected to the storage 1 of server."""
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", **kw)


def commit(storage, stores):
    """Commit (oid, serial, data) items through storage. Return the tid."""
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(b"user", b"description")
    storage.tpc_begin(txn)
    for oid, serial, data in stores:
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


@pytest.fixture
def path(tmp_path):
    return str(tmp_path / "store")


def test_a_stock_client_stores_commits_and_loads(path):
    with ZoeServer(path) as server:
        storage = client(server)
        try:
            assert storage.lastTransaction() == ZERO
            oid = storage.new_oid()
            tid = commit(storage, [(oid, ZERO, record("one"))])
            assert storage.lastTransaction() == tid
            assert storage.load(oid) == (record("one"), tid)

            second = commit(storage, [(oid, tid, record("two"))])
            assert second > tid
            assert storage.load(oid) == (record("two"), second)
            assert storage.loadBefore(oid, second) == (record("one"), tid, second)
        finally:
            storage.close()

        # A new client reads what the first one committed.
        other = client(server)
        try:
            assert other.lastTransaction() == second
            assert other.load(oid) == (record("two"), second)
        finally:
            other.close()


def test_an_unknown_oid_is_a_poskeyerror(path):
    from ZODB.POSException import POSKeyError

    with ZoeServer(path) as server:
        storage = client(server)
        try:
            with pytest.raises(POSKeyError):
                storage.load(storage.new_oid())
        finally:
            storage.close()


def test_a_database_commits_and_reloads_through_the_server(path):
    import transaction
    import ZODB
    from persistent.mapping import PersistentMapping

    with ZoeServer(path) as server:
        db = ZODB.DB(client(server))
        try:
            with db.transaction() as conn:
                conn.root()["folder"] = PersistentMapping(title="first")
            with db.transaction() as conn:
                conn.root()["folder"]["title"] = "second"
        finally:
            db.close()

        db = ZODB.DB(client(server))
        try:
            conn = db.open()
            assert conn.root()["folder"]["title"] == "second"
            transaction.abort()
            conn.close()
        finally:
            db.close()


def test_the_store_keeps_one_format_in_both_modes(path):
    import ZODB
    from persistent.mapping import PersistentMapping

    # Written in-process.
    db = ZODB.DB(zoe.Storage(path))
    with db.transaction() as conn:
        conn.root()["kept"] = PersistentMapping(by="in-process")
    db.close()

    # Read and written through the server.
    with ZoeServer(path) as server:
        db = ZODB.DB(client(server))
        try:
            with db.transaction() as conn:
                assert conn.root()["kept"]["by"] == "in-process"
                conn.root()["kept"]["by"] = "server"
        finally:
            db.close()
        assert server.stop() == 0

    # Read in-process again.
    db = ZODB.DB(zoe.Storage(path))
    try:
        with db.transaction() as conn:
            assert conn.root()["kept"]["by"] == "server"
    finally:
        db.close()


def test_an_acknowledged_commit_survives_a_kill_of_the_server(path):
    """The kill loop: a writer commits while the server dies at random."""
    from ZEO.Exceptions import ClientDisconnected

    rng = random.Random(KILL_SEED)
    acknowledged = {}
    with ZoeServer(path) as server:
        for run in range(KILL_RUNS):
            done = threading.Event()
            confirmed = []

            def writer(server=server, done=done, confirmed=confirmed, run=run):
                storage = client(server, wait_timeout=1)
                try:
                    n = 0
                    while not done.is_set():
                        oid = storage.new_oid()
                        value = f"{run}-{n}"
                        tid = commit(storage, [(oid, ZERO, record(value))])
                        # The finish returned: the commit is acknowledged.
                        confirmed.append((oid, value, tid))
                        n += 1
                except (ClientDisconnected, OSError):
                    pass
                except Exception as error:
                    if not done.is_set():
                        confirmed.append(error)
                finally:
                    try:
                        storage.close()
                    except Exception:
                        pass

            thread = threading.Thread(target=writer, daemon=True)
            thread.start()
            # Let some commits through, then kill at a random point.
            deadline = time.monotonic() + TIMEOUT
            while len(confirmed) < 3 and time.monotonic() < deadline:
                time.sleep(0.01)
            time.sleep(rng.uniform(0.0, 0.2))
            server.kill()
            done.set()
            thread.join(TIMEOUT)
            assert not thread.is_alive(), "the writer did not stop"
            errors = [item for item in confirmed if isinstance(item, Exception)]
            assert not errors, errors
            assert confirmed, f"round {run} acknowledged no commit"
            for oid, value, tid in confirmed:
                acknowledged[oid] = (value, tid)

            server.start()
            storage = client(server)
            try:
                lost = [
                    oid
                    for oid, (value, tid) in acknowledged.items()
                    if storage.load(oid) != (record(value), tid)
                ]
            finally:
                storage.close()
            assert lost == [], f"round {run} lost acknowledged commits: {lost}"
