"""Acceptance test of the log replay (B6) and of the gate report of phase 4.

B6 replays the transactions of a store against ZEO and against `zoe serve`,
and counts the conflicts: raised, resolved natively and resolved in Python.
The success criterion of the specification is the gate of phase 4: the
`ConflictError`s that reach Zope on `zoe serve` are at most 10 % of those
on ZEO.

The harness is the uv project `bench/replay` (package `replay`), beside
`bench/plone` and `bench/shootout`. Its commands:

    just bench replay               # replay a kept store; write
                                    # target/bench/replay/<started>.json
    just bench replay report FILE   # print the report and the verdict
    just bench replay test          # run this module; build `zoe` first

This module drives the interface below. The package `replay` gives each
name.

The log
-------
- `read_log(store, starts) -> Log`. `store` is the path of a FileStorage
  file. `read_log` reads its transactions through the iterator, in tid
  order. `starts` maps the tid of a transaction (8 bytes) to the start of
  that transaction, in seconds since the epoch. A B5 run gives them in its
  probe records. The commit of a transaction is the time of its tid,
  `TimeStamp(tid).timeTime()`.
- `Log.base`: the transactions before the first one that has a start. Each
  run copies them into its new store first, with no timing, and counts
  nothing in them.
- `Log.transactions`: the transactions that have a start (the timed
  transactions), in tid order.
- `read_log` raises `ValueError` for a transaction without a start after
  the first timed transaction.
- Each item of both lists has `tid` (8 bytes), `start` (seconds since the
  epoch, None in the base), `commit` (seconds since the epoch) and `oids`
  (the 8-byte oids of its records).

The run
-------
- `run(log, *, workdir, stacks=STACKS, speeds=SPEEDS, repetitions=1,
  check=check) -> dict` replays the log and returns the result.
- `STACKS == ("zeo", "zoe-native")`. `zeo` is `ZEO.ClientStorage` to a
  ZEO 6 server on a FileStorage; that server resolves conflicts with the
  resolver of ZODB. `zoe-native` is `zoe.zeo.ClientStorage`, the native
  client, to `zoe serve`. `run` starts the servers itself: a new server on
  a new store under `workdir` for each run. The `zoe` binary is
  `ZOE_BINARY`, else `target/release/zoe` of the repository.
- `SPEEDS == (1, 4, 16)`. At the speed s, an event that the log puts t
  seconds after its first start comes t / s seconds after the start of the
  run.
- The runs alternate: for each speed, for each repetition (1 to
  `repetitions`), one run of each stack, in the order of `stacks`.
- A timed transaction reads the serials of its oids at its start and
  stores its records at its commit. Two transactions overlap when one
  starts before the other commits. When both write one oid, the second
  commit is in conflict.
- The log holds each record as the store committed it. So the record of
  the second of two overlapping writes already holds the change of the
  first. That record, stored as it is, conflicts with that change: BTrees
  refuses two inserts of one key, and a bucket conflict could never merge.
  So a transaction stores its own change, applied to the state that it
  read. Its own change goes from the record of the oid before it in the
  log to its own record. When the state that it read is that record before
  it, it stores its own record. When its change does not apply to the
  state that it read, or the oid has no record before it, it stores its
  own record. One way: the `_p_resolveConflict` of the class, with the
  record before it as the old state, the state that it read as the
  committed state and its own record as the new state. This step is not a
  conflict, and no count holds it.
- A commit that raises `ConflictError` gets no acknowledgement, and the
  replay does not retry it. Zope retries it, but the count is of the
  `ConflictError`s that reach Zope.
- After each run, `check` gets a new client of the store of the run and
  the acknowledged transactions.
- Each run also runs a read load. This module does not check it.

The result
----------
A dict that `json.dumps` writes, and that validates against `SCHEMA`, the
path of `bench/schema/replay.schema.json`. It holds at least:

- `schema_version`: 1.
- `speeds`: the speeds of the run.
- `runs`: one item per run, in the order of the runs, with:
  - `stack`, `speed`, `repetition`.
  - `transactions`: the timed transactions that the run replayed.
  - `order`: the tids of the log, as 16 hex digits, in the order in which
    the run ended their commits, acknowledged or refused.
  - `conflicts`: `{class: {path: count}}`. A conflict is one oid in
    conflict at a vote. The class is `module.name`, as the pickle of the
    record names it. The path is what ended the conflict, one of `PATHS`:
    `native` (`zoe serve` resolved it), `python-client` (the resolver of
    ZODB in the process of the replay resolved it), `python-server` (the
    resolver of ZODB in the ZEO server resolved it) or `refused` (nothing
    resolved it, and the commit raised `ConflictError`).
  - `conflict_errors`: the `ConflictError`s that the commits raised, the
    ones that would reach Zope.
  - `check`: `acknowledged`, the acknowledged transactions that the check
    got, and `missing`, the tids (16 hex digits) that the check found
    missing.
  - `counts`: true when the check found nothing missing. A run whose check
    fails does not count.
  - `seconds`: the time from the first start of the schedule to the end
    of the last commit.

The check
---------
- `check(storage, acknowledged) -> list[bytes]`. `acknowledged` lists
  `(tid, oids)` in the order of the acknowledgements: the tid that the
  store gave a commit, and the oids of its records. `check` returns the
  tid of each item in which `storage.loadSerial(oid, tid)` fails for an
  oid, in the order of `acknowledged`.

The gate
--------
- `verdict(zeo, zoe) -> "pass" | "fail"`. `zeo` and `zoe` are the
  `ConflictError`s of each stack, as counts or as rates. It passes when
  `zoe` is at most `LIMIT` (0.1) of `zeo`, 10 % included. When `zeo` is 0,
  it passes only when `zoe` is 0 too.
- `gate(result) -> Gate`. It reads only `speeds` and, in each run,
  `stack`, `speed`, `repetition`, `transactions`, `conflict_errors` and
  `counts`. A `Gate` has:
  - `rates`: `{stack: ConflictErrors per 1,000 transactions}` over the
    runs of the stack that count, for each stack of `STACKS`; None for a
    stack without a run that counts.
  - `excluded`: `(stack, speed, repetition)` of each run that does not
    count, in the order of the runs.
  - `missing`: `(stack, speed)` for each speed of the result and each
    stack of `STACKS`, in that order, without a run that counts.
  - `verdict`: "pass" when nothing is missing and `verdict` of the two
    rates passes, else "fail".
- `python -m replay report FILE` prints one row per run (a run that does
  not count says `does not count`), the conflicts per 1,000 transactions
  by class and by path, and, as its last line, `gate passed: ...` or
  `gate failed: ...`. It exits with 0 when the gate passes, with 1 when it
  fails, and with 2 when FILE is not a result of the replay.

The synthetic log
-----------------
The test writes a FileStorage with ZODB, then copies its transactions
into the store of the log at fixed tids. The base creates the root, then
a `PersistentMapping` `a` = {"v": "t0"}, an `OOBucket` `b` = {"k0": "v0"}
and a `BTrees.Length.Length` `n` = 0. The timed transactions, with their
start and commit in units of U = 0.25 s at the speed 1:

    start  commit  change                        overlap
      0      2     a["v"] = "t1"
      1      3     a["v"] = "t2"                 the same key of a: no merge
      4      6     b["k1"] = "v1"
      5      7     b["k2"] = "v2"                a bucket merge
      8     10     n.change(1)
      9     11     n.change(1)                   a Length merge
     12     13     root["c"] = {"v": "t7"}       none (c is a new mapping)

Two events of the schedule are at least U apart, much more than one
commit takes. So the order of the events does not depend on the machine.
On each stack, each run gives 7 transactions, 6 acknowledgements and one
`ConflictError`. The conflict of `a` is refused. On `zeo`, the ZEO server
resolves the conflicts of `b` and `n` in Python. On `zoe-native`, the
client resolves them in Python, or `zoe serve` resolves them natively
once it merges buckets and `Length`.

The data are synthetic tokens. The classes are those of persistent and
BTrees, so the test needs no Plone. The real runs replay a store that a
B5 run kept, in the interpreter of a Plone instance; this module does not
run them.
"""

import dataclasses
import json
import pathlib
import subprocess
import sys

import jsonschema
import pytest
import transaction
import ZODB
from BTrees.Length import Length
from BTrees.OOBTree import OOBucket
from persistent.mapping import PersistentMapping
from ZODB.FileStorage import FileStorage
from ZODB.TimeStamp import TimeStamp
from ZODB.utils import get_pickle_metadata, p64

import replay

BENCH = pathlib.Path(__file__).resolve().parents[2]
# One unit of the schedule, in seconds at the speed 1.
U = 0.25
# The minute of the timed transactions, and the minute of the base.
MINUTE = (2026, 1, 1, 0, 1)
BASE_MINUTE = (2026, 1, 1, 0, 0)
ORIGIN = TimeStamp(*MINUTE, 0.0).timeTime()
# On zoe-native, a merge of a bucket or of a Length ends in zoe serve or
# in the client.
RESOLVED_ON_ZOE = ({"native": 1}, {"python-client": 1})


def put(key, item, value):
    """Return the change that sets root[key][item] to value."""

    def change(root):
        root[key][item] = value

    return change


def count(key):
    """Return the change that adds 1 to the Length root[key]."""

    def change(root):
        root[key].change(1)

    return change


def add(key, state):
    """Return the change that adds a new mapping to the root."""

    def change(root):
        root[key] = PersistentMapping(state)

    return change


# The timed transactions: start and commit in units of U, then the change.
TIMED = (
    (0, 2, put("a", "v", "t1")),
    (1, 3, put("a", "v", "t2")),
    (4, 6, put("b", "k1", "v1")),
    (5, 7, put("b", "k2", "v2")),
    (8, 10, count("n")),
    (9, 11, count("n")),
    (12, 13, add("c", {"v": "t7"})),
)
# The objects that each timed transaction writes.
WRITES = ({"a"}, {"a"}, {"b"}, {"b"}, {"n"}, {"n"}, {"root", "c"})
# The last commit, in units of U.
END = 13
# The refused transaction: the second write of a.
REFUSED = 1


@dataclasses.dataclass
class Synthetic:
    """The store of the synthetic log and what the test knows of it."""

    path: pathlib.Path
    starts: dict
    base: list
    timed: list
    oids: dict
    classes: dict


def tid_at(seconds, minute=MINUTE):
    """Return the tid of the time `seconds` into `minute`."""
    return TimeStamp(*minute, float(seconds)).raw()


def write_log(directory):
    """Write the store of the synthetic log in directory."""
    source_path = directory / "source.fs"
    db = ZODB.DB(FileStorage(str(source_path)))
    manager = transaction.TransactionManager()
    connection = db.open(manager)
    root = connection.root()
    root["a"] = PersistentMapping({"v": "t0"})
    root["b"] = OOBucket({"k0": "v0"})
    root["n"] = Length(0)
    manager.commit()
    for _start, _commit, change in TIMED:
        change(root)
        manager.commit()
    oids = {key: root[key]._p_oid for key in ("a", "b", "n", "c")}
    oids["root"] = p64(0)
    connection.close()
    db.close()

    # The creation of the root and the first commit are the base.
    base = [tid_at(0, BASE_MINUTE), tid_at(1, BASE_MINUTE)]
    timed = [tid_at(commit * U) for _start, commit, _change in TIMED]
    path = directory / "Data.fs"
    source = FileStorage(str(source_path), read_only=True)
    store = FileStorage(str(path))
    for tid, record in zip(base + timed, source.iterator(), strict=True):
        store.tpc_begin(record, tid, record.status)
        for data in record:
            store.restore(data.oid, tid, data.data, "", None, record)
        store.tpc_vote(record)
        store.tpc_finish(record)
    classes = {
        key: ".".join(get_pickle_metadata(store.load(oids[key])[0]))
        for key in ("a", "b", "n")
    }
    store.close()
    source.close()
    starts = {
        tid: ORIGIN + start * U
        for tid, (start, _commit, _change) in zip(timed, TIMED, strict=True)
    }
    return Synthetic(path, starts, base, timed, oids, classes)


def runs_of(result, stack):
    return [run for run in result["runs"] if run["stack"] == stack]


def nonzero(conflicts):
    """Return the conflicts without the paths that count 0."""
    kept = {}
    for name, paths in conflicts.items():
        counted = {path: n for path, n in paths.items() if n}
        if counted:
            kept[name] = counted
    return kept


def report(path):
    return subprocess.run(
        [sys.executable, "-m", "replay", "report", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )


def write_json(path, document):
    path.write_text(json.dumps(document))
    return path


@pytest.fixture(scope="module")
def synthetic(tmp_path_factory):
    return write_log(tmp_path_factory.mktemp("log"))


@pytest.fixture(scope="module")
def log(synthetic):
    return replay.read_log(synthetic.path, synthetic.starts)


@pytest.fixture(scope="module")
def result(log, tmp_path_factory):
    """Two repetitions of both stacks at the speed 1."""
    workdir = tmp_path_factory.mktemp("work")
    return replay.run(log, speeds=(1,), repetitions=2, workdir=workdir)


@pytest.fixture(scope="module")
def failed(log, tmp_path_factory):
    """One run of zoe-native at the speed 4, with a check that fails.

    The check runs the check of the replay first, then reports the first
    acknowledged transaction as missing.
    """
    calls = []

    def failing(storage, acknowledged):
        calls.append((replay.check(storage, acknowledged), list(acknowledged)))
        return [acknowledged[0][0]]

    workdir = tmp_path_factory.mktemp("failed")
    done = replay.run(
        log, stacks=("zoe-native",), speeds=(4,), workdir=workdir, check=failing
    )
    return done, calls


@pytest.fixture(scope="module")
def schema():
    return json.loads(pathlib.Path(replay.SCHEMA).read_text())


# The constants.


def test_the_constants_of_the_replay():
    assert replay.STACKS == ("zeo", "zoe-native")
    assert replay.SPEEDS == (1, 4, 16)
    assert replay.PATHS == ("native", "python-client", "python-server", "refused")
    assert replay.LIMIT == 0.1
    expected = BENCH / "schema" / "replay.schema.json"
    assert pathlib.Path(replay.SCHEMA).resolve() == expected.resolve()


# The log.


def test_the_log_reads_the_store_through_its_iterator(log, synthetic):
    oids = synthetic.oids
    assert [t.tid for t in log.base] == synthetic.base
    assert all(t.start is None for t in log.base)
    assert set(log.base[0].oids) == {oids["root"]}
    assert set(log.base[1].oids) == {oids[key] for key in ("root", "a", "b", "n")}
    assert [t.tid for t in log.transactions] == synthetic.timed
    for t, (start, commit, _change), writes in zip(
        log.transactions, TIMED, WRITES, strict=True
    ):
        assert t.start == synthetic.starts[t.tid]
        assert t.commit == pytest.approx(TimeStamp(t.tid).timeTime(), abs=1e-6)
        assert t.commit - t.start == pytest.approx((commit - start) * U, abs=1e-6)
        assert set(t.oids) == {oids[key] for key in writes}


def test_a_transaction_without_a_start_after_the_first_timed_one_is_refused(
    synthetic,
):
    first = synthetic.timed[0]
    with pytest.raises(ValueError):
        replay.read_log(synthetic.path, {first: synthetic.starts[first]})


# The runs.


def test_the_runs_alternate_the_stacks(result):
    assert [(r["stack"], r["speed"], r["repetition"]) for r in result["runs"]] == [
        ("zeo", 1, 1),
        ("zoe-native", 1, 1),
        ("zeo", 1, 2),
        ("zoe-native", 1, 2),
    ]
    assert result["speeds"] == [1]


def test_each_run_gives_the_order_of_the_log(result, synthetic):
    order = [tid.hex() for tid in synthetic.timed]
    for run in result["runs"]:
        assert run["transactions"] == len(TIMED), run
        assert run["order"] == order, run


def test_each_run_of_a_stack_gives_the_same_counts(result):
    for stack in replay.STACKS:
        counts = [
            (
                nonzero(run["conflicts"]),
                run["conflict_errors"],
                run["check"]["acknowledged"],
            )
            for run in runs_of(result, stack)
        ]
        assert len(counts) == 2, stack
        assert counts[0] == counts[1], stack


def test_each_run_counts_the_conflicts_by_class_and_by_path(result, synthetic):
    classes = synthetic.classes
    for run in result["runs"]:
        for paths in run["conflicts"].values():
            assert set(paths) <= set(replay.PATHS), run
        assert set(nonzero(run["conflicts"])) == set(classes.values()), run


def test_two_writes_that_cannot_merge_raise_one_conflict_error_on_both_stacks(
    result, synthetic
):
    mapping = synthetic.classes["a"]
    for run in result["runs"]:
        assert nonzero(run["conflicts"])[mapping] == {"refused": 1}, run
        assert run["conflict_errors"] == 1, run


def test_a_bucket_conflict_resolves_in_python_in_the_zeo_server(result, synthetic):
    bucket = synthetic.classes["b"]
    for run in runs_of(result, "zeo"):
        assert nonzero(run["conflicts"])[bucket] == {"python-server": 1}, run
    for run in runs_of(result, "zoe-native"):
        assert nonzero(run["conflicts"])[bucket] in RESOLVED_ON_ZOE, run


def test_a_length_conflict_resolves_on_both_stacks(result, synthetic):
    length = synthetic.classes["n"]
    for run in runs_of(result, "zeo"):
        assert nonzero(run["conflicts"])[length] == {"python-server": 1}, run
    for run in runs_of(result, "zoe-native"):
        assert nonzero(run["conflicts"])[length] in RESOLVED_ON_ZOE, run


def test_every_acknowledged_transaction_loads_after_each_run(result):
    for run in result["runs"]:
        assert run["check"]["acknowledged"] == len(TIMED) - 1, run
        assert run["check"]["missing"] == [], run
        assert run["counts"] is True, run


def test_a_run_follows_the_schedule_of_its_speed(result, failed):
    for run in result["runs"]:
        assert END * U - 0.01 <= run["seconds"] < 2 * END * U, run
    # At the speed 4, the same schedule takes a quarter of the time.
    done, _calls = failed
    (run,) = done["runs"]
    assert END * U / 4 - 0.01 <= run["seconds"] < END * U / 2, run


def test_a_faster_replay_keeps_the_order_of_the_log(failed, synthetic):
    done, _calls = failed
    (run,) = done["runs"]
    assert (run["stack"], run["speed"], run["repetition"]) == ("zoe-native", 4, 1)
    assert run["order"] == [tid.hex() for tid in synthetic.timed]


def test_the_result_is_json_and_matches_the_schema(result, failed, schema):
    for document in (result, failed[0]):
        assert json.loads(json.dumps(document)) == document
        assert document["schema_version"] == 1
        jsonschema.validate(document, schema)


# The check.


def test_the_check_finds_an_acknowledged_transaction_that_does_not_load(synthetic):
    oids = synthetic.oids
    first, last = synthetic.timed[0], synthetic.timed[-1]
    # No commit of the log has this tid, and no record has this oid.
    absent_tid = tid_at(1.5 * U)
    absent_oid = p64(1000)
    storage = FileStorage(str(synthetic.path), read_only=True)
    try:
        assert replay.check(storage, [(first, [oids["a"]])]) == []
        assert replay.check(storage, [(last, [oids["root"], oids["c"]])]) == []
        acknowledged = [
            (first, [oids["a"]]),
            (absent_tid, [oids["a"]]),
            (last, [oids["root"], oids["c"]]),
            (last, [oids["c"], absent_oid]),
        ]
        assert replay.check(storage, acknowledged) == [absent_tid, last]
    finally:
        storage.close()


def test_a_run_whose_check_fails_does_not_count(failed, synthetic):
    done, calls = failed
    oids = synthetic.oids
    # The run called the check once, with each acknowledged transaction,
    # and the check of the replay found each one.
    assert len(calls) == 1
    found, acknowledged = calls[0]
    assert found == []
    expected = [
        {oids[key] for key in writes} for k, writes in enumerate(WRITES) if k != REFUSED
    ]
    assert [set(written) for _tid, written in acknowledged] == expected
    tids = [tid for tid, _written in acknowledged]
    assert tids == sorted(tids)
    (run,) = done["runs"]
    assert run["check"]["acknowledged"] == len(TIMED) - 1
    assert run["check"]["missing"] == [tids[0].hex()]
    assert run["counts"] is False
    gate = replay.gate(done)
    assert gate.excluded == [("zoe-native", 4, 1)]
    assert gate.missing == [("zeo", 4), ("zoe-native", 4)]
    assert gate.rates == {"zeo": None, "zoe-native": None}
    assert gate.verdict == "fail"


# The verdict.


@pytest.mark.parametrize(
    ("zeo", "zoe", "expected"),
    [
        (100, 0, "pass"),
        (100, 9, "pass"),
        # Exactly 10 % passes.
        (100, 10, "pass"),
        (10, 1, "pass"),
        (30, 3, "pass"),
        (50.0, 5.0, "pass"),
        (100, 11, "fail"),
        (9, 1, "fail"),
        (50.0, 5.01, "fail"),
        (1, 1, "fail"),
        (1, 2, "fail"),
        # With no ConflictError on ZEO, only none on zoe serve passes.
        (0, 0, "pass"),
        (0, 1, "fail"),
        (0, 0.5, "fail"),
    ],
)
def test_the_verdict(zeo, zoe, expected):
    assert replay.verdict(zeo=zeo, zoe=zoe) == expected


# The gate.


def run_of(stack, speed, repetition, errors, counts=True, transactions=1000):
    """Return a run of a result with only the fields that the gate reads,
    and the others with values of the right type."""
    return {
        "stack": stack,
        "speed": speed,
        "repetition": repetition,
        "transactions": transactions,
        "order": [],
        "conflicts": {},
        "conflict_errors": errors,
        "check": {
            "acknowledged": transactions - errors,
            "missing": [] if counts else ["00" * 8],
        },
        "counts": counts,
        "seconds": 1.0,
    }


def result_of(*runs, speeds=(1, 4)):
    return {"schema_version": 1, "speeds": list(speeds), "runs": list(runs)}


def test_the_gate_passes_when_zoe_serve_has_at_most_10_percent():
    gate = replay.gate(
        result_of(
            run_of("zeo", 1, 1, 40),
            run_of("zoe-native", 1, 1, 2),
            run_of("zeo", 4, 1, 60),
            run_of("zoe-native", 4, 1, 3),
        )
    )
    assert gate.rates == pytest.approx({"zeo": 50.0, "zoe-native": 2.5})
    assert gate.excluded == []
    assert gate.missing == []
    assert gate.verdict == "pass"


def test_the_gate_passes_at_exactly_10_percent_and_fails_above():
    exactly = replay.gate(
        result_of(
            run_of("zeo", 1, 1, 40),
            run_of("zoe-native", 1, 1, 4),
            run_of("zeo", 4, 1, 60),
            run_of("zoe-native", 4, 1, 6),
        )
    )
    assert exactly.rates == pytest.approx({"zeo": 50.0, "zoe-native": 5.0})
    assert exactly.verdict == "pass"
    above = replay.gate(
        result_of(
            run_of("zeo", 1, 1, 40),
            run_of("zoe-native", 1, 1, 5),
            run_of("zeo", 4, 1, 60),
            run_of("zoe-native", 4, 1, 6),
        )
    )
    assert above.rates == pytest.approx({"zeo": 50.0, "zoe-native": 5.5})
    assert above.verdict == "fail"


def test_a_run_that_does_not_count_leaves_the_rates():
    # The second run of zoe-native does not count. Without it, zoe-native
    # has 8 ConflictErrors per 1,000 transactions against 50 on ZEO: 16 %,
    # and the gate fails. The totals, 8 against 100, give 8 %: the gate
    # compares the rates, not the totals.
    gate = replay.gate(
        result_of(
            run_of("zeo", 1, 1, 50),
            run_of("zoe-native", 1, 1, 8),
            run_of("zeo", 1, 2, 50),
            run_of("zoe-native", 1, 2, 900, counts=False),
            speeds=(1,),
        )
    )
    assert gate.excluded == [("zoe-native", 1, 2)]
    assert gate.missing == []
    assert gate.rates == pytest.approx({"zeo": 50.0, "zoe-native": 8.0})
    assert gate.verdict == "fail"


def test_a_speed_without_a_run_that_counts_fails_the_gate():
    gate = replay.gate(
        result_of(
            run_of("zeo", 1, 1, 40),
            run_of("zoe-native", 1, 1, 0),
            run_of("zeo", 4, 1, 60),
            run_of("zoe-native", 4, 1, 0, counts=False),
        )
    )
    assert gate.excluded == [("zoe-native", 4, 1)]
    assert gate.missing == [("zoe-native", 4)]
    assert gate.verdict == "fail"
    absent = replay.gate(result_of(run_of("zeo", 1, 1, 40), speeds=(1,)))
    assert absent.missing == [("zoe-native", 1)]
    assert absent.rates == {"zeo": pytest.approx(40.0), "zoe-native": None}
    assert absent.verdict == "fail"


def test_with_no_conflict_error_on_zeo_only_none_on_zoe_serve_passes():
    none = replay.gate(
        result_of(run_of("zeo", 1, 1, 0), run_of("zoe-native", 1, 1, 0), speeds=(1,))
    )
    assert none.verdict == "pass"
    one = replay.gate(
        result_of(run_of("zeo", 1, 1, 0), run_of("zoe-native", 1, 1, 1), speeds=(1,))
    )
    assert one.verdict == "fail"


def test_the_gate_of_the_synthetic_log_fails(result):
    # Both stacks refuse the same write, so zoe serve has 100 % of the
    # ConflictErrors of ZEO.
    gate = replay.gate(result)
    rate = 1000 * 2 / (2 * len(TIMED))
    assert gate.rates == pytest.approx({"zeo": rate, "zoe-native": rate})
    assert gate.excluded == []
    assert gate.missing == []
    assert gate.verdict == "fail"


# The report.


def test_the_report_prints_the_counts_and_the_verdict(result, synthetic, tmp_path):
    done = report(write_json(tmp_path / "result.json", result))
    assert done.returncode == 1, done.stdout + done.stderr
    for name in synthetic.classes.values():
        assert name in done.stdout, done.stdout
    assert "refused" in done.stdout
    assert "python-server" in done.stdout
    assert "does not count" not in done.stdout
    assert done.stdout.splitlines()[-1].startswith("gate failed:"), done.stdout


def test_the_report_passes_when_zoe_serve_raises_no_conflict_error(
    result, schema, tmp_path
):
    # The real result, with no ConflictError on zoe-native.
    passing = json.loads(json.dumps(result))
    for run in runs_of(passing, "zoe-native"):
        run["conflict_errors"] = 0
        run["check"]["acknowledged"] = run["transactions"]
        for paths in run["conflicts"].values():
            paths.pop("refused", None)
        run["conflicts"] = {k: v for k, v in run["conflicts"].items() if v}
    jsonschema.validate(passing, schema)
    assert replay.gate(passing).verdict == "pass"
    done = report(write_json(tmp_path / "passing.json", passing))
    assert done.returncode == 0, done.stdout + done.stderr
    assert done.stdout.splitlines()[-1].startswith("gate passed:"), done.stdout


def test_the_report_marks_a_run_that_does_not_count(failed, tmp_path):
    done = report(write_json(tmp_path / "failed.json", failed[0]))
    assert done.returncode == 1, done.stdout + done.stderr
    rows = [line for line in done.stdout.splitlines() if "does not count" in line]
    assert len(rows) == 1, done.stdout
    assert "zoe-native" in rows[0]
    assert done.stdout.splitlines()[-1].startswith("gate failed:"), done.stdout


def test_the_report_refuses_a_file_that_is_not_a_result(tmp_path):
    other = write_json(tmp_path / "other.json", {"schema_version": 1})
    assert report(other).returncode == 2
    text = tmp_path / "text.json"
    text.write_text("{")
    assert report(text).returncode == 2
