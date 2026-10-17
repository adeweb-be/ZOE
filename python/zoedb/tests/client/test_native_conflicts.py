"""Acceptance test of conflict resolution in the native client.

`just test` runs it. Each test opens two DBs on two native clients,
through `zoe.testing.native_client`, against one `zoe serve`.

`zoe serve` merges the conflicts of BTree buckets, sets and `Length` itself
(tier 1), and its reply gives each merged oid as a bare oid. For another
class, when a vote meets a stale serial, its reply lists the conflict as a
map: `oid`, `serials` (the committed serial and the serial that the client
read) and `data` (the pickle that the client stored). The native client
then resolves the conflict in the client, as the vote of
`ZEO.ClientStorage` does: it calls `tryToResolveConflict(oid, committed,
read, data)`, stores the result with `storea` at the committed serial and
votes again. The vote returns the oids that the server or the client
resolved, so the DB loads their state again.

- Two inserts of different keys into one `OOBTree` bucket both commit: the
  server merges them.
- Two increments of a `PCounter` of `ZODB.tests.ConflictResolution` both
  count.
- A conflict that the class does not resolve raises `ConflictError` with
  its oid and its serials. The client drops its cache entry of the oid,
  so the next load of the oid reaches the server.
- After a resolved commit, a load of the oid gives the resolved state, not
  the state that the client stored.

The test counts the loads that reach the server with `zoe_loads_total` of
`GET /metrics` on the admin port of `zoe serve`.
"""

import io
import os
import pickle
import subprocess
import threading
import time
import urllib.request

import pytest
from ZODB.POSException import ConflictError

import zoe.testing
from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30


class Server:
    """A `zoe serve` process with an admin port.

    `zoe.testing.ZoeServer` keeps only the line `listening ADDRESS` of the
    server. This class also keeps the line `admin ADDRESS` before it, so
    the test reads `/metrics`.
    """

    def __init__(self, path):
        self.path = path
        self.address = None
        self.admin = None
        self._process = None

    def start(self):
        command = [
            ZoeServer.binary(), "serve", "--path", str(self.path),
            "--listen", "127.0.0.1:0", "--admin-listen", "127.0.0.1:0",
        ]  # fmt: skip
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
        )
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, process.kill)
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
        code = process.wait()
        process.stdout.close()
        raise RuntimeError(f"zoe serve stopped with {code} before it listened")

    def kill(self):
        process, self._process = self._process, None
        if process is not None:
            process.kill()
            process.wait()
            process.stdout.close()

    def loads(self):
        """Return the number of `loadBefore` calls that reached the server."""
        url = f"http://{self.admin}/metrics"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
            text = reply.read().decode()
        for line in text.splitlines():
            if line.startswith('zoe_loads_total{storage="1"} '):
                return int(line.split()[-1])
        raise AssertionError(f"/metrics has no zoe_loads_total:\n{text}")


def counter(value):
    """A record of ZODB's PCounter, whose _p_resolveConflict adds."""
    meta = (("ZODB.tests.ConflictResolution", "PCounter"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def strict(value):
    """A record of ZODB's PCounter2, whose _p_resolveConflict refuses."""
    meta = (("ZODB.tests.ConflictResolution", "PCounter2"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"_value": value}, 3)


def value_of(data):
    unpickler = pickle.Unpickler(io.BytesIO(data))
    unpickler.load()
    return unpickler.load()["_value"]


def commit(storage, stores):
    """Commit the stores. Return the tid and the result of the vote.

    A failed step aborts the transaction and raises.
    """
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        for oid, serial, data in stores:
            storage.store(oid, serial, data, "", txn)
        resolved = storage.tpc_vote(txn)
        return storage.tpc_finish(txn), resolved
    except BaseException:
        storage.tpc_abort(txn)
        raise


def wait_for(condition):
    """Wait until condition() is true. Fail after TIMEOUT."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.01)


def snapshot(conn, manager):
    """Begin a new transaction of conn and return its root."""
    manager.begin()
    return conn.root()


@pytest.fixture
def server(tmp_path):
    server = Server(tmp_path / "store").start()
    yield server
    server.kill()


@pytest.fixture
def dbs(server):
    """Two DBs, each on its own native client."""
    import ZODB

    opened = []
    try:
        for _ in range(2):
            storage = zoe.testing.native_client(
                server.address, storage="1", wait_timeout=TIMEOUT
            )
            opened.append(ZODB.DB(storage))
        yield opened
    finally:
        for db in opened:
            db.close()


@pytest.fixture
def connections(dbs):
    """A connection of each DB, each with its own transaction manager."""
    import transaction

    pairs = []
    for db in dbs:
        manager = transaction.TransactionManager()
        pairs.append((db.open(transaction_manager=manager), manager))
    yield pairs
    for conn, manager in pairs:
        manager.abort()
        conn.close()


def test_inserts_of_two_clients_into_one_bucket_both_commit(dbs, connections):
    from BTrees.OOBTree import OOBTree

    with dbs[0].transaction() as conn:
        conn.root()["tree"] = OOBTree()
    (conn_a, tm_a), (conn_b, tm_b) = connections
    wait_for(lambda: "tree" in snapshot(conn_b, tm_b))

    # Both connections change the bucket from the same snapshot.
    snapshot(conn_a, tm_a)["tree"]["key-a"] = 1
    conn_b.root()["tree"]["key-b"] = 2
    tm_a.commit()
    # The server merges the bucket in the vote of b.
    tm_b.commit()

    def keys(conn, manager):
        return dict(snapshot(conn, manager)["tree"])

    expected = {"key-a": 1, "key-b": 2}
    wait_for(lambda: keys(conn_a, tm_a) == expected)
    wait_for(lambda: keys(conn_b, tm_b) == expected)


def test_two_increments_of_a_counter_both_count(dbs, connections):
    from ZODB.tests.ConflictResolution import PCounter

    counter = PCounter()
    counter.inc()
    with dbs[0].transaction() as conn:
        conn.root()["counter"] = counter
    (conn_a, tm_a), (conn_b, tm_b) = connections
    wait_for(lambda: "counter" in snapshot(conn_b, tm_b))

    snapshot(conn_a, tm_a)["counter"].inc()
    counter_b = conn_b.root()["counter"]
    counter_b.inc()
    tm_a.commit()
    tm_b.commit()

    # The vote of b returns the oid that it resolved, so the DB loads the
    # resolved state again.
    assert counter_b._value == 3
    wait_for(lambda: snapshot(conn_a, tm_a)["counter"]._value == 3)


def test_an_unresolvable_conflict_raises_and_drops_the_cache_entry(server, dbs):
    a, b = (db.storage for db in dbs)
    oid = a.new_oid()
    base, _ = commit(a, [(oid, ZERO, strict(1))])
    committed, _ = commit(a, [(oid, base, strict(2))])
    wait_for(lambda: b.lastTransaction() == committed)

    # Client b caches the current revision: a second load is a hit.
    assert b.load(oid) == (strict(2), committed)
    loads = server.loads()
    assert b.load(oid) == (strict(2), committed)
    assert server.loads() == loads

    # PCounter2 refuses the conflict.
    with pytest.raises(ConflictError) as caught:
        commit(b, [(oid, base, strict(3))])
    assert caught.value.oid == oid
    assert caught.value.serials == (committed, base)

    # The client dropped its cache entry of the oid.
    loads = server.loads()
    assert b.load(oid) == (strict(2), committed)
    assert server.loads() == loads + 1


def test_a_load_after_a_resolved_commit_gives_the_resolved_state(dbs):
    a, b = (db.storage for db in dbs)
    oid = a.new_oid()
    base, _ = commit(a, [(oid, ZERO, counter(1))])
    wait_for(lambda: b.lastTransaction() == base)
    assert b.load(oid) == (counter(1), base)
    commit(a, [(oid, base, counter(2))])

    # b read the base and adds 4. The resolved state adds both increments.
    tid, resolved = commit(b, [(oid, base, counter(5))])
    assert oid in (resolved or [])

    # The cache of b holds the resolved state, or nothing, but never the
    # state that b stored.
    data, serial = b.load(oid)
    assert serial == tid
    assert value_of(data) == 6
    wait_for(lambda: a.lastTransaction() == tid)
    data, serial = a.load(oid)
    assert serial == tid
    assert value_of(data) == 6
