"""Rule 3 of the split merge keeps the bucket of the other side (ADR 0049).

The order of events of the card, on a real `OOBTree` whose int keys grow:

1. S1 splits the last bucket A of the tree into A and Y.
2. S2 read the tree before S1 and splits A too.
3. T read the tree after S1, and inserts its key into Y.

With a client that declares `server_writes` (`zoe.Storage` under a ZODB
database, and the native mode of `zoe.zeo.ClientStorage`), S2 merges into
A and Y, and T commits. A second Connection of the process of S2 that holds
Y in its object cache sees the key of S2 after the commit. The finish gives
the Connections every oid of the commit in one invalidation.

Without the declaration (a stock `ZEO.ClientStorage`, and the asyncio mode
of `zoe.zeo.ClientStorage`), rule 3 gives no answer: S2 raises
`ConflictError`, and no bucket leaves the tree.

An undo of T after S2 never passes with no effect: it takes effect, or it
fails.
"""

import pytest
import transaction
import ZODB
from BTrees.OOBTree import OOBTree
from ZODB.POSException import ConflictError, UndoError

import zoe
import zoe.policy
import zoe.testing
import zoe.zeo
from zoe.testing import ZoeServer

# The longest time that a test waits for the server, in seconds.
TIMEOUT = 30
# The modes: in-process, and through zoe serve with each client.
DECLARING = ("in-process", "native-client", "native-client-without-cache")
STOCK = ("asyncio-client", "zeo-client")


class Opened:
    """A database of `mode` on a store in `tmp_path`, and its server."""

    def __init__(self, mode, tmp_path):
        self.mode = mode
        self.server = None
        store = tmp_path / "store"
        if mode == "in-process":
            self.db = ZODB.DB(zoe.Storage(str(store)))
            return
        self.server = ZoeServer(path=store)
        self.server.__enter__()
        address = self.server.address
        if mode == "native-client":
            storage = zoe.testing.native_client(
                address, storage="1", wait_timeout=TIMEOUT
            )
        elif mode == "native-client-without-cache":
            storage = zoe.testing.native_client(
                address, storage="1", wait_timeout=TIMEOUT, cache_size=0
            )
        elif mode == "asyncio-client":
            storage = zoe.zeo.ClientStorage(
                address, storage="1", wait_timeout=TIMEOUT, native=False
            )
        else:
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(address, storage="1", wait_timeout=TIMEOUT)
        self.db = ZODB.DB(storage)

    def close(self):
        self.db.close()
        if self.server is not None:
            self.server.__exit__(None, None, None)


@pytest.fixture
def opened(request, tmp_path):
    db = Opened(request.param, tmp_path)
    yield db
    db.close()


def full_tree(db):
    """Make a tree whose root holds buckets and whose last bucket is full,
    with the split merge on. Return the last key."""
    with db.transaction() as conn:
        zoe.policy.install(conn, {"classes": {}, "split_merges": "on"})
        tree = conn.root()["tree"] = OOBTree()
        for key in range(100):
            tree[key] = key
    maximum = getattr(OOBTree, "max_leaf_size", 30)
    key = 99
    while True:
        with db.transaction() as conn:
            tree = conn.root()["tree"]
            last = tree.__getstate__()[0][-1]
            if len(last) >= maximum:
                return key
            key += 1
            tree[key] = key


def buckets(tree):
    """Return the oids of the buckets of `tree`, in order."""
    return [child._p_oid for child in tree.__getstate__()[0][::2]]


def check(db, keys):
    """Check the tree in a new Connection: its structure and each key."""
    import BTrees.check

    with db.transaction() as conn:
        tree = conn.root()["tree"]
        BTrees.check.check(tree)
        tree._check()
        for key in keys:
            assert key in tree, key


class Card:
    """The transactions of the card, each with its own transaction manager
    and Connection."""

    def __init__(self, db, last):
        self.db = db
        self.keys = [last + 1, last + 2, last + 3]
        self.managers = {}
        self.conns = {}

    def begin(self, name):
        manager = self.managers[name] = transaction.TransactionManager()
        conn = self.conns[name] = self.db.open(transaction_manager=manager)
        manager.begin()
        return conn.root()["tree"]

    def commit(self, name, note=None):
        manager = self.managers[name]
        if note:
            manager.get().note(note)
        try:
            manager.commit()
        except ConflictError:
            manager.abort()
            return False
        return True

    def close(self):
        for conn in self.conns.values():
            conn.close()


def run_card(db, last, reader=None):
    """Run the card on `db`. With `reader`, a Connection loads the tree
    after S1 and keeps Y in its object cache. Return whether S2 and T
    committed, and the card."""
    card = Card(db, last)
    k1, k2, k3 = card.keys
    s2 = card.begin("S2")
    tree = card.begin("S1")
    tree[k1] = k1
    assert card.commit("S1")
    if reader is not None:
        seen = card.begin("reader")
        assert list(seen.keys())[-1] == k1
        reader.update(buckets=buckets(seen))
        card.managers["reader"].abort()
    t = card.begin("T")
    t[k3] = k3
    s2[k2] = k2
    s2_committed = card.commit("S2", "S2")
    if reader is not None and s2_committed:
        # The Connection that holds Y sees the key of S2.
        card.managers["reader"].begin()
        assert k2 in card.conns["reader"].root()["tree"]
        card.managers["reader"].abort()
    t_committed = card.commit("T", "T")
    return s2_committed, t_committed, card


@pytest.mark.parametrize("opened", DECLARING, indirect=True)
def test_the_insert_into_the_bucket_of_the_other_side_commits(opened):
    db = opened.db
    last = full_tree(db)
    reader = {}
    s2, t, card = run_card(db, last, reader)
    try:
        assert (s2, t) == (True, True)
        check(db, card.keys)
        # Y keeps its place: the tree holds the buckets that S1 left.
        with db.transaction() as conn:
            assert buckets(conn.root()["tree"]) == reader["buckets"]
    finally:
        card.close()


@pytest.mark.parametrize("opened", STOCK, indirect=True)
def test_without_server_writes_the_merge_gives_no_answer(opened):
    db = opened.db
    last = full_tree(db)
    s2, t, card = run_card(db, last, {})
    try:
        assert (s2, t) == (False, True)
        k1, _, k3 = card.keys
        check(db, [k1, k3])
    finally:
        card.close()


@pytest.mark.parametrize("opened", DECLARING[:2], indirect=True)
def test_the_finish_gives_every_oid_of_the_commit_in_one_invalidation(opened):
    db = opened.db
    last = full_tree(db)
    adapter = db._mvcc_storage
    events = []
    invalidate = adapter.invalidate
    invalidate_finish = adapter._invalidate_finish

    def recording_invalidate(tid, oids):
        events.append(("invalidate", tid, set(oids)))
        return invalidate(tid, oids)

    def recording_finish(tid, oids, instance):
        events.append(("finish", tid, set(oids)))
        return invalidate_finish(tid, oids, instance)

    adapter.invalidate = recording_invalidate
    adapter._invalidate_finish = recording_finish
    reader = {}
    try:
        s2, t, card = run_card(db, last, reader)
    finally:
        del adapter.invalidate
        del adapter._invalidate_finish
    try:
        assert (s2, t) == (True, True)
        a, y = reader["buckets"][-2:]
        # Only the commit of S2 wrote a record that it did not store: one
        # invalidation with A and Y, before the oids that its Connection
        # stored.
        invalidations = [event for event in events if event[0] == "invalidate"]
        assert len(invalidations) == 1, events
        tid = invalidations[0][1]
        assert {a, y} <= invalidations[0][2], events
        mine = [event for event in events if event[1] == tid]
        assert [event[0] for event in mine] == ["invalidate", "finish"], mine
    finally:
        card.close()


def undo_of(db, description):
    """Undo the last transaction whose description is `description`.
    Return True when the undo committed."""
    entry = next(
        e
        for e in db.undoLog(0, 50)
        if e["description"] in (description, description.encode())
    )
    try:
        db.undo(entry["id"])
        transaction.commit()
    except (UndoError, ConflictError):
        transaction.abort()
        return False
    return True


@pytest.mark.parametrize(
    "opened", ("in-process", "native-client", "zeo-client"), indirect=True
)
def test_an_undo_after_the_merge_never_passes_with_no_effect(opened):
    db = opened.db
    last = full_tree(db)
    card = Card(db, last)
    k1, k2, k3 = card.keys
    try:
        s2 = card.begin("S2")
        tree = card.begin("S1")
        tree[k1] = k1
        assert card.commit("S1")
        t = card.begin("T")
        t[k3] = k3
        assert card.commit("T", "T")
        s2[k2] = k2
        merged = card.commit("S2", "S2")
        assert merged == (opened.mode != "zeo-client")
        if undo_of(db, "T"):
            with db.transaction() as conn:
                assert k3 not in conn.root()["tree"]
        check(db, [k1] + ([k2] if merged else []))
    finally:
        card.close()


def test_only_the_native_mode_declares_server_writes():
    assert "server_writes" not in zoe.zeo.EXTENSIONS
    assert "server_writes" in zoe.zeo._NATIVE_EXTENSIONS
