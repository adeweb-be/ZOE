"""Acceptance test of the prefetch of the references in the native client.

`just test` runs this module. When the native client loads a record from
the server, it reads the persistent ids of the record after the record went
to its caller, and prefetches the objects of the same database that its
cache does not hold, as `prefetch` does. So a Connection that touches the
children of an object after its load finds them in the cache. The setting
`prefetch-references` of `<zoeclient>`, on by default, turns it off. Its
twin `ZOE_PREFETCH_REFERENCES` gives the value when the section does not
set the key.

A client of ZEO commits a root `PersistentMapping` of 200 children, each a
`PersistentMapping`. A new native client with an empty cache opens a `DB`,
which loads the root. Before the test touches a child, the cache holds every
child, and the server counted 201 loads in `zoe_loads_total` on the admin
port (`--admin-listen`). The server counts a load before its reply goes
out. With the setting off, the cache holds the root alone.
"""

import time

import pytest
import ZODB
import ZODB.config
from client_support import AdminServer
from persistent.mapping import PersistentMapping
from support import client as stock
from support import native, wait_for
from ZODB.utils import z64

CHILDREN = 200
TWIN = "ZOE_PREFETCH_REFERENCES"
# The time that a test gives a prefetch that must not happen. A prefetch on
# the loopback ends well within it.
SETTLE = 0.5


def fill(address):
    """Commit a root of `CHILDREN` children through a client of ZEO.

    Return the oids of the children.
    """
    db = ZODB.DB(stock(address))
    try:
        children = [PersistentMapping({"n": n}) for n in range(CHILDREN)]
        with db.transaction() as conn:
            root = conn.root()
            for n, child in enumerate(children):
                root[f"child{n:03}"] = child
        return {child._p_oid for child in children}
    finally:
        db.close()


def zoeclient(server, *lines):
    """Open the `<zoeclient>` section of `server` with the keys of `lines`."""
    host, port = server.address
    text = "\n".join(
        ["%import zoe", "<zoeclient>", f"server {host}:{port}", *lines, "</zoeclient>"]
    )
    return ZODB.config.storageFromString(text)


def cached(storage):
    """Return the oids of the current entries of the cache of `storage`."""
    return {oid for oid, _ in storage._cache.contents()}


def open_root(storage):
    """Open a DB on `storage`, which loads the root. Return the DB and the
    root, active."""
    db = ZODB.DB(storage)
    root = db.open().root()
    root._p_activate()
    return db, root


def check_prefetched(server, storage, children, loads):
    """Open a DB on `storage` and check that the children of the root are in
    the cache before the test touches one. `loads` is the count of the
    server before the open."""
    db = None
    try:
        db, root = open_root(storage)
        wait_for(lambda: children <= cached(storage), "the prefetch of the children")
        assert server.loads() - loads == CHILDREN + 1
        # Each child comes from the cache.
        values = sorted(child["n"] for child in root.values())
        assert values == list(range(CHILDREN))
        assert server.loads() - loads == CHILDREN + 1
    finally:
        if db is not None:
            db.close()
        else:
            storage.close()


def check_root_alone(server, storage, loads):
    """Open a DB on `storage` and check that the cache holds the root alone.
    `loads` is the count of the server before the open."""
    db = None
    try:
        db, _ = open_root(storage)
        time.sleep(SETTLE)
        assert cached(storage) == {z64}
        assert server.loads() - loads == 1
    finally:
        if db is not None:
            db.close()
        else:
            storage.close()


def test_a_load_prefetches_the_children_of_its_record(tmp_path, monkeypatch):
    monkeypatch.delenv(TWIN, raising=False)
    with AdminServer(tmp_path / "store") as server:
        children = fill(server.address)
        loads = server.loads()
        check_prefetched(server, native(server), children, loads)


def test_the_section_prefetches_by_default(tmp_path, monkeypatch):
    monkeypatch.delenv(TWIN, raising=False)
    with AdminServer(tmp_path / "store") as server:
        children = fill(server.address)
        loads = server.loads()
        check_prefetched(server, zoeclient(server), children, loads)


def test_with_the_setting_off_the_cache_holds_the_root_alone(tmp_path, monkeypatch):
    monkeypatch.delenv(TWIN, raising=False)
    with AdminServer(tmp_path / "store") as server:
        fill(server.address)
        loads = server.loads()
        storage = zoeclient(server, "prefetch-references off")
        check_root_alone(server, storage, loads)


def test_the_twin_turns_the_setting_off(tmp_path, monkeypatch):
    monkeypatch.setenv(TWIN, "off")
    with AdminServer(tmp_path / "store") as server:
        fill(server.address)
        loads = server.loads()
        check_root_alone(server, zoeclient(server), loads)
        loads = server.loads()
        check_root_alone(server, native(server), loads)


def test_the_key_of_the_section_wins_over_its_twin(tmp_path, monkeypatch):
    monkeypatch.setenv(TWIN, "off")
    with AdminServer(tmp_path / "store") as server:
        children = fill(server.address)
        loads = server.loads()
        storage = zoeclient(server, "prefetch-references on")
        check_prefetched(server, storage, children, loads)


@pytest.mark.parametrize("value", ["maybe", "2"])
def test_a_bad_twin_names_the_variable(tmp_path, monkeypatch, value):
    monkeypatch.setenv(TWIN, value)
    with AdminServer(tmp_path / "store") as server:
        with pytest.raises(ValueError, match=TWIN):
            native(server)
