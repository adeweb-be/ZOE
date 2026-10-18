"""The order merge of ADR 0052 on a model of a folder of plone.folder.

The model keeps what plone.folder keeps, with stock classes: a folder
whose attribute annotations, an `OOBTree`, hold the order, a
`PersistentList` of ids, under ``plone.folder.ordered.order``, and the
positions, an `OIBTree` of id to index, under ``plone.folder.ordered.pos``.
A creation runs the code of `DefaultOrdering.notifyAdded`: it appends the
id to the order and gives it the index ``len(order) - 1`` in the positions.
It also sets ``modification_date`` of the folder, and writes a key of the
queue of queued indexing (ADR 0047) where the storage has namespaces.

In each round, each transaction creates one item from one snapshot, then
they commit one after the other. The rounds pass a split of the bucket
that the positions hold inline. After each round, in each connection that
committed and in a new connection of another process, the order holds each
id once, the positions are ``{id: index}`` of the order, and
`BTrees.check` passes on the positions.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with the
native client and with `ZEO.ClientStorage`. A stock `ZEO.ClientStorage` has
no namespaces, so its transactions queue no indexing: the merge gives no
answer, its rounds raise `ConflictError`, and the order and the positions
still agree.
"""

import itertools
import json
import os
import pathlib
import subprocess
import sys

import persistent
import pytest
import transaction
import ZODB
from BTrees.OIBTree import OIBTree
from BTrees.OOBTree import OOBTree
from persistent.list import PersistentList
from ZODB.POSException import ConflictError

import zoe
import zoe.kv
import zoe.policy
import zoe.testing
from zoe.testing import ZoeServer

# The transactions of a round.
CREATORS = 5
# The rounds of a test.
ROUNDS = 6
# The longest time that a test waits for the server, in seconds.
TIMEOUT = 30
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "native-client", "zeo-client")
ORDER_KEY = "plone.folder.ordered.order"
POS_KEY = "plone.folder.ordered.pos"
# The queue of queued indexing of a catalog.
QUEUE = "zoe.plone.indexing:00000000000000ff"


class Folder(persistent.Persistent):
    """A folder with attribute annotations, as a folder of Plone."""

    def __init__(self):
        self.__annotations__ = OOBTree()
        self.__annotations__[ORDER_KEY] = PersistentList()
        self.__annotations__[POS_KEY] = OIBTree()
        self.modification_date = 0


FOLDER = f"{Folder.__module__}.Folder"
# The policy of the tests: the folder merges its date with `max`.
POLICY = {
    "classes": {
        FOLDER: {"tier2": "on", "default": "strict", "fields": {"modification_date": "max"}}
    },
    "split_merges": "on",
    "folder_orders": "on",
}

# The check of the other process. It opens the store of its first argument
# and checks the folder with `check` of this module, from the directory of
# its second argument, and prints "ok".
CHECK = """
import sys
import ZODB
target = sys.argv[1]
sys.path.insert(0, sys.argv[2])
from test_zoe_folder_orders import check
if target.startswith("zeo:"):
    from ZEO.ClientStorage import ClientStorage
    host, port = target[4:].rsplit(":", 1)
    storage = ClientStorage((host, int(port)), storage="1", wait_timeout=30)
else:
    import zoe
    storage = zoe.Storage(target, read_only=True)
db = ZODB.DB(storage)
try:
    with db.transaction() as conn:
        check(conn.root()["folder"])
finally:
    db.close()
print("ok")
"""


def check(folder):
    """Check that the order of `folder` holds each id once and that its
    positions agree with it. Return the order."""
    import BTrees.check

    order = list(folder.__annotations__[ORDER_KEY])
    pos = folder.__annotations__[POS_KEY]
    BTrees.check.check(pos)
    pos._check()
    assert len(order) == len(set(order)), order
    assert dict(pos.items()) == {item: index for index, item in enumerate(order)}
    return order


def inline_limit():
    """Return the most items that an `OIBTree` holds inline, filled in
    order."""
    tree = OIBTree()
    for count in itertools.count(1):
        tree[name(count)] = count
        if len(tree.__getstate__()) == 2:
            return count - 1
    raise AssertionError("no split")


def buckets(tree):
    """Return the number of buckets of `tree`."""
    state = tree.__getstate__()
    if state is None or len(state) == 1:
        return 1
    return len(state[0]) // 2 + 1


def full_at(least):
    """Return the first number of items, at or above `least`, filled in
    order, whose last bucket the next item splits."""
    tree = OIBTree()
    count = 0
    while True:
        before = buckets(tree)
        count += 1
        tree[name(count - 1)] = count - 1
        if count - 1 >= least and buckets(tree) > before:
            return count - 1


def name(number):
    return f"item-{number:05d}"


def create(conn, item, queue=True):
    """Create `item` in the folder of `conn`, as plone.folder does."""
    folder = conn.root()["folder"]
    order = folder.__annotations__[ORDER_KEY]
    pos = folder.__annotations__[POS_KEY]
    order.append(item)
    pos[item] = len(order) - 1
    folder.modification_date += 1
    if queue and zoe.kv._supported(conn):
        zoe.kv.Namespace(conn, "durable", QUEUE).set(item, b"index")


def rename_on_create(conn, item):
    """Create `item` under a temporary id, then rename it, as
    `plone.api.content.create` without an id does: notifyRemoved deletes
    the temporary id and rebuilds the positions, notifyAdded appends."""
    create(conn, "temporary-id")
    folder = conn.root()["folder"]
    order = folder.__annotations__[ORDER_KEY]
    pos = folder.__annotations__[POS_KEY]
    del order[pos["temporary-id"]]
    pos.clear()
    for count, oid in enumerate(order):
        pos[oid] = count
    order.append(item)
    pos[item] = len(order) - 1


def setup(conn, items):
    zoe.policy.install(conn, POLICY)
    folder = conn.root()["folder"] = Folder()
    for number in range(items):
        folder.__annotations__[ORDER_KEY].append(name(number))
        folder.__annotations__[POS_KEY][name(number)] = number


def one_round(db, start, queue=True):
    """Run one round on `db` from the item number `start`. Return the
    outcome of each transaction: True when it committed."""
    managers = [transaction.TransactionManager() for _ in range(CREATORS)]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        for manager in managers:
            manager.begin()
        for turn, conn in enumerate(conns):
            create(conn, name(start + turn), queue)
        outcomes = []
        for manager in managers:
            try:
                manager.commit()
            except ConflictError:
                manager.abort()
                outcomes.append(False)
            else:
                outcomes.append(True)
        # The connections that committed see the order and the positions.
        for manager, conn in zip(managers, conns, strict=True):
            manager.begin()
            check(conn.root()["folder"])
            manager.abort()
        return outcomes
    finally:
        for conn in conns:
            conn.close()


def other_process(target, tmp_path):
    """Check the folder in a new connection of another process."""
    here = str(pathlib.Path(__file__).resolve().parent)
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    done = subprocess.run(
        [sys.executable, "-c", CHECK, target, here],
        capture_output=True,
        text=True,
        timeout=120,
        cwd=tmp_path,
        env=environ,
    )
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip().endswith("ok"), done.stdout


def order_lines(path):
    """Return the lines of the resolution log at `path` of the order
    merge, as dicts."""
    if not path.exists():
        return []
    lines = [json.loads(line) for line in path.read_text().splitlines()]
    return [line for line in lines if "order" in line]


class Site:
    """A database in `mode` on a store in `tmp_path`, with a resolution
    log. `target` names the store for the other process."""

    def __init__(self, mode, tmp_path):
        self.mode = mode
        self.store = tmp_path / "store"
        self.log = tmp_path / "merges.log"
        self.server = None

    def __enter__(self):
        if self.mode == "in-process":
            self.db = ZODB.DB(zoe.Storage(str(self.store), resolution_log=str(self.log)))
            self.target = str(self.store)
            return self
        self.server = ZoeServer(path=self.store, args=["--resolution-log", str(self.log)])
        self.server.__enter__()
        host, port = self.server.address
        if self.mode == "native-client":
            storage = zoe.testing.native_client(
                self.server.address, storage="1", wait_timeout=TIMEOUT
            )
        else:
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(self.server.address, storage="1", wait_timeout=TIMEOUT)
        self.db = ZODB.DB(storage)
        self.target = f"zeo:{host}:{port}"
        return self

    def check_elsewhere(self, tmp_path):
        """Check the folder in another process. In-process, the store
        closes for it, as the other process opens it read-only."""
        if self.mode != "in-process":
            other_process(self.target, tmp_path)
            return
        self.db.close()
        other_process(self.target, tmp_path)
        self.db = ZODB.DB(zoe.Storage(str(self.store), resolution_log=str(self.log)))

    def __exit__(self, *exc):
        self.db.close()
        if self.server is not None:
            self.server.__exit__(*exc)


def run_rounds(mode, tmp_path, queue=True):
    """Run the rounds in `mode` from a folder a few items below the inline
    limit of its positions. Return the outcomes of each round and the lines
    of the order merge."""
    rounds = []
    with Site(mode, tmp_path) as site:
        start = inline_limit() - 3
        with site.db.transaction() as conn:
            setup(conn, start)
        for _ in range(ROUNDS):
            outcomes = one_round(site.db, start, queue)
            rounds.append(outcomes)
            start += CREATORS
            site.check_elsewhere(tmp_path)
    return rounds, order_lines(site.log)


@pytest.mark.parametrize("mode", MODES[:2])
def test_with_the_order_merge_every_creation_commits_at_the_first_attempt(mode, tmp_path):
    rounds, lines = run_rounds(mode, tmp_path)
    assert all(all(outcomes) for outcomes in rounds), rounds
    parts = {line["order"] for line in lines if line["applied"]}
    assert parts == {"list", "pos"}, lines


def test_a_stock_client_queues_no_indexing_and_keeps_the_stock_behaviour(tmp_path):
    rounds, lines = run_rounds("zeo-client", tmp_path)
    assert not all(all(outcomes) for outcomes in rounds), rounds
    assert lines, "the order merge ran"
    assert all(line["outcome"] == "undecided" for line in lines), lines


def test_a_transaction_that_queues_no_indexing_gets_no_merge(tmp_path):
    rounds, lines = run_rounds("in-process", tmp_path, queue=False)
    assert not all(all(outcomes) for outcomes in rounds), rounds
    assert {line["reason"] for line in lines} == {
        "a transaction that does not queue its indexing"
    }, lines


def race(db, first, second):
    """Let two connections change the folder of `db` from one snapshot:
    `first` commits, then `second`. Return the outcome of `second`."""
    managers = [transaction.TransactionManager() for _ in range(2)]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        for manager in managers:
            manager.begin()
        first(conns[0])
        second(conns[1])
        managers[0].commit()
        try:
            managers[1].commit()
        except ConflictError:
            managers[1].abort()
            return False
        managers[1].begin()
        check(conns[1].root()["folder"])
        managers[1].abort()
        return True
    finally:
        for conn in conns:
            conn.close()


@pytest.mark.parametrize("items", [10, 70])
def test_a_rename_on_create_merges_only_in_an_inline_positions_tree(items, tmp_path):
    with Site("in-process", tmp_path) as site:
        with site.db.transaction() as conn:
            setup(conn, items)
        committed = race(
            site.db,
            lambda conn: create(conn, "appended"),
            lambda conn: rename_on_create(conn, "renamed"),
        )
        # In a folder of more than 60 items the rebuild stores new buckets
        # and drops the old ones: the merge gives no answer.
        assert committed == (items <= 10), items
        if not committed:
            with site.db.transaction() as conn:
                rename_on_create(conn, "renamed")
        with site.db.transaction() as conn:
            order = check(conn.root()["folder"])
        assert order[-2:] == ["appended", "renamed"]


@pytest.mark.parametrize("mode", MODES[:2])
def test_a_new_bucket_that_the_merge_renumbers_turns_into_a_ghost(mode, tmp_path):
    """The other side inserts into the first bucket of the positions. The
    transaction splits the full last bucket into it and a new bucket X,
    which holds its ids. No other transaction wrote X, the last bucket or
    the root: the merge renumbers X and stages it again, and the reply of
    the vote lists it, so the Connection that committed turns X, an object
    that it created, into a ghost, then reads it from the store."""
    with Site(mode, tmp_path) as site:
        items = full_at(80)
        with site.db.transaction() as conn:
            setup(conn, items)
        managers = [transaction.TransactionManager() for _ in range(2)]
        conns = [site.db.open(transaction_manager=manager) for manager in managers]
        try:
            for manager in managers:
                manager.begin()
            create(conns[0], "a-first")
            before = buckets(conns[1].root()["folder"].__annotations__[POS_KEY])
            create(conns[1], name(items))
            create(conns[1], name(items + 1))
            pos = conns[1].root()["folder"].__annotations__[POS_KEY]
            assert buckets(pos) == before + 1, "the transaction splits"
            managers[0].commit()
            managers[1].commit()
            managers[1].begin()
            order = check(conns[1].root()["folder"])
            managers[1].abort()
        finally:
            for conn in conns:
                conn.close()
        assert order[-3:] == ["a-first", name(items), name(items + 1)]
        site.check_elsewhere(tmp_path)
    lines = order_lines(site.log)
    assert any(line["order"] == "pos" and line["read"] == line["committed"] for line in lines), lines
