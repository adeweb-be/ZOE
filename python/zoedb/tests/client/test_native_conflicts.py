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

`test_conflict_resolution_over_zeo.py` checks, for the native client and
for a stock client, that after a resolved commit a load of the oid gives
the resolved state, not the state that the client stored.

The test counts the loads that reach the server with `zoe_loads_total` of
`GET /metrics` on the admin port of `zoe serve`.
"""

import pytest
from client_support import AdminServer
from support import ZERO, native, strict, wait_for
from support import commit_resolving as commit
from ZODB.POSException import ConflictError


def snapshot(conn, manager):
    """Begin a new transaction of conn and return its root."""
    manager.begin()
    return conn.root()


@pytest.fixture
def server(tmp_path):
    with AdminServer(tmp_path / "store") as server:
        yield server


@pytest.fixture
def dbs(server):
    """Two DBs, each on its own native client."""
    import ZODB

    opened = []
    try:
        for _ in range(2):
            opened.append(ZODB.DB(native(server)))
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
