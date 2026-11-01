"""Acceptance test of conflict resolution by the client, through the vote.

ZEO 6's own protocol lets the client resolve conflicts: when a vote meets a
stale serial that it can resolve, the reply of `vote` lists the conflict as
`{"oid": oid, "serials": (committed, read), "data": stored_pickle}`, where
`data` is the pickle that the client stored.
An unmodified `ZEO.ClientStorage` runs `tryToResolveConflict`, stores the
resolved state over the committed serial with `storea`, and votes again.
ZEO's server does this when `client-conflict-resolution` is on.

`zoe serve` has no Python, so this is how a stock client gets the
`_p_resolveConflict` of its classes: the core keeps the locks of the vote
(`vote_resolving`), returns the committed records, and checks every serial
again at the second vote with the same locks. A stale read-current serial
is never resolved. A conflict that the class does not resolve raises
`ConflictError` on the client.

The native client of `zoe.zeo` resolves in the client too. The resolution
of a counter runs through both kinds of client. After the resolved commit,
a load of the oid through either client gives the resolved state, not the
state that the client stored.
"""

import threading

import pytest
from support import TIMEOUT, ZERO, client, counter, native, strict, value_of, wait_for
from support import commit_resolving as commit


@pytest.fixture
def clients(server):
    opened = [client(server), client(server)]
    yield opened
    for storage in opened:
        storage.close()


@pytest.fixture(params=["zeo-client", "native-client"])
def resolvers(request, server):
    """Two clients of `server` of one kind: two stock clients, or two
    native clients, each under a DB of its own."""
    import ZODB

    if request.param == "zeo-client":
        opened = [client(server), client(server)]
        yield opened
        for storage in opened:
            storage.close()
        return
    dbs = []
    try:
        for _ in range(2):
            dbs.append(ZODB.DB(native(server)))
        yield [db.storage for db in dbs]
    finally:
        for db in dbs:
            db.close()


def test_the_client_resolves_a_counter_through_the_vote(resolvers):
    a, b = resolvers
    oid = a.new_oid()
    base, _ = commit(a, [(oid, ZERO, counter(1))])
    wait_for(lambda: b.lastTransaction() == base, "the base on the client b")
    assert b.load(oid) == (counter(1), base)
    commit(a, [(oid, base, counter(2))])

    # b read the base and adds 4: the vote lists the conflict, the client
    # resolves it from the pickle that it stored, and the commit holds both
    # additions. A resolution from the committed pickle would give 3.
    tid, resolved = commit(b, [(oid, base, counter(5))])
    assert oid in (resolved or [])

    # The cache of b holds the resolved state, or nothing, but never the
    # state that b stored.
    data, serial = b.load(oid)
    assert serial == tid
    assert value_of(data) == 6
    wait_for(lambda: a.lastTransaction() == tid, "the resolved commit on the client a")
    data, serial = a.load(oid)
    assert serial == tid
    assert value_of(data) == 6


def test_a_conflict_that_the_class_refuses_is_raised(clients):
    from ZODB.POSException import ConflictError

    a, b = clients
    oid = a.new_oid()
    base, _ = commit(a, [(oid, ZERO, strict(1))])
    commit(a, [(oid, base, strict(2))])
    with pytest.raises(ConflictError) as caught:
        commit(b, [(oid, base, strict(2))])
    assert caught.value.oid == oid


def test_a_read_current_conflict_is_never_resolved(clients):
    from ZODB.Connection import TransactionMetaData
    from ZODB.POSException import ReadConflictError

    a, b = clients
    read, written = a.new_oid(), a.new_oid()
    base, _ = commit(a, [(read, ZERO, counter(1)), (written, ZERO, counter(1))])
    commit(a, [(read, base, counter(2))])

    txn = TransactionMetaData()
    b.tpc_begin(txn)
    b.checkCurrentSerialInTransaction(read, base, txn)
    b.store(written, base, counter(2), "", txn)
    with pytest.raises(ReadConflictError):
        b.tpc_vote(txn)
    b.tpc_abort(txn)


def test_btrees_merge_concurrent_inserts_in_one_bucket(server):
    import transaction
    import ZODB
    from BTrees.Length import Length
    from BTrees.OOBTree import OOBTree

    db = ZODB.DB(client(server))
    try:
        with db.transaction() as conn:
            conn.root()["tree"] = OOBTree()
            conn.root()["count"] = Length()

        # Two connections, each in its own transaction manager, change the
        # same bucket and the same Length from the same snapshot.
        managers = [transaction.TransactionManager() for _ in range(2)]
        conns = [db.open(transaction_manager=tm) for tm in managers]
        for n, conn in enumerate(conns):
            conn.root()["tree"][f"key-{n}"] = n
            conn.root()["count"].change(1)
        barrier = threading.Barrier(2)
        errors = []

        def finish(tm):
            try:
                barrier.wait(TIMEOUT)
                tm.commit()
            except Exception as error:
                errors.append(error)

        threads = [threading.Thread(target=finish, args=(tm,)) for tm in managers]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
        assert errors == []
        for conn in conns:
            conn.close()

        with db.transaction() as conn:
            assert dict(conn.root()["tree"]) == {"key-0": 0, "key-1": 1}
            assert conn.root()["count"]() == 2
    finally:
        db.close()


def test_the_status_counts_resolved_conflicts(clients):
    a, b = clients
    oid = a.new_oid()
    base, _ = commit(a, [(oid, ZERO, counter(1))])
    commit(a, [(oid, base, counter(2))])
    commit(b, [(oid, base, counter(2))])
    assert a.server_status()["conflicts_resolved"] >= 1
