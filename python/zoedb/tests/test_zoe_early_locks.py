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


def test_a_load_below_a_held_hot_object_ends_the_attempt_and_the_retry_passes(tmp_path):
    db = ZODB.DB(storage(tmp_path))
    try:
        with db.transaction() as conn:
            conn.root()["a"] = PersistentMapping(n=0)
            conn.root()["b"] = PersistentMapping(n=0)
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        manager.begin()
        root = conn.root()
        a_oid = root["a"]._p_oid
        # The order of the loads: the root, then b, then a, whose oid is
        # below the oid of b.
        root["b"]["n"] += 1
        assert root["b"]._p_oid > a_oid
        with pytest.raises(ConflictError):
            root["a"]["n"] += 1
        manager.abort()
        # The next attempt takes the hot oids of the failed one in oid
        # order at its first load of a hot object.
        manager.begin()
        root = conn.root()
        root["b"]["n"] += 1
        root["a"]["n"] += 1
        manager.commit()
        conn.close()
        with db.transaction() as conn:
            assert (conn.root()["a"]["n"], conn.root()["b"]["n"]) == (1, 1)
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
    text = f"%import zoe\n<zodb>\n  <zoe>\n    path {tmp_path / 'other'}\n  </zoe>\n</zodb>\n"
    db = ZODB.config.databaseFromString(text)
    try:
        assert db.storage.early_lock_classes == {HOT, "x.Y"}
    finally:
        db.close()
