"""Acceptance test of the reader of ZOE (`zoe.reader`, ADR 0057).

For each construct of a record, the state that the reader gives pickles to
the bytes of the state of ZODB's `ObjectReader.getState`, with each
persistent object as its oid and the name of its class. Each persistent
object of the state is the object that the pickle cache of the connection
holds for its oid: a new ghost at the first decode, the same object at the
next one. The counts of the reader tell that its Rust path decoded the
record.

A weak reference, a reference into another database, a class that the
class factory gives as `Broken` and a truncated record take the Python
path: the reader gives the state of `ObjectReader.getState`, or raises its
error, and the counts name the reason.

`install(db)` gives the reader to the connection that the pool of `db`
holds and to the next connection that `db.open()` makes.
"""

import collections
import contextlib
import io
import sys
import types

import pytest
import transaction
import ZODB
from persistent import Persistent
from persistent.list import PersistentList
from persistent.mapping import PersistentMapping
from persistent.wref import WeakRef
from ZODB._compat import PersistentPickler
from ZODB.MappingStorage import MappingStorage
from ZODB.serialize import ObjectReader

import zoe.reader


class Slotted:
    __slots__ = ("a", "b")

    def __init__(self, a, b):
        self.a = a
        self.b = b


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


class Child(Persistent):
    def __init__(self, n=0):
        self.n = n


class NewArgsChild(Persistent):
    """ZODB refers to an object of a class with `__getnewargs__` by its oid
    alone, without its class."""

    def __getnewargs__(self):
        return ()


class Holder(Persistent):
    pass


def shared_lists():
    shared = [1, 2]
    return [shared, shared, (shared,), {"again": shared}]


def mixed():
    value = Mixed()
    value.x = 1
    value.y = [2]
    return value


# Each construct of a record, as the value of an attribute of a `Holder`.
CONSTRUCTS = {
    "ints of each width": lambda: [
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
    "floats": lambda: [0.0, -1.5, 1e300, 5e-324, float("inf"), float("-inf")],
    "str with a lone surrogate": lambda: [
        "",
        "a",
        "é",
        "\U0001f600",
        "x" * 300,
        "\ud800",
        "a\udfffb",
    ],
    "bytes above 64 KB": lambda: [b"", b"a", b"\x00" * 300, b"y" * 70_000],
    "tuples of each length": lambda: [
        (),
        (1,),
        (1, 2),
        (1, 2, 3),
        (1, 2, 3, 4),
        tuple(range(300)),
    ],
    "shared lists": shared_lists,
    "sets": lambda: [set(), {1, 2, 3}, {"a", (1, 2)}],
    "frozensets": lambda: [frozenset(), frozenset({"a"}), frozenset(range(10))],
    "an OrderedDict": lambda: collections.OrderedDict(a=1, b=[2]),
    "subclasses of list and dict": lambda: [ItemList([1, 2]), ItemDict(a=1)],
    "slots": lambda: Slotted(1, [2]),
    "__setstate__": lambda: WithState([1]),
    "slots with a dict": mixed,
    "a dict of 2,500 items": lambda: {i: str(i) for i in range(2500)},
    "a list of 2,500 items": lambda: list(range(2500)),
    "persistent references with a class": lambda: [
        Child(1),
        PersistentMapping({"k": Child(2)}),
        PersistentList([Child(3)]),
    ],
    "persistent references without a class": lambda: [NewArgsChild(), NewArgsChild()],
}


def dump(state):
    """Return the bytes of `state` and the persistent objects in it.

    Each persistent object pickles as its oid and the name of its class, and
    a weak reference as its oid.
    """
    seen = []

    def persistent_id(obj):
        if isinstance(obj, WeakRef):
            return ("weak", obj.oid)
        if isinstance(obj, type) or getattr(obj, "_p_jar", None) is None:
            return None
        seen.append(obj)
        klass = type(obj)
        return (obj._p_oid, klass.__module__, klass.__qualname__)

    file = io.BytesIO()
    PersistentPickler(persistent_id, file, 3).dump(state)
    return file.getvalue(), seen


@contextlib.contextmanager
def counting():
    """Yield a dict that holds, at the exit, the records that the Rust path
    decoded in the block and the records that took the Python path, by
    reason."""
    found = {}
    before = zoe.reader.counts()
    try:
        yield found
    finally:
        after = zoe.reader.counts()
        found["decoded"] = after["decoded"] - before["decoded"]
        found["fallback"] = {
            reason: n - before["fallback"].get(reason, 0)
            for reason, n in after["fallback"].items()
            if n != before["fallback"].get(reason, 0)
        }


def error_of(call, *args):
    """Return the exception that `call(*args)` raises."""
    try:
        call(*args)
    except Exception as error:  # noqa: BLE001 - the test compares it
        return error
    pytest.fail(f"{call} raised nothing")


@pytest.fixture
def db():
    db = ZODB.DB(MappingStorage())
    yield db
    db.close()


@pytest.fixture
def opened(db):
    """Return a function that opens a connection of a database in a
    transaction manager of its own, with a reader of the class `reader`
    when it is given. The fixture closes the connections at the end, so
    a connection that wrote does not go back to the pool during the test."""
    connections = []

    def open_connection(reader=None, database=None):
        database = database or db
        conn = database.open(transaction.TransactionManager())
        if reader is not None:
            conn._reader = reader(conn, conn._cache, database.classFactory)
        connections.append(conn)
        return conn

    yield open_connection
    for conn in connections:
        conn.close()


def store(conn, value):
    """Commit a `Holder` whose attribute `value` is `value`. Return the oids
    of the records of the commit."""
    holder = Holder()
    holder.value = value
    conn.root()["holder"] = holder
    conn.transaction_manager.commit()
    last = list(conn.db().storage.iterator())[-1]
    return [record.oid for record in last]


def check_python_path(rust, python, oid, reason):
    """Check that the record of `oid` takes the Python path for `reason`,
    and that the reader gives the state of `ObjectReader.getState`."""
    pickle, _ = rust._storage.load(oid)
    with counting() as counts:
        state = rust._reader.getState(pickle)
    assert counts == {"decoded": 0, "fallback": {reason: 1}}
    assert dump(state)[0] == dump(python._reader.getState(pickle))[0]


@pytest.mark.parametrize("name", list(CONSTRUCTS))
def test_a_record_decodes_as_object_reader_decodes_it(opened, name):
    oids = store(opened(), CONSTRUCTS[name]())
    rust = opened(zoe.reader.Reader)
    python = opened(ObjectReader)
    for oid in oids:
        pickle, _ = rust._storage.load(oid)
        want, _ = dump(python._reader.getState(pickle))
        # The first decode makes the ghosts of the references, the second
        # one finds them in the pickle cache.
        for _ in range(2):
            with counting() as counts:
                state = rust._reader.getState(pickle)
            assert counts == {"decoded": 1, "fallback": {}}
            got, references = dump(state)
            assert got == want
            for obj in references:
                assert rust._cache.get(obj._p_oid) is obj


def test_a_load_through_the_connection_decodes_in_rust(opened):
    writer = opened()
    holder = Holder()
    holder.__dict__.update({name: make() for name, make in CONSTRUCTS.items()})
    writer.root()["holder"] = holder
    writer.transaction_manager.commit()
    python = opened(ObjectReader)
    pickle, _ = python._storage.load(holder._p_oid)
    want, _ = dump(python._reader.getState(pickle))
    conn = opened(zoe.reader.Reader)
    with counting() as counts:
        got, _ = dump(conn.root()["holder"].__getstate__())
    assert got == want
    # The root and the holder.
    assert counts == {"decoded": 2, "fallback": {}}


def test_a_weak_reference_takes_the_python_path(opened):
    writer = opened()
    target = Child(1)
    writer.root()["target"] = target
    holder = Holder()
    holder.weak = WeakRef(target)
    writer.root()["holder"] = holder
    writer.transaction_manager.commit()
    rust = opened(zoe.reader.Reader)
    python = opened(ObjectReader)
    check_python_path(rust, python, holder._p_oid, "weak reference")


def test_a_reference_into_another_database_takes_the_python_path():
    databases = {}
    one = ZODB.DB(MappingStorage(), databases=databases, database_name="one")
    two = ZODB.DB(MappingStorage(), databases=databases, database_name="two")
    try:
        writer = one.open(transaction.TransactionManager())
        target = Child(1)
        writer.get_connection("two").root()["target"] = target
        # A new object reachable from two databases does not commit, so the
        # target commits first.
        writer.transaction_manager.commit()
        holder = Holder()
        holder.target = target
        writer.root()["holder"] = holder
        writer.transaction_manager.commit()
        rust = one.open(transaction.TransactionManager())
        rust._reader = zoe.reader.Reader(rust, rust._cache, one.classFactory)
        python = one.open(transaction.TransactionManager())
        python._reader = ObjectReader(python, python._cache, one.classFactory)
        check_python_path(rust, python, holder._p_oid, "other database")
        for conn in (rust, python, writer):
            conn.close()
    finally:
        one.close()
        two.close()


def test_a_broken_class_takes_the_python_path(opened):
    # A class of a module that goes away: the class factory then gives a
    # subclass of `ZODB.broken.Broken`.
    name = "zoe_reader_test_gone"
    module = types.ModuleType(name)
    gone = type("Gone", (Persistent,), {"__module__": name})
    module.Gone = gone
    sys.modules[name] = module
    try:
        writer = opened()
        obj = gone()
        obj.n = 1
        writer.root()["gone"] = obj
        holder = Holder()
        holder.gone = obj
        writer.root()["holder"] = holder
        writer.transaction_manager.commit()
    finally:
        del sys.modules[name]
    rust = opened(zoe.reader.Reader)
    python = opened(ObjectReader)
    # The record of an object of the class, and a reference to one.
    for oid in (obj._p_oid, holder._p_oid):
        check_python_path(rust, python, oid, "broken")


def test_a_truncated_record_takes_the_python_path(opened):
    store(opened(), [Child(1), "text", 2**40])
    rust = opened(zoe.reader.Reader)
    python = opened(ObjectReader)
    oid = rust.root()["holder"]._p_oid
    pickle, _ = rust._storage.load(oid)
    truncated = pickle[:-3]
    with counting() as counts:
        got = error_of(rust._reader.getState, truncated)
    want = error_of(python._reader.getState, truncated)
    assert counts == {"decoded": 0, "fallback": {"malformed": 1}}
    assert type(got) is type(want)
    assert repr(got.__cause__) == repr(want.__cause__)
    assert str(got) == str(want)


def test_install_gives_the_reader_to_the_pooled_and_the_next_connection(db):
    # `DB()` keeps the connection that made the root in its pool. It gets a
    # stock reader here, whatever the switch of the tests.
    (pooled,) = list(db.pool)
    factory = pooled._reader._factory
    pooled._reader = ObjectReader(pooled, pooled._cache, factory)
    zoe.reader.install(db)
    assert isinstance(pooled._reader, zoe.reader.Reader)
    assert pooled._reader._factory is factory
    assert db.klass is zoe.reader.Connection
    conn = db.open()
    assert conn is pooled
    other = db.open()
    assert isinstance(other, zoe.reader.Connection)
    assert isinstance(other._reader, zoe.reader.Reader)
    other.close()
    holder = Holder()
    holder.child = Child(3)
    holder.values = CONSTRUCTS["shared lists"]()
    conn.root()["holder"] = holder
    transaction.commit()
    conn.cacheMinimize()
    with counting() as counts:
        assert conn.root()["holder"].child.n == 3
    assert counts["decoded"] >= 3
    assert counts["fallback"] == {}
    conn.close()
