"""The reader in Rust of the spike ZOE-274 (`zoe.reader`).

For each record, the state of `zoe.reader.Reader` pickles to the bytes of
the state of ZODB's `ObjectReader`, with each persistent object as its oid
and its class, or the record takes the Python path for a named reason.
"""

import collections
import io

import persistent
import pytest
import transaction
import ZODB
from persistent.list import PersistentList
from persistent.mapping import PersistentMapping
from ZODB._compat import PersistentPickler
from ZODB.MappingStorage import MappingStorage
from ZODB.serialize import ObjectReader

import zoe.reader


class Slotted:
    __slots__ = ("a", "b")

    def __init__(self, a, b):
        self.a = a
        self.b = b

    def __eq__(self, other):
        return (self.a, self.b) == (other.a, other.b)


class Plain:
    def __init__(self, **kw):
        self.__dict__.update(kw)


class WithState:
    def __init__(self, value):
        self.value = value

    def __getstate__(self):
        return {"v": self.value}

    def __setstate__(self, state):
        self.value = state["v"]


class Mixed:
    __slots__ = ("x", "__dict__")


class ItemList(list):
    pass


class ItemDict(dict):
    pass


class Child(persistent.Persistent):
    def __init__(self, n):
        self.n = n


class Holder(persistent.Persistent):
    pass


def dump(state):
    def persistent_id(obj):
        if isinstance(obj, type) or getattr(obj, "_p_jar", None) is None:
            return None
        return (obj._p_oid, type(obj))

    file = io.BytesIO()
    PersistentPickler(persistent_id, file, 3).dump(state)
    return file.getvalue()


@pytest.fixture
def db():
    db = ZODB.DB(MappingStorage())
    yield db
    db.close()


def values():
    mixed = Mixed()
    mixed.x = 1
    mixed.y = 2
    shared = [1, 2]
    return {
        "ints": [
            0,
            1,
            255,
            256,
            65535,
            65536,
            -1,
            -(2**31),
            2**31 - 1,
            2**31,
            2**63,
            -(2**100),
            2**126,
            -(2**127),
        ],
        "floats": [0.0, -1.5, 1e300, float("inf")],
        "text": ["", "a", "é", "\U0001f600", "x" * 300, "\ud800"],
        "bytes": [b"", b"a", b"\x00" * 300, b"y" * 70000],
        "bools": [True, False, None],
        "tuples": [(), (1,), (1, 2), (1, 2, 3), (1, 2, 3, 4)],
        "nested": {"a": {"b": [(), {"c": (1, [2])}]}},
        "shared": [shared, shared, (shared,)],
        "set": {1, 2, 3},
        "frozenset": frozenset({"a"}),
        "slotted": Slotted(1, [2]),
        "plain": Plain(x=1, y="two"),
        "with_state": WithState([1]),
        "mixed": mixed,
        "item_list": ItemList([1, 2]),
        "item_dict": ItemDict(a=1),
        "ordered": collections.OrderedDict(a=1, b=2),
        "big_dict": {i: str(i) for i in range(2500)},
        "big_list": list(range(2500)),
    }


def test_states_equal_the_python_path(db):
    conn = db.open()
    root = conn.root()
    holder = Holder()
    holder.__dict__.update(values())
    holder.children = [Child(i) for i in range(5)]
    holder.mapping = PersistentMapping({"k": Child(9)})
    holder.plist = PersistentList([Child(10)])
    holder.self_ref = holder
    root["h"] = holder
    transaction.commit()
    oids = [holder._p_oid, holder.mapping._p_oid, holder.plist._p_oid, root._p_oid]
    oids += [c._p_oid for c in holder.children]
    conn.close()

    rust = db.open()
    rust._reader = zoe.reader.Reader(rust, rust._cache, db.classFactory)
    python = db.open()
    python._reader = ObjectReader(python, python._cache, db.classFactory)
    for oid in oids:
        pickle, _ = rust._storage.load(oid)
        state, reason = rust._reader._decode(pickle)
        assert reason is None
        assert dump(state) == dump(python._reader.getState(pickle))
        for value in state.values() if isinstance(state, dict) else []:
            if getattr(value, "_p_jar", None) is not None:
                assert rust._cache.get(value._p_oid) is value


def test_loads_through_install(db):
    zoe.reader.install(db)
    # The pool keeps the connection that DB() opened to make the root: a
    # stock Connection, whose reader `install` replaced.
    conn = db.open()
    assert isinstance(conn._reader, zoe.reader.Reader)
    other = db.open()
    assert isinstance(other, zoe.reader.Connection)
    assert isinstance(other._reader, zoe.reader.Reader)
    other.close()
    root = conn.root()
    root["h"] = holder = Holder()
    holder.__dict__.update(values())
    holder.child = Child(3)
    transaction.commit()
    conn.cacheMinimize()
    before = zoe.reader.counts()
    assert root["h"].ints == values()["ints"]
    assert root["h"].child.n == 3
    assert root["h"].slotted == Slotted(1, [2])
    assert root["h"].mixed.y == 2
    after = zoe.reader.counts()
    assert after["decoded"] - before["decoded"] >= 3
    assert after["fallback"] == before["fallback"]
    conn.close()


def test_weak_and_cross_database_references_take_the_python_path():
    from persistent.wref import WeakRef

    databases = {}
    one = ZODB.DB(MappingStorage(), databases=databases, database_name="one")
    two = ZODB.DB(MappingStorage(), databases=databases, database_name="two")
    zoe.reader.install(one)
    conn = one.open()
    other = conn.get_connection("two")
    target = Child(1)
    other.root()["t"] = target
    transaction.commit()
    conn.root()["x"] = holder = Holder()
    holder.target = target
    holder.weak = WeakRef(holder)
    transaction.commit()
    pickle, _ = conn._storage.load(holder._p_oid)
    state, reason = conn._reader._decode(pickle)
    assert state is None
    assert reason in ("other database", "weak reference")
    conn.cacheMinimize()
    before = sum(zoe.reader.counts()["fallback"].values())
    assert conn.root()["x"].target.n == 1
    assert sum(zoe.reader.counts()["fallback"].values()) - before == 1
    conn.close()
    one.close()
    two.close()
