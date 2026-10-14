"""Acceptance test: `pack-gc` and `IExternalGC` through `zoe serve`.

`zoe serve --pack-gc=false` (`ZOE_PACK_GC`) packs every store without
garbage collection, and the key `pack-gc` of a `[storages.NAME]` table sets
it for one store, as `--read-only` and the key `read-only` do. `zoe pack
--pack-gc=false` packs a store directory without garbage collection. The
default stays a pack with garbage collection.

`get_info` lists `IExternalGC` among the interfaces of the store, so a
stock `ZEO.ClientStorage` provides it.
"""

import json
import subprocess
import time

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.POSException import POSKeyError

import zoe
from zoe.testing import ZoeServer

TIMEOUT = 30


def client(server, storage="1", **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage=storage, **kw)


def unreachable_object(db):
    """Commit an object, then unlink it from the root. Return its oid."""
    tm = transaction.TransactionManager()
    conn = db.open(transaction_manager=tm)
    try:
        conn.root()["item"] = PersistentMapping(value=1)
        tm.commit()
        oid = conn.root()["item"]._p_oid
        del conn.root()["item"]
        tm.commit()
    finally:
        conn.close()
    return oid


def kept_after_pack(server, storage="1"):
    """Pack over ZEO after an object became unreachable. Return whether it stays."""
    db = ZODB.DB(client(server, storage))
    try:
        oid = unreachable_object(db)
        db.pack(time.time() + 1)
    finally:
        db.close()
    # A new client has an empty cache, so its load reaches the server.
    fresh = client(server, storage)
    try:
        fresh.load(oid)
    except POSKeyError:
        return False
    finally:
        fresh.close()
    return True


def test_get_info_lists_iexternalgc(tmp_path):
    from ZODB.interfaces import IExternalGC

    with ZoeServer(str(tmp_path / "store")) as server:
        storage = client(server)
        try:
            interfaces = {tuple(item) for item in storage._info["interfaces"]}
            assert ("ZODB.interfaces", "IExternalGC") in interfaces
            assert ("ZODB.interfaces", "IStorageUndoable") not in interfaces
            assert IExternalGC.providedBy(storage)
        finally:
            storage.close()


def test_zoe_serve_packs_with_garbage_collection_by_default(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        assert not kept_after_pack(server)


def test_zoe_serve_pack_gc_false_keeps_an_unreachable_object(tmp_path):
    with ZoeServer(str(tmp_path / "store"), args=["--pack-gc=false"]) as server:
        assert kept_after_pack(server)


def test_zoe_pack_gc_is_the_twin_of_the_option(tmp_path):
    env = {"ZOE_PACK_GC": "false"}
    with ZoeServer(str(tmp_path / "store"), env=env) as server:
        assert kept_after_pack(server)


def test_the_key_pack_gc_of_a_storages_table(tmp_path):
    config = tmp_path / "serve.toml"
    # A JSON string is a basic string of TOML.
    main, other = (json.dumps(str(tmp_path / name)) for name in ("main", "other"))
    config.write_text(
        f"[storages.main]\npath = {main}\npack-gc = false\n\n"
        f"[storages.other]\npath = {other}\n"
    )
    with ZoeServer(args=["--config", str(config)]) as server:
        assert kept_after_pack(server, "main")
        assert not kept_after_pack(server, "other")


@pytest.mark.parametrize("pack_gc", [None, "false"])
def test_zoe_pack_takes_pack_gc(tmp_path, pack_gc):
    path = str(tmp_path / "store")
    db = ZODB.DB(zoe.Storage(path))
    try:
        oid = unreachable_object(db)
    finally:
        db.close()

    command = [ZoeServer.binary(), "pack", "--path", path, "--days", "0"]
    if pack_gc is not None:
        command.append(f"--pack-gc={pack_gc}")
    done = subprocess.run(command, capture_output=True, text=True, timeout=TIMEOUT)
    assert done.returncode == 0, done.stderr

    storage = zoe.Storage(path)
    try:
        if pack_gc == "false":
            storage.load(oid)
        else:
            with pytest.raises(POSKeyError):
                storage.load(oid)
    finally:
        storage.close()
