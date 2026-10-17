"""Unit tests of the lines of the resolution log of an undo, in-process and
through `zoe serve` with both clients.

An undo that merges through tier 2 writes one line, marked `"undo":true`,
also when the undo fails before the vote. No line is written twice.
"""

import json

import pytest
import transaction
import ZODB
from persistent import Persistent
from ZODB.POSException import UndoError

import zoe
import zoe.policy
import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30
MODES = ("in-process", "zeo-client", "native-client")


class Item(Persistent):
    """A class that only tier 2 merges: it has no `_p_resolveConflict`."""


CLASS = f"{Item.__module__}.{Item.__qualname__}"


@pytest.fixture
def opened(tmp_path):
    """Open a database in `mode` that writes its resolution log to `log`."""
    servers = []

    def open_db(mode, log):
        path = str(tmp_path / f"store-{mode}")
        if mode == "in-process":
            return ZODB.DB(zoe.Storage(path, resolution_log=str(log)))
        server = ZoeServer(path, args=("--resolution-log", str(log))).start(TIMEOUT)
        servers.append(server)
        kw = dict(storage="1", wait_timeout=TIMEOUT)
        if mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            return ZODB.DB(ClientStorage(server.address, **kw))
        return ZODB.DB(zoe.testing.native_client(server.address, **kw))

    yield open_db
    for server in servers:
        server.kill()


def undo_after(db, tier2, later):
    """Commit an item, then the transaction "T" that changes its field `a`,
    then a later change of the field `later`. Undo "T" and return the
    error of the undo."""
    with db.transaction() as conn:
        zoe.policy.install(
            conn, {"classes": {CLASS: {"tier2": tier2, "default": "strict"}}}
        )
    with db.transaction() as conn:
        item = Item()
        item.a, item.b = "A", "x"
        conn.root()["item"] = item
    with db.transaction("T") as conn:
        conn.root()["item"].a = "B"
    with db.transaction("L") as conn:
        setattr(conn.root()["item"], later, "C")
    (undo_id,) = [e["id"] for e in db.undoLog(0, 10) if e["description"] == "T"]
    manager = transaction.TransactionManager()
    manager.begin()
    db.undo(undo_id, manager.get())
    with pytest.raises(UndoError) as raised:
        manager.commit()
    manager.abort()
    return raised.value


def lines(log):
    return [json.loads(line) for line in log.read_text().splitlines()]


@pytest.mark.parametrize("mode", MODES)
def test_an_undo_in_shadow_mode_of_tier_2_writes_one_line(opened, tmp_path, mode):
    log = tmp_path / f"{mode}.log"
    db = opened(mode, log)
    try:
        # Tier 2 merges in shadow mode only for the log, so the undo goes
        # on as without it: the class has no resolver, and the undo fails.
        undo_after(db, "shadow", "b")
    finally:
        db.close()
    (line,) = lines(log)
    assert (line["tier"], line["mode"], line["outcome"], line["undo"]) == (
        2,
        "shadow",
        "merged",
        True,
    )
    assert line["applied"] is False


@pytest.mark.parametrize("mode", MODES)
def test_a_refusal_of_tier_2_in_an_undo_writes_one_line(opened, tmp_path, mode):
    log = tmp_path / f"{mode}.log"
    db = opened(mode, log)
    try:
        # The later transaction changed the field that the undo gives back.
        undo_after(db, "on", "a")
    finally:
        db.close()
    (line,) = lines(log)
    assert (line["tier"], line["mode"], line["outcome"], line["undo"]) == (
        2,
        "on",
        "refused",
        True,
    )
