"""Unit tests of the undo of `zoe.Storage`: the undo ids, the undo log and
its filters, and the errors of an undo."""

import base64

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.POSException import UndoError

import zoe
from zoe.storage import undo_id


@pytest.fixture
def db(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    yield db
    db.close()


def commit(db, description, change, **extension):
    manager = transaction.TransactionManager()
    conn = db.open(transaction_manager=manager)
    try:
        with manager as txn:
            txn.note(description)
            for key, value in extension.items():
                txn.setExtendedInfo(key, value)
            change(conn.root())
    finally:
        conn.close()


def test_an_undo_id_is_the_base64_text_of_the_tid(db):
    commit(db, "one", lambda root: root.__setitem__("a", 1))
    (entry,) = db.undoLog(0, 1)
    tid = db.lastTransaction()
    assert entry["id"] == undo_id(tid) == base64.encodebytes(tid).rstrip()


def test_the_undo_log_holds_the_extension_and_filters_before_it_counts(db):
    for n in range(5):
        commit(db, f"t{n}", lambda root, n=n: root.__setitem__("a", n), kind=n % 2)
    log = db.undoLog(0, 3)
    assert [entry["description"] for entry in log] == ["t4", "t3", "t2"]
    assert log[0]["kind"] == 0
    odd = db.storage.undoLog(0, 2, lambda entry: entry.get("kind") == 1)
    assert [entry["description"] for entry in odd] == [b"t3", b"t1"]
    info = db.undoInfo(1, 3, {"kind": 0})
    assert [entry["description"] for entry in info] == ["t2", "t0"]


def test_an_id_that_names_no_transaction_raises_undo_error(db):
    commit(db, "one", lambda root: root.__setitem__("a", 1))
    for transaction_id in (b"not an id", undo_id(b"\0" * 7 + b"\1")):
        manager = transaction.TransactionManager()
        manager.begin()
        db.undo(transaction_id, manager.get())
        with pytest.raises(UndoError):
            manager.commit()
        manager.abort()


def test_an_undo_that_python_cannot_merge_raises_undo_error(db):
    commit(db, "create", lambda root: root.__setitem__("c", PersistentMapping(v=1)))
    commit(db, "first", lambda root: root["c"].__setitem__("v", 2))
    commit(db, "second", lambda root: root["c"].__setitem__("v", 3))
    first = [e["id"] for e in db.undoLog(0, 3) if e["description"] == "first"]
    manager = transaction.TransactionManager()
    manager.begin()
    db.undo(first[0], manager.get())
    with pytest.raises(UndoError):
        manager.commit()
    manager.abort()
    with db.transaction() as conn:
        assert conn.root()["c"]["v"] == 3
