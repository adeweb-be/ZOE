"""Acceptance test of the `setstate` of `zoe.reader.Connection` (ADR 0057,
amendment).

`zoe.reader.Connection.setstate` does in one call into Rust what ZODB's
`Connection.setstate` does for an ordinary record: the check of an open
connection, `self._storage.load(oid)`, `_load_count`, the decode and the
`__setstate__` of the reader, `_p_serial`, the size estimation of the
pickle cache and `_p_estimated_size`. A blob and a closed connection go
to ZODB's method. A `ConflictError` leaves as it is. Another error is
logged, then raised.

Each test loads the same ghost through a `zoe.reader.Connection` and
through a ZODB `Connection` with ZODB's `ObjectReader`, on one database.
For an ordinary record, a blob, a closed connection, a `ConflictError` of
the storage, another error of the storage and a record that does not
decode, the object, `_p_serial`, `_p_estimated_size`, `_load_count`, the
size estimation of the pickle cache, the log records and the exception
are the same. No Python frame calls the load of an ordinary record
through a `zoe.reader.Connection`.

When another function replaces the `setstate` of ZODB's `Connection`, a
`zoe.reader.Connection` calls it. The `setstate` of the early locks of
`zoe.zodb` loads for a storage with hot classes; without them, the load
stays in Rust.

`install(db)` makes the connection that the pool of `db` holds a
`zoe.reader.Connection`.
"""

import io
import logging
import sys
import types

import pytest
import transaction
import ZODB
import ZODB.Connection
from persistent import Persistent
from persistent.mapping import PersistentMapping
from ZODB._compat import PersistentPickler
from ZODB.blob import Blob, BlobStorage
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import POSKeyError, ReadConflictError
from ZODB.serialize import ObjectReader
from ZODB.utils import oid_repr, z64

import zoe.reader

ZODB_CONNECTION = ZODB.Connection.Connection


def is_zodb_setstate(code):
    """Whether `code` is the code of the function `setstate` of ZODB. The
    early locks of `zoe.zodb` may have replaced the method of the class."""
    return (
        code.co_qualname == "Connection.setstate"
        and code.co_filename == ZODB.Connection.__file__
    )


class Child(Persistent):
    def __init__(self, n=0):
        self.n = n


class Holder(Persistent):
    pass


class Storage:
    """The storage of a connection, in front of the storage that the
    connection had. Its `load` records the code that calls it. It raises
    `error` when one is set, and gives `record(pickle)` in place of the
    pickle when `record` is set."""

    def __init__(self, inner, error=None, record=None):
        self.inner = inner
        self.error = error
        self.record = record
        self.callers = []

    def load(self, oid):
        self.callers.append(sys._getframe(1).f_code)
        if self.error is not None:
            raise self.error
        pickle, serial = self.inner.load(oid)
        if self.record is not None:
            pickle = self.record(pickle)
        return pickle, serial

    def __getattr__(self, name):
        return getattr(self.inner, name)


def dump(state):
    """Return the bytes of `state`, with each persistent object as its oid
    and the name of its class."""

    def persistent_id(obj):
        if isinstance(obj, type) or getattr(obj, "_p_jar", None) is None:
            return None
        klass = type(obj)
        return (obj._p_oid, klass.__module__, klass.__qualname__)

    file = io.BytesIO()
    PersistentPickler(persistent_id, file, 3).dump(state)
    return file.getvalue()


def activate(obj):
    """Load the state of the ghost `obj` as an access does. Return the
    exception that the load raises, or None."""
    try:
        obj._p_activate()
    except BaseException as error:  # noqa: BLE001 - the test compares it
        return error
    return None


def error_facts(error):
    """The type, the text and the cause of `error`."""
    if error is None:
        return None
    return (type(error), str(error), repr(error.__cause__))


def facts(conn, obj, error, records):
    """What the test compares after a load of `obj` in `conn`."""
    loaded = obj._p_status != "ghost"
    return {
        "status": obj._p_status,
        "state": dump(obj.__getstate__()) if loaded else None,
        "serial": obj._p_serial,
        "estimated size": obj._p_estimated_size,
        "load count": conn._load_count,
        "cache size": conn._cache.total_estimated_size,
        "log": [
            (
                record.name,
                record.levelno,
                record.getMessage(),
                error_facts(record.exc_info[1]) if record.exc_info else None,
            )
            for record in records
        ],
        "error": error_facts(error),
    }


def load(conn, oid, caplog, storage=None, close=False):
    """Load the ghost of `oid` in `conn`. Put `storage` in front of the
    storage of `conn` before the load, or close `conn` before the load
    when `close` is true. Return the object, the exception of the load and
    the facts of `facts`."""
    obj = conn.get(oid)
    assert obj._p_status == "ghost"
    if storage is not None:
        storage.inner = conn._storage
        conn._storage = storage
    if close:
        conn.close()
    caplog.clear()
    error = activate(obj)
    return obj, error, facts(conn, obj, error, caplog.records)


def open_as(db, klass):
    """Open a connection of the class `klass` on `db`."""
    db.klass = klass
    try:
        conn = db.open(transaction.TransactionManager())
    finally:
        del db.klass
    assert type(conn) is klass
    return conn


@pytest.fixture
def stored(tmp_path):
    """A database with a `Holder` of values and references, and a blob.
    A writer holds the connection of the pool open, so each `db.open()` of
    the test makes a new connection."""
    db = ZODB.DB(BlobStorage(str(tmp_path / "blobs"), MappingStorage()))
    writer = db.open(transaction.TransactionManager())
    holder = Holder()
    holder.value = {
        "text": "é",
        "n": 2**40,
        "items": [1, (2, 3), b"x" * 300],
        "child": Child(1),
        "mapping": PersistentMapping({"k": Child(2)}),
    }
    blob = Blob(b"the data of the blob")
    root = writer.root()
    root["holder"] = holder
    root["blob"] = blob
    writer.transaction_manager.commit()
    yield types.SimpleNamespace(
        db=db,
        holder=holder._p_oid,
        child=holder.value["child"]._p_oid,
        blob=blob._p_oid,
    )
    writer.close()
    db.close()


@pytest.fixture
def pair(stored, caplog):
    """Return a function that opens a `zoe.reader.Connection` whose reader
    is of the class `reader` and a ZODB `Connection` with ZODB's
    `ObjectReader`. The fixture closes them at the end."""
    caplog.set_level(logging.DEBUG)
    connections = []

    def open_pair(reader=zoe.reader.Reader):
        db = stored.db
        ours = open_as(db, zoe.reader.Connection)
        ours._reader = reader(ours, ours._cache, db.classFactory)
        theirs = open_as(db, ZODB_CONNECTION)
        theirs._reader = ObjectReader(theirs, theirs._cache, db.classFactory)
        connections.extend((ours, theirs))
        return ours, theirs

    yield open_pair
    for conn in connections:
        while isinstance(conn._storage, Storage):
            conn._storage = conn._storage.inner
        if conn.opened is not None:
            conn.close()


def test_the_setstate_of_the_connection_of_zoe_is_not_zodbs():
    assert zoe.reader.Connection.setstate is not ZODB_CONNECTION.setstate
    assert issubclass(zoe.reader.Connection, ZODB_CONNECTION)


@pytest.mark.parametrize("reader", [zoe.reader.Reader, ObjectReader])
def test_an_ordinary_record(stored, pair, caplog, reader):
    ours, theirs = pair(reader)
    ours_storage, theirs_storage = Storage(None), Storage(None)
    obj, error, got = load(ours, stored.holder, caplog, ours_storage)
    _, _, want = load(theirs, stored.holder, caplog, theirs_storage)
    assert error is None
    assert got == want
    assert got["status"] == "saved"
    assert got["load count"] == 1
    assert obj.value["child"] is ours.get(obj.value["child"]._p_oid)
    # One call into Rust: the load of the connection of ZOE has no Python
    # frame between the access and the storage.
    assert ours_storage.callers == [activate.__code__]
    (theirs_caller,) = theirs_storage.callers
    assert is_zodb_setstate(theirs_caller)


def test_a_blob(stored, pair, caplog):
    ours, theirs = pair()
    got_blob, error, got = load(ours, stored.blob, caplog)
    want_blob, _, want = load(theirs, stored.blob, caplog)
    assert error is None
    assert got == want
    assert got["status"] == "saved"
    assert got_blob._p_blob_uncommitted is want_blob._p_blob_uncommitted is None
    assert got_blob._p_blob_committed == want_blob._p_blob_committed
    assert got_blob._p_blob_committed is not None
    with got_blob.open() as file:
        assert file.read() == b"the data of the blob"


def test_a_closed_connection(stored, pair, caplog):
    ours, theirs = pair()
    _, error, got = load(ours, stored.holder, caplog, close=True)
    _, _, want = load(theirs, stored.holder, caplog, close=True)
    assert got == want
    assert type(error).__name__ == "ConnectionStateError"
    assert got["load count"] == 0
    ((name, level, message, logged),) = got["log"]
    assert (name, level) == ("ZODB.Connection", logging.ERROR)
    assert message.startswith("Shouldn't load state for ")
    assert logged == error_facts(error)


def test_a_conflict_error_of_the_storage(stored, pair, caplog):
    ours, theirs = pair()
    conflict = ReadConflictError(oid=stored.holder)
    _, got_error, got = load(ours, stored.holder, caplog, Storage(None, conflict))
    _, want_error, want = load(theirs, stored.holder, caplog, Storage(None, conflict))
    assert got == want
    # Raised as it is, and not logged.
    assert got_error is want_error is conflict
    assert got["log"] == []
    assert got["status"] == "ghost"
    assert got["load count"] == 0


@pytest.mark.parametrize(
    "make_error",
    [lambda oid: POSKeyError(oid), lambda oid: KeyboardInterrupt()],
    ids=["POSKeyError", "KeyboardInterrupt"],
)
def test_another_error_of_the_storage(stored, pair, caplog, make_error):
    ours, theirs = pair()
    error = make_error(stored.holder)
    _, got_error, got = load(ours, stored.holder, caplog, Storage(None, error))
    _, want_error, want = load(theirs, stored.holder, caplog, Storage(None, error))
    assert got == want
    # Logged, then raised as it is.
    assert got_error is want_error is error
    ((name, level, message, logged),) = got["log"]
    assert (name, level) == ("ZODB.Connection", logging.ERROR)
    assert (
        message
        == f"Couldn't load state for {__name__}.Holder {oid_repr(stored.holder)}"
    )
    assert logged == error_facts(error)
    assert got["status"] == "ghost"
    assert got["load count"] == 0


@pytest.mark.parametrize("reader", [zoe.reader.Reader, ObjectReader])
def test_a_record_that_does_not_decode(stored, pair, caplog, reader):
    ours, theirs = pair(reader)

    def truncate(pickle):
        return pickle[:-3]

    _, error, got = load(ours, stored.holder, caplog, Storage(None, record=truncate))
    _, _, want = load(theirs, stored.holder, caplog, Storage(None, record=truncate))
    assert error is not None
    assert got == want
    want_message = (
        f"Couldn't load state for {__name__}.Holder {oid_repr(stored.holder)}"
    )
    assert got["log"][-1][2] == want_message
    assert got["status"] == "ghost"
    assert got["load count"] == 1


def test_install_makes_the_pooled_connection_a_connection_of_zoe():
    db = ZODB.DB(MappingStorage())
    try:
        # `DB()` keeps the connection that made the root in its pool. It is
        # a ZODB `Connection` with ZODB's reader here, whatever the switch
        # of the tests.
        (pooled,) = list(db.pool)
        pooled.__class__ = ZODB_CONNECTION
        pooled._reader = ObjectReader(pooled, pooled._cache, pooled._reader._factory)
        zoe.reader.install(db)
        assert type(pooled) is zoe.reader.Connection
        assert isinstance(pooled._reader, zoe.reader.Reader)
        conn = db.open()
        assert conn is pooled
        holder = Holder()
        holder.child = Child(3)
        conn.root()["holder"] = holder
        transaction.commit()
        conn.cacheMinimize()
        storage = Storage(conn._storage)
        conn._storage = storage
        assert activate(holder) is None
        assert holder.child.n == 3
        assert storage.callers
        assert not any(is_zodb_setstate(code) for code in storage.callers)
        conn._storage = storage.inner
        conn.close()
    finally:
        db.close()


def test_a_function_that_replaced_zodbs_setstate_loads(
    stored, pair, caplog, monkeypatch
):
    ours, _ = pair()
    # The first call reads the names of ZODB before the replacement.
    assert activate(ours.get(z64)) is None
    calls = []
    replaced_function = ZODB_CONNECTION.setstate

    def replacement(self, obj):
        calls.append(obj)
        return replaced_function(self, obj)

    monkeypatch.setattr(ZODB_CONNECTION, "setstate", replacement)
    obj, error, _ = load(ours, stored.holder, caplog)
    assert error is None
    assert calls == [obj]
    assert obj._p_status == "saved"


def test_a_storage_with_hot_classes_loads_through_the_early_locks(
    stored, pair, caplog, monkeypatch
):
    import zoe.zodb

    ours, _ = pair()
    # The first call reads the names of ZODB before the replacement.
    assert activate(ours.get(z64)) is None
    monkeypatch.setattr(
        ZODB_CONNECTION, "setstate", zoe.zodb.LockingConnection.setstate
    )
    # Without hot classes, the load stays in Rust.
    storage = Storage(None)
    _, error, _ = load(ours, stored.holder, caplog, storage)
    assert error is None
    assert storage.callers == [activate.__code__]
    # With hot classes, the `setstate` of the early locks loads. It gives an
    # object of another class to the `setstate` of ZODB.
    hot = types.SimpleNamespace(early_lock_classes=frozenset({"elsewhere.Hot"}))
    monkeypatch.setattr(stored.db, "storage", hot)
    storage = Storage(None)
    obj, error, _ = load(ours, stored.child, caplog, storage)
    assert error is None
    assert obj._p_status == "saved"
    (caller,) = storage.callers
    assert is_zodb_setstate(caller)
