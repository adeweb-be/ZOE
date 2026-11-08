"""Acceptance test of `catalog_query` of the native client (ADR 0059,
section 9).

A store holds a catalog of Products.ZCatalog and its registration key in
the durable namespace `zoe.catalog`. `zoe.Storage` answers the plans from
the mirror of the process. Then `zoe serve` serves the same store, and
`zoe.zeo.ClientStorage` with the native client inside answers the same
plans from the mirror of the server: the replies are the same, rows and
paths included, and the rows stay lazy. `catalog_supported()` is true on
the native client against `zoe serve` and false against a stock ZEO 6
server. `ZEO.ClientStorage` and the asyncio mode of
`zoe.zeo.ClientStorage` have no `catalog_query`.
"""

import time

import pytest
import transaction
import ZODB
from client_support import native
from support import TIMEOUT, client

import zoe
import zoe.kv
from zoe.testing import ZoeServer

pytest.importorskip("Products.ZCatalog")

NAMESPACE = "zoe.catalog"
TYPES = ("Document", "Folder", "News Item")
COLUMNS = ["getId", "modified", "Type", "Description"]


class Content:
    """An object to catalog: an attribute for each index and column."""

    def __init__(self, **attributes):
        self.__dict__.update(attributes)


def clause(index, *keys):
    return {"k": "v", "i": index, "q": list(keys), "and": False, "r": None, "not": []}


def plan(clauses, sort=(), limit=None, b_start=0, b_size=None, cols=None, rn=0):
    return {
        "c": list(clauses),
        "s": [list(s) for s in sort],
        "l": limit,
        "bs": b_start,
        "bz": b_size,
        "cols": cols,
        "rn": rn,
    }


# The plans that both storages answer, and those that give a reason.
PLANS = [
    plan([clause("portal_type", "Document")]),
    plan([clause("portal_type", "Document", "Folder")], cols=COLUMNS, rn=50),
    plan(
        [clause("portal_type", "News Item")],
        sort=[["getId", True]],
        limit=3,
        cols=COLUMNS,
        rn=3,
    ),
    plan(
        [clause("review_state", "published")],
        sort=[["getId", None]],
        b_start=2,
        b_size=4,
        cols=["getId", "modified"],
        rn=4,
    ),
    plan([{"k": "b", "i": "is_folderish", "q": True}], cols=["getId"], rn=10),
    # A kind of clause that the mirror does not know: `plan`.
    plan([{"k": "x-unknown", "i": "SearchableText", "q": "word"}]),
    # A kind that does not read the class of its index: `plan`.
    plan([{"k": "b", "i": "portal_type", "q": True}]),
    # An index of the catalog that the mirror does not serve.
    plan([clause("nothing_here", "x")]),
]

# Plans that do not have the form of section 8.
MALFORMED = [
    {"c": "not a list"},
    {"s": []},
    plan([{"i": "portal_type"}]),
]


def make_catalog(path):
    """Write a catalog of 30 objects and its registration key to a new
    `zoe.Storage` in `path`. Return the oid of the `Catalog` object."""
    from DateTime import DateTime
    from Products.PluginIndexes.BooleanIndex.BooleanIndex import BooleanIndex
    from Products.PluginIndexes.FieldIndex.FieldIndex import FieldIndex
    from Products.ZCatalog.Catalog import Catalog
    from zope.i18nmessageid import Message

    db = ZODB.DB(zoe.Storage(str(path)))
    try:
        connection = db.open()
        catalog = Catalog()
        for name in ("portal_type", "review_state", "getId"):
            catalog.addIndex(name, FieldIndex(name))
        catalog.addIndex("is_folderish", BooleanIndex("is_folderish"))
        for name in COLUMNS:
            catalog.addColumn(name)
        connection.root()["catalog"] = catalog
        transaction.commit()
        for i in range(30):
            kind = TYPES[i % 3]
            obj = Content(
                portal_type=kind,
                review_state="published" if i % 2 else "private",
                getId=f"item-{i:02d}",
                is_folderish=kind == "Folder",
                modified=DateTime(1767225600 + i * 3600, "GMT+2"),
                Type=Message(kind, domain="plone"),
                Description=f"The item {i}" if i % 4 else None,
            )
            catalog.catalogObject(obj, f"/site/item-{i:02d}")
        zoe.kv.Namespace(connection, "durable", NAMESPACE).set(catalog._p_oid, b"")
        transaction.commit()
        oid = catalog._p_oid
        connection.close()
    finally:
        db.close()
    return oid


def after(tid):
    """The exclusive tid of the snapshot that holds `tid`."""
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


def settled(storage, oid, tid, query):
    """The reply of `query` once the mirror is built."""
    deadline = time.monotonic() + TIMEOUT
    while True:
        reply = storage.catalog_query(oid, tid, query)
        if reply.get("f") not in ("not_ready", "not_registered"):
            return reply
        if time.monotonic() > deadline:
            raise AssertionError(f"the mirror is not ready: {reply}")
        time.sleep(0.05)


def comparable(reply):
    """The reply with its rows as a list, and without the text of a
    fallback, which names a place of the code."""
    out = dict(reply)
    if out.get("rows") is not None:
        out["rows"] = list(out["rows"])
    out.pop("why", None)
    return out


def test_the_native_client_and_zoe_storage_give_the_same_replies(tmp_path):
    path = tmp_path / "store"
    oid = make_catalog(path)
    storage = zoe.Storage(str(path))
    try:
        tid = after(storage.lastTransaction())
        local = [comparable(settled(storage, oid, tid, query)) for query in PLANS]
        for query in MALFORMED:
            with pytest.raises(ValueError):
                storage.catalog_query(oid, tid, query)
    finally:
        storage.close()
    assert local[0]["r"], "the catalog answers"
    assert [r.get("f") for r in local[-3:]] == ["plan", "plan", "index_unavailable"]
    with ZoeServer(str(path)) as server:
        remote = native(server)
        try:
            assert remote.catalog_supported()
            got = [comparable(settled(remote, oid, tid, query)) for query in PLANS]
            assert got == local
            for query in MALFORMED:
                with pytest.raises(ValueError):
                    remote.catalog_query(oid, tid, query)
            # The connection stays open after a malformed plan.
            assert comparable(remote.catalog_query(oid, tid, PLANS[0])) == local[0]
        finally:
            remote.close()


def test_the_rows_of_the_native_client_stay_lazy(tmp_path):
    path = tmp_path / "store"
    oid = make_catalog(path)
    with ZoeServer(str(path)) as server:
        remote = native(server)
        try:
            tid = after(remote.lastTransaction())
            reply = settled(remote, oid, tid, PLANS[1])
            rows = reply["rows"]
            assert len(rows) == len(reply["r"]) > 1
            assert rows._decoded == 0
            first = rows[0]
            assert rows._decoded == 1
            assert first[0] == reply["paths"][0].rsplit("/", 1)[1]
            assert type(first[1]).__name__ == "DateTime"
            assert str(first[2]) in TYPES
        finally:
            remote.close()


def test_catalog_supported_is_false_against_a_stock_zeo_server(tmp_path):
    import ZEO

    # The native client speaks msgpack only.
    address, stop = ZEO.server(
        path=str(tmp_path / "Data.fs"), zeo_conf={"msgpack": "true"}
    )
    try:
        remote = native(address)
        try:
            assert not remote.catalog_supported()
        finally:
            remote.close()
    finally:
        stop()


def test_a_stock_client_and_the_asyncio_mode_have_no_catalog_query(tmp_path):
    import zoe.zeo

    with ZoeServer(str(tmp_path / "store")) as server:
        stock = client(server)
        asyncio = zoe.zeo.ClientStorage(
            server.address, storage="1", native=False, wait_timeout=TIMEOUT
        )
        try:
            for storage in (stock, asyncio):
                assert not hasattr(storage, "catalog_query")
                assert not hasattr(storage, "catalog_supported")
            assert "catalog_query" not in asyncio._info["extensions"]
        finally:
            stock.close()
            asyncio.close()
