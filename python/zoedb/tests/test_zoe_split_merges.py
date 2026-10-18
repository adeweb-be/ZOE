"""The split merge of ADR 0048 on real BTrees: the model of the spike.

Rounds of transactions insert keys into BTrees, as the creations of a site
insert keys into the trees of CMFEditions and of the intid utility:

- ``grow``: an `OOBTree` whose int keys grow, as the history ids, with a
  new persistent object as the value of each key;
- ``scatter``: an `OOBTree` of random str keys, as the version histories;
- ``runs``: an `IOBTree` whose keys come in a run for each transaction, as
  the intids;
- ``deep``: an `OOBTree` of two levels of nodes, whose keys grow;
- ``members``: an `OOTreeSet` whose keys grow.

In each round, each transaction inserts one key into each tree from one
snapshot, then they commit one after the other. From one snapshot, the
inserts into one full bucket all split it, so the rounds make the buckets
split, and the roots that hold their bucket inline split too. With the
switch ``split_merges`` on, every transaction commits at the first attempt.
After each round:

- in each connection that committed, `BTrees.check`, `_check` and a lookup
  of every committed key pass on each tree, so a stale cache of a new
  bucket that the vote merged shows;
- in a new connection of another process, the same checks pass.

Without the switch, the same rounds raise `ConflictError`.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with the
native client and with `ZEO.ClientStorage`. A stock `ZEO.ClientStorage`
does not declare `server_writes`, so two splits of one bucket raise
`ConflictError` for it (ADR 0049), and every committed key stays in the
tree. In-process, the store closes
for the check of the other process, which opens it read-only.
"""

import itertools
import json
import os
import pathlib
import random
import subprocess
import sys

import pytest
import transaction
import ZODB
from BTrees.IOBTree import IOBTree
from BTrees.OOBTree import OOBTree, OOTreeSet
from persistent.mapping import PersistentMapping
from ZODB.POSException import ConflictError

import zoe
import zoe.policy
import zoe.testing
from zoe.testing import ZoeServer

# The transactions of a round.
CREATORS = 6
# The rounds of a test.
ROUNDS = 8
# The longest time that a test waits for the server, in seconds.
TIMEOUT = 30
# The seed of the random keys.
SEED = 4219
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "native-client", "zeo-client")

# The check of the other process. It opens the store of its first argument,
# checks the trees against the keys of the JSON file of its second argument
# with `check` of this module, from the directory of its third argument,
# and prints "ok".
CHECK = """
import json, sys
import ZODB
target, path = sys.argv[1], sys.argv[2]
sys.path.insert(0, sys.argv[3])
from test_zoe_split_merges import check
if target.startswith("zeo:"):
    from ZEO.ClientStorage import ClientStorage
    host, port = target[4:].rsplit(":", 1)
    storage = ClientStorage((host, int(port)), storage="1", wait_timeout=30)
else:
    import zoe
    storage = zoe.Storage(target, read_only=True)
db = ZODB.DB(storage)
try:
    with open(path) as file:
        expected = json.load(file)
    with db.transaction() as conn:
        check(conn.root(), {name: set(keys) for name, keys in expected.items()})
finally:
    db.close()
print("ok")
"""


def check(root, expected):
    """Check each tree of `root`: its structure, its keys, and a lookup of
    each key of `expected`."""
    import BTrees.check

    for name, keys in expected.items():
        tree = root[name]
        BTrees.check.check(tree)
        tree._check()
        assert set(tree.keys()) == keys, name
        for key in keys:
            assert key in tree, (name, key)


class Keys:
    """The keys that each transaction inserts: one key for each tree."""

    def __init__(self):
        self.rng = random.Random(SEED)
        self.grow = itertools.count(1_000_000)
        self.deep = itertools.count(1_000_000)
        self.members = itertools.count(1_000_000)
        self.runs = {}

    def scatter(self):
        return f"{self.rng.randrange(10**10):010d}"

    def run(self, creator):
        """The next key of the run of `creator`."""
        start = self.runs.get(creator, creator * 1_000_000)
        self.runs[creator] = start + 1
        return start


def setup(conn, mode, keys):
    """Install the merge policy and make the trees in `conn`. Return the
    keys of each tree."""
    if mode is not None:
        zoe.policy.install(conn, {"classes": {}, "split_merges": mode})
    root = conn.root()
    expected = {name: set() for name in ("grow", "scatter", "runs", "deep", "members")}
    root["grow"] = OOBTree()
    root["scatter"] = OOBTree()
    root["runs"] = IOBTree()
    root["deep"] = OOBTree()
    root["members"] = OOTreeSet()
    # The roots of grow, runs and members hold their bucket inline, a few
    # keys below its maximum: the rounds split it.
    for _ in range(25):
        key = next(keys.grow)
        root["grow"][key] = PersistentMapping()
        expected["grow"].add(key)
        key = next(keys.members)
        root["members"].add(key)
        expected["members"].add(key)
    for _ in range(55):
        key = keys.run(-1)
        root["runs"][key] = "intid"
        expected["runs"].add(key)
    for _ in range(300):
        key = keys.scatter()
        root["scatter"][key] = "history"
        expected["scatter"].add(key)
    # Sequential keys leave buckets half full. 9,000 of them make more
    # buckets than one node holds, so the root holds nodes.
    for _ in range(9000):
        key = next(keys.deep)
        root["deep"][key] = 1
        expected["deep"].add(key)
    return expected


def one_round(db, keys, expected):
    """Run one round on `db`. Return the outcome of each transaction: True
    when it committed. Each key of a transaction that committed goes to
    `expected`."""
    managers = [transaction.TransactionManager() for _ in range(CREATORS)]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        # Each transaction begins before any of them commits: one snapshot.
        for manager in managers:
            manager.begin()
        inserted = []
        for creator, conn in enumerate(conns):
            root = conn.root()
            mine = {
                "grow": next(keys.grow),
                "scatter": keys.scatter(),
                "runs": keys.run(creator),
                "deep": next(keys.deep),
                "members": next(keys.members),
            }
            root["grow"][mine["grow"]] = PersistentMapping()
            root["scatter"][mine["scatter"]] = "history"
            root["runs"][mine["runs"]] = "intid"
            root["deep"][mine["deep"]] = 1
            root["members"].add(mine["members"])
            inserted.append(mine)
        outcomes = []
        for manager, mine in zip(managers, inserted, strict=True):
            try:
                manager.commit()
            except ConflictError:
                manager.abort()
                outcomes.append(False)
            else:
                outcomes.append(True)
                for name, key in mine.items():
                    expected[name].add(key)
        # The connections that committed see every committed key.
        for manager, conn in zip(managers, conns, strict=True):
            manager.begin()
            check(conn.root(), expected)
            manager.abort()
        return outcomes
    finally:
        for conn in conns:
            conn.close()


def other_process(target, expected, tmp_path):
    """Check the trees in a new connection of another process."""
    path = tmp_path / "expected.json"
    path.write_text(json.dumps({name: sorted(keys) for name, keys in expected.items()}))
    here = str(pathlib.Path(__file__).resolve().parent)
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    done = subprocess.run(
        [sys.executable, "-c", CHECK, target, str(path), here],
        capture_output=True,
        text=True,
        timeout=120,
        cwd=tmp_path,
        env=environ,
    )
    assert done.returncode == 0, done.stderr
    assert done.stdout.strip().endswith("ok"), done.stdout


def split_lines(path):
    """Return the lines of the resolution log at `path` that the split
    merge wrote, as dicts."""
    if not path.exists():
        return []
    lines = [json.loads(line) for line in path.read_text().splitlines()]
    return [line for line in lines if "split" in line]


def run_rounds(mode, switch, tmp_path):
    """Run the rounds in `mode` with `split_merges` set to `switch`, or
    without a policy when it is None. Return the outcomes of each round
    and the lines of the split merge in the resolution log."""
    store = tmp_path / "store"
    log = tmp_path / "merges.log"
    keys = Keys()
    rounds = []
    if mode == "in-process":

        def open_db():
            return ZODB.DB(zoe.Storage(str(store), resolution_log=str(log)))

        db = open_db()
        try:
            with db.transaction() as conn:
                expected = setup(conn, switch, keys)
            for _ in range(ROUNDS):
                rounds.append(one_round(db, keys, expected))
                # The other process opens the store when this one closed it.
                db.close()
                other_process(str(store), expected, tmp_path)
                db = open_db()
        finally:
            db.close()
        return rounds, split_lines(log)
    server = ZoeServer(path=store, args=["--resolution-log", str(log)])
    with server:
        host, port = server.address
        if mode == "native-client":
            storage = zoe.testing.native_client(
                server.address, storage="1", wait_timeout=TIMEOUT
            )
        else:
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
        db = ZODB.DB(storage)
        try:
            with db.transaction() as conn:
                expected = setup(conn, switch, keys)
            for _ in range(ROUNDS):
                rounds.append(one_round(db, keys, expected))
                other_process(f"zeo:{host}:{port}", expected, tmp_path)
        finally:
            db.close()
    return rounds, split_lines(log)


@pytest.mark.parametrize("mode", MODES[:2])
def test_with_the_split_merge_every_creation_commits_at_the_first_attempt(
    mode, tmp_path
):
    rounds, lines = run_rounds(mode, "on", tmp_path)
    assert all(all(outcomes) for outcomes in rounds), rounds
    # The rounds split buckets, so the rules merged. Rule 3 merges a split of
    # one bucket, also of a bucket inline in a root. Rule 1 lets the root of
    # a tree of two levels, or of random keys, through.
    rules = {line["split"] for line in lines if line["applied"]}
    assert {1, 3} <= rules, lines
    assert all(line["outcome"] in ("merged", "passed") for line in lines), lines


def test_without_the_switch_the_rounds_raise_conflict_error(tmp_path):
    rounds, lines = run_rounds("in-process", None, tmp_path)
    assert not all(all(outcomes) for outcomes in rounds), rounds
    assert lines == []


def test_in_shadow_mode_the_rounds_raise_conflict_error_and_log_the_rules(tmp_path):
    rounds, lines = run_rounds("in-process", "shadow", tmp_path)
    assert not all(all(outcomes) for outcomes in rounds), rounds
    # A line of shadow mode gives the outcome of the vote without the rules,
    # and what the rules give in its field `shadow`. A read-current node for
    # which rule 1 gives no answer gets a line that says why.
    assert lines, "the shadow mode logs what the rules give"
    for line in lines:
        assert line["outcome"] != "merged", line
        shadow = line["shadow"]
        if shadow["outcome"] == "undecided":
            assert line["split"] == 1 and shadow["reason"], line
        else:
            assert shadow["outcome"] in ("merged", "passed"), line


def test_a_stock_client_gets_no_merge_of_two_splits_of_one_bucket(tmp_path):
    rounds, lines = run_rounds("zeo-client", "on", tmp_path)
    # Some creations split one bucket from one snapshot: rule 3 gives no
    # answer for them. The checks of each round pass on every committed key.
    assert not all(all(outcomes) for outcomes in rounds), rounds
    assert lines, "the rules ran"
