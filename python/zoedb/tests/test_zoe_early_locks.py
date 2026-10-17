"""Unit tests of the early write-intent locks of `zoe.Storage` and of
`zoe.zodb` (ADR 0045): the hot classes, the owners, the order of the locks,
and the end of a transaction that read a hot object."""

import threading

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.POSException import ConflictError, StorageTransactionError

import zoe
import zoe.zeo

HOT = "persistent.mapping.PersistentMapping"


def storage(tmp_path, **options):
    return zoe.Storage(
        str(tmp_path / "store"), early_locks=True, early_lock_classes=[HOT], **options
    )


def test_a_class_of_tier_1_or_a_bad_name_is_never_hot(tmp_path):
    for classes in (["BTrees.Length.Length"], ["BTrees.OOBTree.OOBucket"], ["Length"]):
        with pytest.raises(ValueError, match="early-lock-classes"):
            zoe.Storage(
                str(tmp_path / "store"), early_locks=True, early_lock_classes=classes
            )
    # Without early locks, a storage has no hot class.
    plain = zoe.Storage(str(tmp_path / "plain"))
    assert plain.early_lock_classes is None
    plain.close()
    # The asyncio client of ZEO takes no early locks.
    with pytest.raises(ValueError, match="native"):
        zoe.zeo.ClientStorage(("127.0.0.1", 1), native=False, early_locks=True)


def test_intent_locks_come_in_oid_order_and_the_next_tpc_begin_adopts_them(tmp_path):
    s = storage(tmp_path)
    try:
        db = ZODB.DB(s)
        with db.transaction() as conn:
            conn.root()["a"] = PersistentMapping()
            conn.root()["b"] = PersistentMapping()
        with db.transaction() as conn:
            a, b = conn.root()["a"]._p_oid, conn.root()["b"]._p_oid
        serials = s.lock_intent([b])
        assert serials == [s.load(b)[1]]
        with pytest.raises(StorageTransactionError):
            s.lock_intent([a])
        # A second owner waits for the lock of the default owner.
        other = s.new_intent_owner()
        got = threading.Event()

        def wait():
            s.lock_intent([b], other)
            got.set()

        waiter = threading.Thread(target=wait)
        waiter.start()
        assert not got.wait(0.2)
        s.unlock_intent()
        waiter.join(10)
        assert got.is_set()
        s.unlock_intent(other)
        db.close()
    finally:
        s.close()


def test_a_load_below_a_held_hot_object_takes_the_set_again_in_oid_order(tmp_path):
    db = ZODB.DB(storage(tmp_path))
    try:
        with db.transaction() as conn:
            conn.root()["a"] = PersistentMapping(n=0)
            conn.root()["b"] = PersistentMapping(n=0)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        root = conn.root()
        # The order of the loads: the root, then b, then a, whose oid is
        # below the oid of b. Nothing changed b meanwhile, so the load of a
        # goes on.
        root["b"]["n"] += 1
        root["a"]["n"] += 1
        assert root["b"]._p_oid > root["a"]._p_oid
        manager.commit()
        conn.close()
        with db.transaction() as conn:
            assert (conn.root()["a"]["n"], conn.root()["b"]["n"]) == (1, 1)
    finally:
        db.close()


def test_a_change_between_the_release_and_the_new_grant_ends_the_attempt(tmp_path):
    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        with db.transaction() as conn:
            conn.root()["a"] = PersistentMapping(n=0)
            conn.root()["b"] = PersistentMapping(n=0)
        lock_intent = s.lock_intent

        def changing(oids, owner=None):
            # Another transaction commits b after the release of the set.
            if len(oids) > 1:
                with db.transaction() as other:
                    other.root()["b"]["n"] = 10
            return lock_intent(oids, owner)

        s.lock_intent = changing
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        root = conn.root()
        root["b"]["n"] += 1
        with pytest.raises(ConflictError):
            root["a"]["n"] += 1
        del s.lock_intent
        manager.abort()
        # Zope closes the connection after the attempt, and the retry can
        # get another one.
        conn.close()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        root = conn.root()
        root["b"]["n"] += 1
        root["a"]["n"] += 1
        manager.commit()
        conn.close()
        with db.transaction() as conn:
            assert (conn.root()["a"]["n"], conn.root()["b"]["n"]) == (1, 11)
    finally:
        db.close()


def test_the_end_of_a_transaction_that_read_a_hot_object_releases_its_lock(tmp_path):
    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        with db.transaction() as conn:
            conn.root()["hot"] = PersistentMapping(n=0)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        hot = conn.root()["hot"]
        assert hot["n"] == 0
        oid = hot._p_oid
        # The reader holds the lock until the end of its transaction.
        other = s.new_intent_owner()
        holder = threading.Thread(target=lambda: s.lock_intent([oid], other))
        holder.start()
        holder.join(0.2)
        assert holder.is_alive()
        manager.commit()
        holder.join(10)
        assert not holder.is_alive()
        s.unlock_intent(other)
        conn.close()
    finally:
        db.close()


def test_the_config_keys_give_the_hot_classes(tmp_path, monkeypatch):
    import ZODB.config

    text = f"""
    %import zoe
    <zodb>
      <zoe>
        path {tmp_path / "store"}
        early-locks on
        early-lock-classes {HOT} zope.interface.Interface
      </zoe>
    </zodb>
    """
    db = ZODB.config.databaseFromString(text)
    try:
        assert db.storage.early_lock_classes == {HOT, "zope.interface.Interface"}
    finally:
        db.close()
    monkeypatch.setenv("ZOE_EARLY_LOCKS", "on")
    monkeypatch.setenv("ZOE_EARLY_LOCK_CLASSES", f"{HOT}, x.Y")
    other = tmp_path / "other"
    text = f"%import zoe\n<zodb>\n  <zoe>\n    path {other}\n  </zoe>\n</zodb>\n"
    db = ZODB.config.databaseFromString(text)
    try:
        assert db.storage.early_lock_classes == {HOT, "x.Y"}
    finally:
        db.close()


def hot_and_other(db):
    with db.transaction() as conn:
        conn.root()["hot"] = PersistentMapping(n=0)
        conn.root()["other"] = PersistentMapping(n=0)


def waits_for(s, oid, seconds=0.3):
    """Tell if another owner waits longer than `seconds` for the lock of
    oid. The other owner takes the lock at the end and keeps it: the caller
    releases it with the returned owner."""
    other = s.new_intent_owner()
    got = threading.Event()

    def take():
        s.lock_intent([oid], other)
        got.set()

    taker = threading.Thread(target=take, daemon=True)
    taker.start()
    waited = not got.wait(seconds)
    return waited, other, taker


def test_a_savepoint_keeps_the_change_of_a_hot_object_that_turns_into_a_ghost(tmp_path):
    db = ZODB.DB(storage(tmp_path))
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        hot = conn.root()["hot"]
        hot["n"] = 1
        manager.savepoint()
        # The garbage collection of the cache can do this after a savepoint.
        hot._p_deactivate()
        assert hot._p_changed is None
        seen = hot["n"]
        hot["n"] = seen + 1
        manager.commit()
        conn.close()
        with db.transaction() as conn:
            assert (seen, conn.root()["hot"]["n"]) == (1, 2)
    finally:
        db.close()


def test_the_rollback_of_a_savepoint_gives_the_state_of_the_savepoint(tmp_path):
    db = ZODB.DB(storage(tmp_path))
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        hot = conn.root()["hot"]
        hot["n"] = 1
        savepoint = manager.savepoint()
        hot["n"] = 5
        savepoint.rollback()
        assert hot["n"] == 1
        manager.abort()
        conn.close()
    finally:
        db.close()


def test_a_historical_connection_reads_the_old_state_without_a_lock(tmp_path):
    from ZODB.utils import p64, u64

    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        hot_and_other(db)
        with db.transaction() as conn:
            hot = conn.root()["hot"]
            assert hot["n"] == 0
            before = p64(u64(hot._p_serial) + 1)
            oid = hot._p_oid
        with db.transaction() as conn:
            conn.root()["hot"]["n"] = 7
        # Another owner holds the lock of the root and of the hot object.
        s.lock_intent([p64(0), oid])
        historical = db.open(before=before)
        assert historical.root()["hot"]["n"] == 0
        historical.close()
        s.unlock_intent()
    finally:
        db.close()


def test_a_read_only_storage_has_no_hot_class(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    hot_and_other(db)
    db.close()
    s = storage(tmp_path, read_only=True)
    assert s.early_lock_classes is None
    db = ZODB.DB(s)
    try:
        conn = db.open()
        assert conn.root()["hot"]["n"] == 0
        conn.close()
    finally:
        db.close()


def test_an_explicit_transaction_manager_releases_the_locks_at_the_end(tmp_path):
    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager(explicit=True)
        conn = db.open(transaction_manager=manager)
        with manager:
            oid = conn.root()["hot"]._p_oid
            assert conn.root()["hot"]["n"] == 0
        waited, other, taker = waits_for(s, oid)
        taker.join(10)
        s.unlock_intent(other)
        conn.close()
        assert not waited
    finally:
        db.close()


def test_a_close_turns_the_hot_objects_into_ghosts_for_the_next_lock(tmp_path):
    from ZODB.utils import p64

    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        hot = conn.root()["hot"]
        oid = hot._p_oid
        assert hot["n"] == 0
        # A read-only use closes the connection without an end of the
        # transaction.
        conn.close()
        assert hot._p_changed is None
        again = db.open(transaction_manager=manager)
        assert again is conn
        manager.begin()
        # Another owner holds the locks now: the next read waits for them.
        other = s.new_intent_owner()
        s.lock_intent([p64(0), oid], other)
        read = threading.Event()

        def load():
            assert again.root()["hot"]["n"] == 0
            read.set()

        reader = threading.Thread(target=load, daemon=True)
        reader.start()
        waited = not read.wait(0.3)
        s.unlock_intent(other)
        reader.join(10)
        manager.abort()
        again.close()
        assert waited
    finally:
        db.close()


def test_the_close_of_a_joined_connection_keeps_its_locks(tmp_path):
    from ZODB.POSException import ConnectionStateError

    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        hot = conn.root()["hot"]
        hot["n"] = 3
        with pytest.raises(ConnectionStateError):
            conn.close()
        waited, other, taker = waits_for(s, hot._p_oid)
        manager.abort()
        taker.join(10)
        s.unlock_intent(other)
        conn.close()
        assert waited
    finally:
        db.close()


def test_a_release_that_fails_leaves_the_connection_clean(tmp_path, caplog):
    s = storage(tmp_path)
    db = ZODB.DB(s)
    try:
        hot_and_other(db)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        assert conn.root()["hot"]["n"] == 0
        intents = conn._zoe_intents

        def failing(owner=None):
            raise RuntimeError("the release failed")

        s.unlock_intent = failing
        with caplog.at_level("WARNING", logger="zoe.zodb"):
            manager.abort()
        del s.unlock_intent
        assert intents.held == {}
        assert not intents.cached
        assert "release of the intent locks" in caplog.text
        s.unlock_intent(intents.owner)
        conn.close()
    finally:
        db.close()


def test_a_connection_that_took_no_lock_leaves_the_locks_of_the_default_owner(
    tmp_path,
):
    from ZODB.tests.MinPO import MinPO

    s = zoe.Storage(
        str(tmp_path / "store"),
        early_locks=True,
        early_lock_classes=["ZODB.tests.MinPO.MinPO"],
    )
    db = ZODB.DB(s)
    try:
        with db.transaction() as conn:
            conn.root()["hot"] = MinPO(0)
            conn.root()["cold"] = PersistentMapping(n=0)
        with db.transaction() as conn:
            oid = conn.root()["hot"]._p_oid
        s.lock_intent([oid])
        # The connection loads no hot object, and commits.
        with db.transaction() as conn:
            conn.root()["cold"]["n"] = 1
        # Its tpc_begin did not adopt the locks of the default owner.
        waited, other, taker = waits_for(s, oid)
        s.unlock_intent()
        taker.join(10)
        s.unlock_intent(other)
        assert waited
    finally:
        db.close()


def test_install_keeps_a_patch_that_another_package_made_before(tmp_path, monkeypatch):
    from ZODB.Connection import Connection

    from zoe import zodb

    # Start from the methods that the first install found.
    for name in zodb._NAMES:
        found = zodb._replaced[name] if zodb._installed else getattr(Connection, name)
        monkeypatch.setattr(Connection, name, found)
    monkeypatch.setattr(zodb, "_installed", False)
    monkeypatch.setattr(zodb, "_replaced", dict(zodb._replaced))
    calls = []
    stock = Connection.newTransaction

    def patched(self, transaction, sync=True):
        calls.append(self)
        return stock(self, transaction, sync)

    monkeypatch.setattr(Connection, "newTransaction", patched)
    db = ZODB.DB(storage(tmp_path))
    try:
        assert Connection.newTransaction is zodb.LockingConnection.newTransaction
        with db.transaction() as conn:
            conn.root()["hot"] = PersistentMapping(n=0)
        assert calls
    finally:
        db.close()


def test_a_server_without_lock_intent_gives_no_hot_class_and_one_warning(
    tmp_path, caplog
):
    import ZEO

    address, stop = ZEO.server(
        path=str(tmp_path / "Data.fs"), zeo_conf={"msgpack": True}
    )
    try:
        with caplog.at_level("WARNING", logger="zoe.zeo"):
            s = zoe.zeo.ClientStorage(
                address, wait_timeout=30, early_locks=True, early_lock_classes=[HOT]
            )
        db = ZODB.DB(s)
        try:
            assert type(s) is zoe.zeo._NativeClientStorage
            assert s.early_lock_classes is None
            with db.transaction() as conn:
                conn.root()["hot"] = PersistentMapping(n=1)
            with db.transaction() as conn:
                assert conn.root()["hot"]["n"] == 1
        finally:
            db.close()
    finally:
        stop()
    warnings = [r for r in caplog.records if "lock_intent" in r.getMessage()]
    assert len(warnings) == 1


def test_the_client_of_zeo_inside_takes_no_early_lock(tmp_path, caplog):
    import ZEO

    address, stop = ZEO.server(path=str(tmp_path / "Data.fs"))
    try:
        with caplog.at_level("WARNING", logger="zoe.zeo"):
            s = zoe.zeo.ClientStorage(
                address, wait_timeout=30, early_locks=True, early_lock_classes=[HOT]
            )
        db = ZODB.DB(s)
        try:
            assert type(s) is zoe.zeo.ClientStorage
            assert s.early_lock_classes is None
            with db.transaction() as conn:
                conn.root()["hot"] = PersistentMapping(n=1)
        finally:
            db.close()
    finally:
        stop()
    assert "takes no early lock" in caplog.text


def test_a_read_only_native_client_has_no_hot_class(tmp_path):
    from zoe.testing import ZoeServer

    with ZoeServer(str(tmp_path / "store")) as server:
        db = ZODB.DB(zoe.zeo.ClientStorage(server.address, wait_timeout=30))
        hot_and_other(db)
        db.close()
        s = zoe.zeo.ClientStorage(
            server.address,
            wait_timeout=30,
            read_only=True,
            early_locks=True,
            early_lock_classes=[HOT],
        )
        db = ZODB.DB(s)
        try:
            assert s.early_lock_classes is None
            conn = db.open()
            assert conn.root()["hot"]["n"] == 0
            conn.close()
        finally:
            db.close()


def test_an_owner_whose_first_lock_fails_gives_its_connection_back(
    tmp_path, monkeypatch
):
    from zoe.testing import ZoeServer

    # Two connections commit at once: the holder takes one, the owner whose
    # lock fails takes the other until the failure.
    monkeypatch.setattr(zoe.zeo, "_POOL_BOUND", 2)
    with ZoeServer(str(tmp_path / "store"), args=["--lock-timeout", "1"]) as server:
        s = zoe.zeo.ClientStorage(server.address, wait_timeout=30)
        db = ZODB.DB(s)
        try:
            hot_and_other(db)
            with db.transaction() as conn:
                oid = conn.root()["hot"]._p_oid
            # The holder is not the default owner: the commit below begins
            # on a connection of its own.
            holder = s.new_intent_owner()
            s.lock_intent([oid], holder)
            owner = s.new_intent_owner()
            with pytest.raises(ConflictError):
                s.lock_intent([oid], owner)
            committed = threading.Event()

            def commit():
                with db.transaction() as conn:
                    conn.root()["other"]["n"] = 1
                committed.set()

            committer = threading.Thread(target=commit, daemon=True)
            committer.start()
            done = committed.wait(10)
            s.unlock_intent(holder)
            committer.join(10)
            assert done, "a commit waited for the connection of a failed owner"
        finally:
            db.close()
