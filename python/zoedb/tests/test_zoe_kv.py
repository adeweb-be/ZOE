"""Unit tests of the KV namespaces of `zoe.Storage` and of `zoe.kv` (ADR
0043): the storage methods, the writes in the two-phase commit, the data
manager of a connection, savepoints, and the errors."""

import time

import pytest
import transaction
import ZODB
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import ReadOnlyError, Unsupported
from ZODB.utils import z64

import zoe
import zoe.kv


def commit(storage, *writes):
    """Commit `writes`, each a tuple of the arguments of `kv_set` without the
    transaction, or `("delete", flavour, name, key)`. Return the tid."""
    meta = TransactionMetaData()
    storage.tpc_begin(meta)
    for write in writes:
        if write[0] == "delete":
            storage.kv_delete(*write[1:], meta)
        else:
            storage.kv_set(*write, meta)
    storage.tpc_vote(meta)
    return storage.tpc_finish(meta)


@pytest.fixture
def storage(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    yield storage
    storage.close()


def test_a_write_commits_with_its_transaction(storage):
    assert storage.kv_get("durable", "registry", b"title") is None
    assert storage.kv_generation("registry") == z64
    tid = commit(storage, ("durable", "registry", b"title", b"site", None))
    assert storage.kv_get("durable", "registry", b"title") == b"site"
    assert storage.kv_generation("registry") == tid
    assert storage.lastTransaction() == tid
    commit(storage, ("delete", "durable", "registry", b"title"))
    assert storage.kv_get("durable", "registry", b"title") is None


def test_an_abort_writes_nothing(storage):
    meta = TransactionMetaData()
    storage.tpc_begin(meta)
    storage.kv_set("durable", "registry", b"title", b"site", None, meta)
    storage.tpc_vote(meta)
    storage.tpc_abort(meta)
    assert storage.kv_get("durable", "registry", b"title") is None


def test_an_ephemeral_key_expires(storage):
    commit(storage, ("ephemeral", "sessions", b"s1", b"data", 0.2))
    assert storage.kv_get("ephemeral", "sessions", b"s1") == b"data"
    assert storage.kv_scan("ephemeral", "sessions") == [(b"s1", b"data")]
    time.sleep(0.3)
    assert storage.kv_get("ephemeral", "sessions", b"s1") is None
    assert storage.kv_scan("ephemeral", "sessions") == []


def test_a_scan_takes_bounds_and_a_limit(storage):
    commit(
        storage,
        *[("durable", "registry", key, key, None) for key in (b"a", b"b", b"c", b"d")],
    )
    assert storage.kv_scan("durable", "registry", b"b", b"d") == [
        (b"b", b"b"),
        (b"c", b"c"),
    ]
    assert storage.kv_scan("durable", "registry", limit=2) == [
        (b"a", b"a"),
        (b"b", b"b"),
    ]
    for limit in (0, -1):
        with pytest.raises(ValueError):
            storage.kv_scan("durable", "registry", limit=limit)


@pytest.mark.parametrize(
    "args",
    [
        ("other", "registry", b"k", b"v", None),
        ("durable", "", b"k", b"v", None),
        ("durable", "n" * 256, b"k", b"v", None),
        ("durable", "registry", b"", b"v", None),
        ("durable", "registry", b"k" * 1025, b"v", None),
        ("durable", "registry", b"k", b"v", 5),
        ("ephemeral", "sessions", b"k", b"v", None),
        ("ephemeral", "sessions", b"k", b"v", 0),
        ("ephemeral", "sessions", b"k", b"v", -1),
        ("ephemeral", "sessions", b"k", b"v", float("nan")),
        ("ephemeral", "sessions", b"k", b"v", 2**32),
    ],
)
def test_a_bad_write_raises_value_error(storage, args):
    meta = TransactionMetaData()
    storage.tpc_begin(meta)
    try:
        with pytest.raises(ValueError):
            storage.kv_set(*args, meta)
    finally:
        storage.tpc_abort(meta)


def test_a_key_or_a_value_that_is_not_bytes_raises_type_error(storage):
    meta = TransactionMetaData()
    storage.tpc_begin(meta)
    try:
        with pytest.raises(TypeError):
            storage.kv_set("durable", "registry", "k", b"v", None, meta)
        with pytest.raises(TypeError):
            storage.kv_set("durable", "registry", b"k", "v", None, meta)
        with pytest.raises(TypeError):
            storage.kv_set("ephemeral", "sessions", b"k", b"v", True, meta)
    finally:
        storage.tpc_abort(meta)


def test_a_write_outside_a_transaction_of_the_storage_is_refused(storage):
    from ZODB.POSException import StorageTransactionError

    with pytest.raises(StorageTransactionError):
        storage.kv_set("durable", "r", b"k", b"v", None, TransactionMetaData())


def test_a_read_only_storage_reads_and_refuses_writes(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    commit(storage, ("durable", "registry", b"title", b"site", None))
    storage.close()
    storage = zoe.Storage(path, read_only=True)
    try:
        assert storage.kv_get("durable", "registry", b"title") == b"site"
        with pytest.raises(ReadOnlyError):
            storage.kv_set("durable", "registry", b"t", b"v", None, object())
    finally:
        storage.close()


def test_a_durable_key_survives_a_reopen(tmp_path):
    path = str(tmp_path / "store")
    storage = zoe.Storage(path)
    tid = commit(storage, ("durable", "registry", b"title", b"site", None))
    storage.close()
    storage = zoe.Storage(path)
    try:
        assert storage.kv_get("durable", "registry", b"title") == b"site"
        assert storage.kv_generation("registry") == tid
    finally:
        storage.close()


@pytest.fixture
def db(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    yield db
    db.close()


def test_a_namespace_commits_with_the_objects_of_a_connection(db):
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    registry = zoe.kv.Namespace(conn, "durable", "registry")
    conn.root()["x"] = 1
    registry.set("title", b"site")
    # The transaction reads its own write first.
    assert registry.get("title") == b"site"
    assert db.storage.kv_get("durable", "registry", b"title") is None
    manager.commit()
    assert db.storage.kv_get("durable", "registry", b"title") == b"site"
    # One commit holds both: the generation is the tid of the object.
    assert registry.generation() == conn.root()._p_serial
    conn.close()


def test_a_namespace_without_object_changes_commits_alone(db):
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    sessions = zoe.kv.Namespace(conn, "ephemeral", "sessions")
    last = db.storage.lastTransaction()
    sessions.set(b"s1", b"data", ttl=60)
    manager.commit()
    assert db.storage.lastTransaction() > last
    assert sessions.get(b"s1") == b"data"
    sessions.delete(b"s1")
    assert sessions.get(b"s1") is None
    manager.commit()
    assert db.storage.kv_get("ephemeral", "sessions", b"s1") is None
    conn.close()


def test_an_abort_of_the_transaction_drops_the_writes(db):
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    registry = zoe.kv.Namespace(conn, "durable", "registry")
    registry.set("title", b"site")
    manager.abort()
    assert registry.get("title") is None
    manager.commit()
    assert db.storage.kv_get("durable", "registry", b"title") is None
    conn.close()


def test_a_scan_sees_the_writes_of_the_transaction(db):
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    registry = zoe.kv.Namespace(conn, "durable", "registry")
    for key in ("a", "b", "c"):
        registry.set(key, key.encode())
    manager.commit()
    registry.set("d", b"d")
    registry.delete("b")
    registry.set("a", b"A")
    assert registry.scan() == [(b"a", b"A"), (b"c", b"c"), (b"d", b"d")]
    assert registry.scan(start="b", stop="d") == [(b"c", b"c")]
    assert registry.scan(limit=1) == [(b"a", b"A")]
    manager.abort()
    conn.close()


def test_a_savepoint_rolls_back_the_writes(db):
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    registry = zoe.kv.Namespace(conn, "durable", "registry")
    registry.set("a", b"1")
    savepoint = manager.savepoint()
    registry.set("a", b"2")
    registry.set("b", b"3")
    savepoint.rollback()
    assert registry.get("a") == b"1"
    assert registry.get("b") is None
    manager.commit()
    assert db.storage.kv_get("durable", "registry", b"a") == b"1"
    assert db.storage.kv_get("durable", "registry", b"b") is None
    conn.close()


def test_a_namespace_of_another_storage_is_unsupported():
    from ZODB.MappingStorage import MappingStorage

    db = ZODB.DB(MappingStorage())
    conn = db.open()
    with pytest.raises(Unsupported):
        zoe.kv.Namespace(conn, "durable", "registry")
    conn.close()
    db.close()


def test_a_namespace_checks_its_arguments(db):
    conn = db.open()
    with pytest.raises(ValueError):
        zoe.kv.Namespace(conn, "lasting", "registry")
    with pytest.raises(TypeError):
        zoe.kv.Namespace(conn, "durable", b"registry")
    registry = zoe.kv.Namespace(conn, "durable", "registry")
    with pytest.raises(TypeError):
        registry.set(1, b"v")
    with pytest.raises(TypeError):
        registry.set("k", "v")
    transaction.abort()
    conn.close()


def test_a_demo_storage_over_a_zoe_storage_keeps_its_writes_in_an_overlay(tmp_path):
    from ZODB.DemoStorage import DemoStorage

    base = zoe.Storage(str(tmp_path / "store"))
    commit(base, ("durable", "registry", b"kept", b"base", None))
    generation = base.kv_generation("registry")
    demo = DemoStorage(base=base, close_base_on_close=False)
    db = ZODB.DB(demo)
    try:
        manager = transaction.TransactionManager()
        conn = db.open(manager)
        registry = zoe.kv.Namespace(conn, "durable", "registry")
        sessions = zoe.kv.Namespace(conn, "ephemeral", "sessions")
        # A read goes through to the base.
        assert registry.get("kept") == b"base"
        registry.set("new", b"demo")
        registry.delete("kept")
        sessions.set("s", b"v", ttl=0.2)
        manager.commit()
        assert registry.get("new") == b"demo"
        assert registry.get("kept") is None
        assert registry.scan() == [(b"new", b"demo")]
        assert sessions.get("s") == b"v"
        assert registry.generation() != generation
        # The base never sees the writes of the overlay.
        assert base.kv_get("durable", "registry", b"new") is None
        assert base.kv_get("durable", "registry", b"kept") == b"base"
        assert base.kv_generation("registry") == generation
        # An aborted write stays out of the overlay too.
        registry.set("aborted", b"x")
        manager.abort()
        assert registry.get("aborted") is None
        time.sleep(0.3)
        assert sessions.get("s") is None
        # A DemoStorage on the DemoStorage reads both overlays.
        top = ZODB.DB(DemoStorage(base=demo, close_base_on_close=False))
        try:
            with top.transaction() as other:
                upper = zoe.kv.Namespace(other, "durable", "registry")
                assert upper.get("new") == b"demo"
                upper.set("top", b"t")
            with top.transaction() as other:
                assert zoe.kv.Namespace(other, "durable", "registry").get("top") == b"t"
            assert registry.get("top") is None
        finally:
            top.close()
        # A bad write fails the commit, as on the store.
        sessions.set("bad", b"v")
        with pytest.raises(ValueError):
            manager.commit()
        manager.abort()
        conn.close()
    finally:
        db.close()
        base.close()


def test_a_demo_storage_over_another_storage_has_no_namespaces():
    from ZODB.DemoStorage import DemoStorage

    db = ZODB.DB(DemoStorage())
    conn = db.open()
    try:
        assert not zoe.kv.supported(conn)
        with pytest.raises(Unsupported):
            zoe.kv.Namespace(conn, "durable", "registry")
    finally:
        conn.close()
        db.close()
