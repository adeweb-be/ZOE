"""The transaction overlay of the catalog backend of `zoe.plone` (ADR 0059,
section 8) without Plone: the hook pickles copies of the states and changes
no object, and it stops at the cap of a call before it pickles more.
"""

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from persistent.wref import WeakRef
from ZODB.serialize import ObjectWriter

from zoe.plone import catalog


@pytest.fixture
def db():
    db = ZODB.DB(None)
    yield db
    db.close()


@pytest.fixture
def conn(db):
    manager = transaction.TransactionManager()
    conn = db.open(transaction_manager=manager)
    yield conn
    manager.abort()
    conn.close()


@pytest.fixture
def pickles(monkeypatch):
    """The number of pickles that the writers make."""
    made = []
    original = ObjectWriter.serialize

    def counting(self, obj):
        made.append(obj)
        return original(self, obj)

    monkeypatch.setattr(ObjectWriter, "serialize", counting)
    return made


def provisional(records):
    return [oid for oid, _ in records if oid[0] & 0x80]


def test_a_weak_reference_to_a_new_object_gets_a_provisional_oid_and_commits(db, conn):
    root = conn.root()
    root["a"] = PersistentMapping()
    conn.transaction_manager.commit()
    new = PersistentMapping({"x": 1})
    ref = WeakRef(new)
    root["a"]["ref"] = ref
    root["a"]["strong"] = new
    records = catalog._overlay(conn)
    # The new object and the weak reference stay as they were.
    assert new._p_oid is None
    assert new._p_jar is None
    assert ref.oid is None
    assert len(provisional(records)) == 1
    conn.transaction_manager.commit()
    other = db.open()
    try:
        assert dict(other.root()["a"]["strong"]) == {"x": 1}
        assert other.root()["a"]["ref"]() == {"x": 1}
    finally:
        other.close()


def test_a_new_object_reached_twice_has_one_provisional_oid(conn):
    root = conn.root()
    new = PersistentMapping()
    root["one"] = new
    root["two"] = PersistentMapping({"same": new})
    records = catalog._overlay(conn)
    oids = provisional(records)
    assert len(oids) == 2
    assert new._p_oid is None


def test_the_count_of_the_records_comes_before_any_pickle(conn, pickles, monkeypatch):
    root = conn.root()
    for i in range(500):
        root[i] = PersistentMapping({"i": i})
    conn.transaction_manager.commit()
    for i in range(500):
        root[i]["i"] = -i
    pickles.clear()
    monkeypatch.setattr(catalog, "_CAP_VALUES", 100)
    with pytest.raises(catalog._TooLarge):
        catalog._overlay(conn)
    assert pickles == []


def test_the_pickles_stop_at_the_bytes_of_the_cap(conn, pickles, monkeypatch):
    root = conn.root()
    for i in range(500):
        root[i] = PersistentMapping({"title": "x" * 200})
    monkeypatch.setattr(catalog, "_CAP_BYTES", 10_000)
    with pytest.raises(catalog._TooLarge):
        catalog._overlay(conn)
    # The registered root and a few of the new objects, not all of them.
    assert len(pickles) < 100


def test_an_overlay_within_the_cap_holds_each_record(conn):
    root = conn.root()
    conn.transaction_manager.commit()
    root["a"] = PersistentMapping({"x": 1})
    conn.transaction_manager.savepoint()
    root["b"] = PersistentMapping({"y": 2})
    records = catalog._overlay(conn)
    oids = [oid for oid, _ in records]
    assert len(oids) == len(set(oids))
    # The root, `b` as a new object, and `a` from the savepoint.
    assert len(records) == 3
    assert len(provisional(records)) == 1
