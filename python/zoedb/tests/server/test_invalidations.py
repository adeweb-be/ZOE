"""Acceptance test of invalidations through `zoe serve`.

After each commit, every other client of the storage gets
`invalidateTransaction(tid, oids)`, so a `ZODB.DB` on an unmodified
`ZEO.ClientStorage` sees the commits of the other clients. A client that
reconnects asks `getInvalidations(tid)` for what changed while it was away,
and the server answers from the transaction log of the store, so the
answer also holds after a restart of the server.
"""

import time

import pytest

import zoe
from zoe.testing import ZoeServer

TIMEOUT = 30


def client(server, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", **kw)


def wait_for(condition):
    """Wait until condition() is true. Fail after TIMEOUT."""
    deadline = time.monotonic() + TIMEOUT
    while not condition():
        assert time.monotonic() < deadline, "timed out"
        time.sleep(0.01)


@pytest.fixture
def path(tmp_path):
    return str(tmp_path / "store")


def test_a_connection_sees_the_commit_of_another_client(path):
    import transaction
    import ZODB
    from persistent.mapping import PersistentMapping

    with ZoeServer(path) as server:
        writer = ZODB.DB(client(server))
        reader = ZODB.DB(client(server))
        try:
            with writer.transaction() as conn:
                conn.root()["item"] = PersistentMapping(value=1)

            tm = transaction.TransactionManager()
            conn = reader.open(transaction_manager=tm)

            def value():
                tm.begin()
                item = conn.root().get("item")
                return None if item is None else item["value"]

            wait_for(lambda: value() == 1)

            with writer.transaction() as wconn:
                wconn.root()["item"]["value"] = 2

            # The reader's cached object is invalidated: a new transaction
            # sees the new value.
            wait_for(lambda: value() == 2)
            tm.abort()
            conn.close()
        finally:
            reader.close()
            writer.close()


def test_a_client_catches_up_after_a_restart_of_the_server(path):
    """The client reconnects and asks getInvalidations for what it missed."""
    import transaction
    import ZODB
    from persistent.mapping import PersistentMapping

    with ZoeServer(path) as server:
        db = ZODB.DB(client(server, min_disconnect_poll=0.1, max_disconnect_poll=0.5))
        try:
            with db.transaction() as conn:
                conn.root()["item"] = PersistentMapping(value="before")

            tm = transaction.TransactionManager()
            conn = db.open(transaction_manager=tm)
            assert conn.root()["item"]["value"] == "before"
            tm.abort()

            # While the server is down, a commit changes the item in-process.
            server.kill()
            local = ZODB.DB(zoe.Storage(path))
            with local.transaction() as lconn:
                lconn.root()["item"]["value"] = "while away"
            local.close()
            server.start()

            def value():
                tm.begin()
                try:
                    return conn.root()["item"]["value"]
                except Exception:
                    return None

            wait_for(lambda: value() == "while away")
            tm.abort()
            conn.close()
        finally:
            db.close()


def test_get_invalidations_answers_from_the_transaction_log(path):
    from ZODB.Connection import TransactionMetaData

    def commit(storage, oids, serials):
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        for oid in oids:
            storage.store(oid, serials.get(oid, bytes(8)), b"x" * 10, "", txn)
        storage.tpc_vote(txn)
        tid = storage.tpc_finish(txn)
        for oid in oids:
            serials[oid] = tid
        return tid

    with ZoeServer(path) as server:
        storage = client(server)
        serials = {}
        try:
            a, b, c = (storage.new_oid() for _ in range(3))
            first = commit(storage, [a], serials)
            commit(storage, [b], serials)
            last = commit(storage, [a, c], serials)
        finally:
            storage.close()

        server.kill()
        server.start()
        storage = client(server)
        try:
            tid, oids = storage._call("getInvalidations", first)
            assert tid == last
            assert sorted(oids) == sorted([a, b, c])
            tid, oids = storage._call("getInvalidations", last)
            assert tid == last
            assert list(oids) == []
        finally:
            storage.close()
