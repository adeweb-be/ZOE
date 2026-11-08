"""Acceptance test of the text methods of the storages (ADR 0060, sections
8 and 12).

A store holds a catalog of Products.ZCatalog, its registration key in
`zoe.catalog` and the settings of its text index `Title` in `zoe.text`.
`zoe.Storage` and `zoe.zeo.ClientStorage` with the native client inside
stage text deltas in a transaction. After the commit, a text clause of
`catalog_query` and `text_query` find them, and `text_info` gives the live
text; after an abort, nothing. Each error of the core raises its Python
class. `text_supported()` is true on `zoe.Storage` and on the native client
against `zoe serve`, and false against a stock ZEO 6 server. The asyncio
mode has no text methods.
"""

import json
import time

import pytest
import transaction
import ZODB
from client_support import native
from support import TIMEOUT, client
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import (
    ConflictError,
    ReadConflictError,
    StorageError,
    UndoError,
    Unsupported,
)

import zoe
import zoe.kv
import zoe.storage
from zoe.testing import ZoeServer

pytest.importorskip("Products.ZCatalog")

SETTINGS = {
    "indexes": {
        "Title": {
            "indexed_attrs": ["Title"],
            "mode": "enhanced",
            "language": ["en"],
            "boosts": {},
            "fuzzy": 0,
        }
    }
}


class Content:
    """An object to catalog."""

    def __init__(self, **attributes):
        self.__dict__.update(attributes)


def make_catalog(path):
    """Write a catalog of 6 objects, its registration key and the settings
    of its text index to a new `zoe.Storage` in `path`. Return the oid of
    the `Catalog` object and the rids by path."""
    from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
    from Products.ZCatalog.Catalog import Catalog

    db = ZODB.DB(zoe.Storage(str(path)))
    try:
        connection = db.open()
        catalog = Catalog()
        catalog.addIndex("portal_type", FieldIndex("portal_type"))
        connection.root()["catalog"] = catalog
        transaction.commit()
        for i in range(6):
            kind = "Document" if i % 2 == 0 else "Folder"
            catalog.catalogObject(Content(portal_type=kind), f"/site/item-{i}")
        root = connection.root()
        oid = catalog._p_oid
        zoe.kv.Namespace(connection, "durable", "zoe.catalog").set(oid, b"")
        settings = json.dumps(SETTINGS).encode()
        zoe.kv.Namespace(connection, "durable", "zoe.text").set(oid, settings)
        transaction.commit()
        rids = {path: rid for path, rid in catalog.uids.items()}
        del root
        connection.close()
    finally:
        db.close()
    return oid, rids


def after(tid):
    """The exclusive tid of the snapshot that holds `tid`."""
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


def clause(index, words, **extra):
    out = {
        "i": index,
        "q": {"n": "atom", "w": list(words)},
        "lg": None,
        "cl": [],
        "st": [],
        "set": None,
    }
    out.update(extra)
    return out


def plan(clauses):
    return {
        "c": list(clauses),
        "s": [],
        "l": None,
        "bs": 0,
        "bz": None,
        "cols": None,
        "rn": 0,
    }


def commit(storage, stage):
    """Commit a transaction of `storage` in which `stage(t)` stages its
    deltas. Return the tid."""
    t = TransactionMetaData()
    storage.tpc_begin(t)
    stage(t)
    storage.tpc_vote(t)
    tid = storage.tpc_finish(t)
    return tid


def settled(call):
    """The result of `call` once the text index answers."""
    deadline = time.monotonic() + TIMEOUT
    while True:
        try:
            return call()
        except zoe.storage.Unavailable:
            if time.monotonic() > deadline:
                raise
            time.sleep(0.05)


def check_storage(storage, oid, rids):
    """Stage, commit, abort and query the text deltas on `storage`."""
    assert storage.text_supported()
    first = rids["/site/item-0"]
    second = rids["/site/item-1"]
    before = after(storage.lastTransaction())
    tid = commit(
        storage,
        lambda t: (
            storage.text_set(oid, "Title", first, "en", "kalo mite", before, t),
            storage.text_set(oid, "Title", second, "en", "kalo", before, t),
        ),
    )
    now = after(tid)
    found = settled(lambda: storage.text_query(oid, now, clause("Title", ["mite"])))
    assert found["r"] == [first]
    assert found["sc"][0] > 0
    found = settled(
        lambda: storage.text_query(oid, now, clause("Title", ["kalo"]), [second, 99])
    )
    assert found["r"] == [second]
    info = settled(lambda: storage.text_info(oid, "Title", now, first))
    assert (info["lang"], info["text"]) == ("en", "kalo mite")
    assert info["docs"] == 2
    # A text clause in a plan of `catalog_query`, once the mirror is built.
    query = plan(
        [
            {
                "k": "v",
                "i": "portal_type",
                "q": ["Document"],
                "and": False,
                "r": None,
                "not": [],
            },
            {"k": "t", **clause("Title", ["kalo"])},
        ]
    )
    deadline = time.monotonic() + TIMEOUT
    while True:
        reply = settled(lambda: storage.catalog_query(oid, now, query))
        if reply.get("f") is None:
            break
        assert time.monotonic() < deadline, reply
        time.sleep(0.05)
    assert reply["r"] == [first]
    assert len(reply["sc"]) == 1
    # An abort leaves nothing.
    t = TransactionMetaData()
    storage.tpc_begin(t)
    storage.text_set(oid, "Title", second, "en", "ruso", now, t)
    storage.tpc_abort(t)
    later = after(storage.lastTransaction())
    assert storage.text_query(oid, later, clause("Title", ["ruso"]))["r"] == []
    # An unindex.
    tid = commit(
        storage, lambda t: storage.text_unindex(oid, "Title", second, later, t)
    )
    found = settled(
        lambda: storage.text_query(oid, after(tid), clause("Title", ["kalo"]))
    )
    assert found["r"] == [first]
    return after(tid)


def refused(storage, error, stage):
    """Tell that `stage(t)` in a transaction of `storage` raises `error`,
    at the call or at the vote."""
    t = TransactionMetaData()
    storage.tpc_begin(t)
    try:
        with pytest.raises(error) as raised:
            stage(t)
            storage.tpc_vote(t)
    finally:
        storage.tpc_abort(t)
    return raised.value


def check_errors(storage, oid, rids, now):
    """Each error of the core that a storage with its default limits gives
    raises its Python class on `storage`."""
    first = rids["/site/item-0"]
    for bad in (
        lambda t: storage.text_set(oid, "Title", 1 << 40, "en", "x", now, t),
        lambda t: storage.text_set(oid, "", first, "en", "x", now, t),
        lambda t: storage.text_set(oid, "Title", first, "e" * 36, "x", now, t),
        lambda t: storage.text_unindex(oid, "Title", -(1 << 31) - 1, now, t),
    ):
        refused(storage, ValueError, bad)
    # Two transactions of one snapshot that write one document.
    commit(
        storage, lambda t: storage.text_set(oid, "Title", first, "en", "vite", now, t)
    )
    conflict = refused(
        storage,
        ConflictError,
        lambda t: storage.text_set(oid, "Title", first, "en", "nemo", now, t),
    )
    assert conflict.oid == oid
    # The undo of a transaction that holds a clear delta.
    commit(storage, lambda t: storage.text_clear(oid, "Title", t))
    undo = storage.undoLog(0, 1)[0]["id"]
    refused(storage, UndoError, lambda t: storage.undo(undo, t))
    last = after(storage.lastTransaction())
    # A catalog with no index.
    with pytest.raises(zoe.storage.Unavailable):
        storage.text_query(b"\xff" * 8, last, clause("Title", ["kalo"]))
    assert issubclass(zoe.storage.Unavailable, StorageError)
    # A snapshot before a clear delta that the index holds.
    cleared = settled(lambda: storage.text_query(oid, last, clause("Title", ["kalo"])))
    assert cleared["r"] == []
    with pytest.raises(ReadConflictError):
        settled(lambda: storage.text_query(oid, now, clause("Title", ["kalo"])))
    # A clause of another form, and the compatibility mode.
    with pytest.raises(ValueError):
        storage.text_query(oid, last, {"i": "Title"})
    compat = {"mode": "compat", "language": ["en"], "boosts": {}}
    with pytest.raises(Unsupported):
        storage.text_query(
            oid, last, clause("Title", ["kalo"], cl=["Title"], set=compat)
        )


def test_zoe_storage_stages_and_queries_text_deltas(tmp_path):
    path = tmp_path / "store"
    oid, rids = make_catalog(path)
    storage = zoe.Storage(str(path))
    try:
        now = check_storage(storage, oid, rids)
        check_errors(storage, oid, rids, now)
    finally:
        storage.close()


def test_a_packed_snapshot_raises_read_conflict_error(tmp_path):
    from ZODB.serialize import referencesf

    path = tmp_path / "store"
    oid, rids = make_catalog(path)
    storage = zoe.Storage(str(path))
    try:
        first = rids["/site/item-0"]
        before = after(storage.lastTransaction())
        commit(
            storage,
            lambda t: storage.text_set(oid, "Title", first, "en", "kalo", before, t),
        )
        middle = after(storage.lastTransaction())
        commit(
            storage,
            lambda t: storage.text_set(oid, "Title", first, "en", "mite", middle, t),
        )
        # The text index caps the pack at its tid: pack again until the
        # pack passes the snapshot.
        deadline = time.monotonic() + TIMEOUT
        while True:
            storage.pack(time.time(), referencesf)
            try:
                settled(
                    lambda: storage.text_query(oid, before, clause("Title", ["kalo"]))
                )
            except ReadConflictError:
                break
            assert time.monotonic() < deadline
            time.sleep(0.1)
    finally:
        storage.close()


def test_the_native_client_stages_and_queries_text_deltas(tmp_path):
    path = tmp_path / "store"
    oid, rids = make_catalog(path)
    with ZoeServer(str(path)) as server:
        remote = native(server)
        try:
            now = check_storage(remote, oid, rids)
            check_errors(remote, oid, rids, now)
        finally:
            remote.close()


def test_text_supported_is_false_against_a_stock_zeo_server(tmp_path):
    import ZEO

    address, stop = ZEO.server(
        path=str(tmp_path / "Data.fs"), zeo_conf={"msgpack": "true"}
    )
    try:
        remote = native(address)
        try:
            assert not remote.text_supported()
        finally:
            remote.close()
    finally:
        stop()


def test_a_stock_client_and_the_asyncio_mode_have_no_text_methods(tmp_path):
    import zoe.zeo

    with ZoeServer(str(tmp_path / "store")) as server:
        stock = client(server)
        asyncio = zoe.zeo.ClientStorage(
            server.address, storage="1", native=False, wait_timeout=TIMEOUT
        )
        try:
            for storage in (stock, asyncio):
                for name in (
                    "text_set",
                    "text_unindex",
                    "text_clear",
                    "text_query",
                    "text_info",
                    "text_supported",
                ):
                    assert not hasattr(storage, name), name
            assert "text" not in asyncio._info["extensions"]
        finally:
            stock.close()
            asyncio.close()
