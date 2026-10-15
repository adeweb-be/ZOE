"""Acceptance test: `len(zoe.Storage)` is the object count of the store.

The store keeps an exact count of its objects: the oids that have a
record, a deletion included, until a pack removes every record of the oid,
as `len()` of FileStorage. `len(zoe.Storage)` gives it. A new object counts
at its commit, a new revision does not, a deletion counts until a pack
removes the object, and a pack that removes an unreachable object lowers
the count.
"""

import time

import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.Connection import TransactionMetaData

import zoe


def test_len_counts_the_objects(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    assert len(storage) == 0
    db = ZODB.DB(storage)
    try:
        # The database commits the root.
        assert len(storage) == 1
        tm = transaction.TransactionManager()
        conn = db.open(transaction_manager=tm)
        conn.root()["a"] = PersistentMapping(value=1)
        conn.root()["b"] = PersistentMapping(value=1)
        tm.commit()
        assert len(storage) == 3

        # New revisions are no new objects.
        conn.root()["a"]["value"] = 2
        tm.commit()
        assert len(storage) == 3

        # An unreachable object counts until a pack removes it.
        del conn.root()["b"]
        tm.commit()
        assert len(storage) == 3
        conn.close()
        db.pack(time.time() + 1)
        assert len(storage) == 2
    finally:
        db.close()


def test_a_deleted_object_counts_until_the_pack(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store"), pack_gc=False))
    try:
        tm = transaction.TransactionManager()
        conn = db.open(transaction_manager=tm)
        conn.root()["a"] = PersistentMapping(value=1)
        tm.commit()
        oid, serial = conn.root()["a"]._p_oid, conn.root()["a"]._p_serial
        del conn.root()["a"]
        tm.commit()
        conn.close()
        storage = db.storage
        assert len(storage) == 2

        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.deleteObject(oid, serial, txn)
        storage.tpc_vote(txn)
        storage.tpc_finish(txn)
        # The deletion is a record: the object still counts.
        assert len(storage) == 2
        db.pack(time.time() + 1)
        assert len(storage) == 1
    finally:
        db.close()


def test_the_count_survives_a_reopen(tmp_path):
    path = str(tmp_path / "store")
    db = ZODB.DB(zoe.Storage(path))
    try:
        with db.transaction() as conn:
            for name in "abcde":
                conn.root()[name] = PersistentMapping()
    finally:
        db.close()
    storage = zoe.Storage(path)
    try:
        assert len(storage) == 6
    finally:
        storage.close()
