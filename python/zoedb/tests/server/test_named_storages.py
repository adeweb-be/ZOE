"""Acceptance test of named storages in one `zoe serve` process.

`zoe serve --storage main=PATH --storage catalog=PATH` serves two stores.
A client selects one with the storage name of `register`, as the `storage`
argument of `ZEO.ClientStorage` and the `name` key of `<zeoclient>` give
it. Each store has its own data, its own lock manager and its own
invalidations: a client of one store never sees the commits of the other,
and a commit on one store never waits for a lock of the other. An unknown
name is refused at registration.
"""

import pickle
import threading

import pytest

from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def client(server, name, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage=name, **kw)


def commit(storage, stores):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    for oid, serial, data in stores:
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


@pytest.fixture
def server(tmp_path):
    args = [
        "--storage",
        f"main={tmp_path / 'main'}",
        "--storage",
        f"catalog={tmp_path / 'catalog'}",
    ]
    with ZoeServer(args=args) as server:
        yield server


def test_each_name_selects_its_own_store(server):
    from ZODB.POSException import POSKeyError

    main, catalog = client(server, "main"), client(server, "catalog")
    try:
        oid = main.new_oid()
        tid = commit(main, [(oid, ZERO, record("main"))])
        assert main.load(oid) == (record("main"), tid)
        with pytest.raises(POSKeyError):
            catalog.load(oid)
        assert catalog.lastTransaction() == ZERO
    finally:
        main.close()
        catalog.close()


def test_a_database_per_store_like_a_mount_point(server):
    import ZODB
    from persistent.mapping import PersistentMapping

    databases = {}
    main = ZODB.DB(client(server, "main"), database_name="main", databases=databases)
    catalog = ZODB.DB(
        client(server, "catalog"), database_name="catalog", databases=databases
    )
    try:
        with main.transaction() as conn:
            other = conn.get_connection("catalog")
            other.root()["index"] = PersistentMapping(kind="catalog")
            conn.root()["site"] = PersistentMapping(kind="site")
            conn.root()["site"]["index"] = other.root()["index"]
        with main.transaction() as conn:
            assert conn.root()["site"]["index"]["kind"] == "catalog"
            assert conn.root()["site"]["index"]._p_jar.db().database_name == "catalog"
    finally:
        main.close()
        catalog.close()


def test_a_commit_on_one_store_never_waits_for_the_other(server):
    from ZODB.Connection import TransactionMetaData

    main, catalog = client(server, "main"), client(server, "catalog")
    try:
        oid = main.new_oid()
        assert catalog.new_oid() == oid
        first_main = commit(main, [(oid, ZERO, record("m"))])
        first_catalog = commit(catalog, [(oid, ZERO, record("c"))])

        # main votes and keeps the lock of its oid.
        held = TransactionMetaData()
        main.tpc_begin(held)
        main.store(oid, first_main, record("m2"), "", held)
        main.tpc_vote(held)

        # The same oid number in catalog does not wait for it.
        done = []
        thread = threading.Thread(
            target=lambda: done.append(
                commit(catalog, [(oid, first_catalog, record("c2"))])
            ),
            daemon=True,
        )
        thread.start()
        thread.join(TIMEOUT)
        assert done, "a commit on catalog waited for a lock of main"
        main.tpc_finish(held)
    finally:
        main.close()
        catalog.close()


def test_invalidations_stay_within_their_store(server):
    main, catalog = client(server, "main"), client(server, "catalog")
    try:
        last = commit(catalog, [(catalog.new_oid(), ZERO, record("c"))])
        commit(main, [(main.new_oid(), ZERO, record("m"))])
        tid, oids = catalog._call("getInvalidations", last)
        assert tid == last
        assert list(oids) == []
        assert catalog.lastTransaction() == last
    finally:
        main.close()
        catalog.close()


def test_an_unknown_name_is_refused(server):
    from ZEO.Exceptions import ClientDisconnected

    with pytest.raises(ClientDisconnected):
        client(server, "nothing-here", wait_timeout=2)
