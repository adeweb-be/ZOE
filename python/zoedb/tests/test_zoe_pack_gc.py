"""Acceptance test: the key `pack-gc` of a store, and `IExternalGC`.

A store packs with garbage collection by default (F8). With `pack-gc`
false, a pack removes old revisions only, as the `pack-gc` of FileStorage
and RelStorage does: for each object, it keeps the newest record at or
before the pack tid, also when no reader can reach the object. An object
whose newest record at or before the pack tid is a deletion
(`deleteObject`) goes, with each of its records at or before the pack tid,
as in FileStorage. The records after the pack tid stay, as always.

`zoe.Storage(path, pack_gc=False)` and the key `pack-gc` of the `<zoe>`
section set the switch. It holds for the open storage; the store records
nothing.

`zoe.Storage` declares `IExternalGC`, because it has `deleteObject`.
"""

import time

import pytest
import transaction
import ZODB
import ZODB.config
from persistent.mapping import PersistentMapping
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import POSKeyError

import zoe


def stanza(path, *lines):
    body = "".join(f"  {line}\n" for line in (f"path {path}", *lines))
    return f"%import zoe\n<zoe>\n{body}</zoe>\n"


def unreachable_object(db):
    """Commit an object in two revisions, then unlink it from the root.

    Return its oid and the tids of its two revisions.
    """
    tm = transaction.TransactionManager()
    conn = db.open(transaction_manager=tm)
    try:
        conn.root()["item"] = PersistentMapping(value=1)
        tm.commit()
        item = conn.root()["item"]
        oid, first = item._p_oid, item._p_serial
        item["value"] = 2
        tm.commit()
        second = item._p_serial
        del conn.root()["item"]
        tm.commit()
    finally:
        conn.close()
    return oid, first, second


def pack(db):
    db.pack(time.time() + 1)


def test_the_storage_declares_iexternalgc(tmp_path):
    from ZODB.interfaces import IExternalGC
    from zope.interface.verify import verifyObject

    storage = zoe.Storage(str(tmp_path / "store"))
    try:
        assert IExternalGC.providedBy(storage)
        assert verifyObject(IExternalGC, storage)
    finally:
        storage.close()


def test_a_pack_collects_garbage_by_default(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        oid, _, _ = unreachable_object(db)
        pack(db)
        with pytest.raises(POSKeyError):
            db.storage.load(oid)
    finally:
        db.close()


def test_a_pack_without_gc_keeps_an_unreachable_object(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store"), pack_gc=False))
    try:
        oid, first, second = unreachable_object(db)
        pack(db)
        # The newest revision stays, and the older one goes.
        assert db.storage.load(oid)[1] == second
        with pytest.raises(POSKeyError):
            db.storage.loadSerial(oid, first)
    finally:
        db.close()


def test_a_pack_without_gc_removes_a_deleted_object(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store"), pack_gc=False))
    try:
        oid, first, second = unreachable_object(db)
        storage = db.storage
        txn = TransactionMetaData()
        storage.tpc_begin(txn)
        storage.deleteObject(oid, second, txn)
        storage.tpc_vote(txn)
        deleted = storage.tpc_finish(txn)
        # Before the pack, a reader before the deletion still sees it.
        assert storage.loadBefore(oid, deleted)[1] == second
        pack(db)
        with pytest.raises(POSKeyError):
            storage.load(oid)
        with pytest.raises(POSKeyError):
            storage.loadBefore(oid, deleted)
        with pytest.raises(POSKeyError):
            storage.loadSerial(oid, second)
    finally:
        db.close()


def test_the_key_pack_gc_of_the_zoe_section(tmp_path):
    path = tmp_path / "store"
    storage = ZODB.config.storageFromString(stanza(path, "pack-gc false"))
    db = ZODB.DB(storage)
    try:
        oid, _, second = unreachable_object(db)
        pack(db)
        assert db.storage.load(oid)[1] == second
    finally:
        db.close()

    # The store records nothing: a default open collects garbage again.
    db = ZODB.DB(ZODB.config.storageFromString(stanza(path)))
    try:
        with db.transaction() as conn:
            conn.root()["later"] = 1
        pack(db)
        with pytest.raises(POSKeyError):
            db.storage.load(oid)
    finally:
        db.close()
