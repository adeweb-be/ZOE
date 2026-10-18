"""Acceptance test of the replay of the batches of a background job.

With queued indexing (ADR 0047), an indexer thread of a Zope process
applies the queue of the catalog in batches: transactions with the note
`zoe.plone: indexing queue` and the system user, which no request makes.
The thread runs one batch after the other. The replay matches each
transaction of a log to a request of the probe records by time, so it
gives a batch the start of a request that was in flight at its commit.
Two batches that ran one after the other then replay as concurrent.

The synthetic log has two batches of the indexer that write one key of
one `PersistentMapping` `a`, one after the other, while two requests are
in flight. Each request commits one transaction of its own, on an object
of its own, after the batches. The test runs the command of the replay on
the store and the probe records, on both stacks at the speed 1, and
checks that:

- the two batches run one after the other: no conflict on `a`, and no
  `ConflictError`, on either stack;
- the same log whose two transactions on `a` are transactions of a
  request (another note, another user) replays as today: they overlap,
  and the second one raises one `ConflictError` on each stack.

The data are synthetic tokens. `just bench replay test` runs this module;
it needs the `zoe` binary of `ZOE_BINARY`.
"""

import dataclasses
import json

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.FileStorage import FileStorage
from ZODB.TimeStamp import TimeStamp

from replay import __main__ as command

# One unit of the schedule, in seconds at the speed 1.
U = 0.25
# The minute of the timed transactions, and the minute of the base.
MINUTE = (2026, 1, 1, 0, 1)
BASE_MINUTE = (2026, 1, 1, 0, 0)
ORIGIN = TimeStamp(*MINUTE, 0.0).timeTime()
# The note and the user of a batch of the indexer of `zoe.plone`.
NOTE = "zoe.plone: indexing queue"
SYSTEM = "System Processes"
# The class of `a`.
MAPPING = "persistent.mapping.PersistentMapping"


@dataclasses.dataclass(frozen=True)
class Step:
    """A timed transaction: its commit in units of U, its change, and
    whether a batch of the indexer made it."""

    commit: int
    key: str
    value: str
    batch: bool


def steps(background):
    """Return the timed transactions. With `background`, the two writes of
    `a` are batches of the indexer; else they are transactions of a
    request."""
    return (
        Step(2, "a", "b1", background),
        Step(4, "a", "b2", background),
        Step(6, "x", "r1", False),
        Step(8, "y", "r2", False),
    )


def tid_at(seconds, minute=MINUTE):
    """Return the tid of the time `seconds` into `minute`."""
    return TimeStamp(*minute, float(seconds)).raw()


def write_store(directory, background):
    """Write the store of the log in `directory`, with its timed
    transactions at fixed tids. Return its path."""
    source_path = directory / "source.fs"
    db = ZODB.DB(FileStorage(str(source_path)))
    manager = transaction.TransactionManager()
    connection = db.open(manager)
    root = connection.root()
    for key in ("a", "x", "y"):
        root[key] = PersistentMapping({"v": "t0"})
    manager.commit()
    for step in steps(background):
        root[step.key]["v"] = step.value
        txn = manager.get()
        if step.batch:
            txn.note(NOTE)
            txn.setUser(SYSTEM, "")
        else:
            txn.note(f"/site/{step.key}/edit")
            txn.setUser("editor", "/site/acl_users")
        manager.commit()
    connection.close()
    db.close()

    # The creation of the root and the first commit are the base.
    tids = [tid_at(0, BASE_MINUTE), tid_at(1, BASE_MINUTE)]
    tids += [tid_at(step.commit * U) for step in steps(background)]
    path = directory / "Data.fs"
    source = FileStorage(str(source_path), read_only=True)
    store = FileStorage(str(path))
    for tid, record in zip(tids, source.iterator(), strict=True):
        store.tpc_begin(record, tid, record.status)
        for data in record:
            store.restore(data.oid, tid, data.data, "", None, record)
        store.tpc_vote(record)
        store.tpc_finish(record)
    store.close()
    source.close()
    return path


def write_probes(directory):
    """Write the probe records of the run: two requests in flight from the
    origin, each with one commit of its own, which end after the two
    writes of `a`."""
    probes = directory / "probe"
    probes.mkdir()
    records = [
        {"kind": "request", "t": ORIGIN, "seconds": 6 * U + 0.01, "finishes": 1},
        {"kind": "request", "t": ORIGIN, "seconds": 8 * U + 0.01, "finishes": 1},
    ]
    (probes / "instance0.jsonl").write_text(
        "".join(json.dumps(record) + "\n" for record in records)
    )
    return probes


def replayed(directory, background):
    """Run the command of the replay on the log, on both stacks at the
    speed 1. Return the runs of the result."""
    store = write_store(directory, background)
    probes = write_probes(directory)
    out = directory / "result.json"
    line = ["run", "--store", str(store), "--probes", str(probes)]
    line += ["--stacks", "zeo,zoe-native", "--speeds", "1"]
    line += ["--workdir", str(directory / "work"), "--out", str(out)]
    code = command.main(line)
    assert code != 2, "the command refused the log"
    return json.loads(out.read_text())["runs"]


def conflicts_on(run, name):
    """Return the conflicts of the class `name` in `run`, without the paths
    that count 0."""
    paths = run["conflicts"].get(name, {})
    return {path: count for path, count in paths.items() if count}


def test_two_batches_of_the_indexer_replay_one_after_the_other(tmp_path):
    runs = replayed(tmp_path, background=True)
    assert sorted(run["stack"] for run in runs) == ["zeo", "zoe-native"]
    for run in runs:
        assert run["transactions"] == 4, run
        assert run["counts"], run
        assert conflicts_on(run, MAPPING) == {}, run
        assert run["conflict_errors"] == 0, run


@pytest.fixture(scope="module")
def request_runs(tmp_path_factory):
    """The runs of the log whose writes of `a` are transactions of a
    request."""
    return replayed(tmp_path_factory.mktemp("requests"), background=False)


@pytest.mark.parametrize("stack", ["zeo", "zoe-native"])
def test_two_transactions_of_requests_still_overlap(request_runs, stack):
    [run] = [run for run in request_runs if run["stack"] == stack]
    assert run["transactions"] == 4, run
    assert conflicts_on(run, MAPPING) == {"refused": 1}, run
    assert run["conflict_errors"] == 1, run
