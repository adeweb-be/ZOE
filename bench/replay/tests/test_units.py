"""Unit tests of the replay: the items that a transaction applies to what it
read, and the starts that the probe records give."""

import hashlib
import json
import subprocess
import sys
import time
import types
from pathlib import Path

import pytest
import transaction
import ZODB
from BTrees.IIBTree import IISet, IITreeSet
from BTrees.IOBTree import IOBucket
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree
from persistent.mapping import PersistentMapping
from ZODB.ConflictResolution import PersistentReference
from ZODB.FileStorage import FileStorage
from ZODB.TimeStamp import TimeStamp

from replay import __main__ as command
from replay import log, report, runner, servers
from replay.gate import gate


def test_a_bucket_takes_the_items_that_the_transaction_changed():
    old = IOBucket({1: {7: 1}, 2: "a", 3: "c"}).__getstate__()
    # Another transaction added 4 and changed 2 before this one committed.
    read = IOBucket({1: {7: 1}, 2: "b", 3: "c", 4: "d"}).__getstate__()
    # This one changed the dict of 1, deleted 3 and added 5.
    own = IOBucket({1: {7: 1, 8: 2}, 2: "a", 5: "e"}).__getstate__()
    state = runner.apply_items(IOBucket, old, read, own)
    merged = IOBucket()
    merged.__setstate__(state)
    assert dict(merged.items()) == {1: {7: 1, 8: 2}, 2: "b", 4: "d", 5: "e"}


def test_a_set_and_a_tree_of_one_bucket_take_the_items_too():
    state = runner.apply_items(
        IISet,
        IISet([1, 2]).__getstate__(),
        IISet([1, 2, 3]).__getstate__(),
        IISet([2, 4]).__getstate__(),
    )
    merged = IISet()
    merged.__setstate__(state)
    assert list(merged) == [2, 3, 4]
    state = runner.apply_items(
        OOBTree,
        OOBTree({"a": 1}).__getstate__(),
        OOBTree({"a": 1, "b": 2}).__getstate__(),
        OOBTree({"a": 1, "c": 3}).__getstate__(),
    )
    tree = OOBTree()
    tree.__setstate__(state)
    assert dict(tree.items()) == {"a": 1, "b": 2, "c": 3}
    state = runner.apply_items(
        IITreeSet,
        None,
        IITreeSet([5]).__getstate__(),
        IITreeSet([6]).__getstate__(),
    )
    tree_set = IITreeSet()
    tree_set.__setstate__(state)
    assert list(tree_set) == [5, 6]


def test_a_value_that_does_not_compare_counts_as_changed():
    # Two persistent references to two objects do not compare.
    one = PersistentReference((b"\0" * 7 + b"\1", None))
    two = PersistentReference((b"\0" * 7 + b"\2", None))
    with pytest.raises(ValueError):
        one == two  # noqa: B015
    state = runner.apply_items(
        IOBucket,
        ((1, one),),
        ((1, one, 2, 5),),
        ((1, two),),
    )
    assert state == ((1, two, 2, 5),)


def test_other_classes_and_trees_of_many_buckets_raise_value_error():
    with pytest.raises(ValueError):
        runner.apply_items(Length, 1, 2, 3)
    with pytest.raises(ValueError):
        runner.apply_items(PersistentMapping, {}, {}, {})
    big = OOBTree({f"k{i:03}": i for i in range(200)})
    db = ZODB.DB(None)
    with db.transaction() as conn:
        # The buckets of a tree of many buckets need oids for its state.
        conn.root()["big"] = big
        state = big.__getstate__()
        with pytest.raises(ValueError):
            runner.apply_items(OOBTree, state, state, state)
    db.close()


def tids(path, count):
    """Commit `count` transactions to a new FileStorage at path. Return the
    commit time of the creation of the root, then the tid and the commit
    time of each transaction."""
    db = ZODB.DB(FileStorage(str(path)))
    manager = transaction.TransactionManager()
    conn = db.open(manager)
    for i in range(count):
        # The commits are apart, so a start between two of them is clear.
        time.sleep(0.05)
        conn.root()["n"] = i
        manager.commit()
    db.close()
    storage = FileStorage(str(path), read_only=True)
    found = [txn.tid for txn in storage.iterator()]
    storage.close()
    times = [TimeStamp(tid).timeTime() for tid in found]
    return times[0], list(zip(found[1:], times[1:], strict=True))


def test_each_tid_starts_at_the_request_that_ends_first_after_it(tmp_path):
    root, [(first, c1), (second, c2)] = tids(tmp_path / "Data.fs", 2)
    # The first request starts after the creation of the root, which stays
    # in the base.
    t1 = (root + c1) / 2
    probes = tmp_path / "instance0.jsonl"
    records = [
        # The request of the first tid: one attempt, it ends just after it.
        {"kind": "request", "t": t1, "seconds": c1 - t1 + 0.001, "finishes": 1},
        # A request that commits nothing.
        {"kind": "request", "t": t1, "seconds": 0.0001, "finishes": 0},
        # The request of the second tid: two attempts.
        {
            "kind": "request",
            "t": t1,
            "seconds": c2 - t1 + 0.002,
            "attempts": 2,
            "finishes": 1,
        },
        {"kind": "snapshot", "t": c2},
    ]
    probes.write_text("".join(json.dumps(r) + "\n" for r in records))
    starts, matched = log.starts_of_probes(tmp_path / "Data.fs", [probes])
    assert matched == {"matched": 2, "unmatched": 0, "requests": 2}
    assert starts[first] == pytest.approx(t1, abs=1e-6)
    # A request of two attempts starts at its first one.
    assert starts[second] == pytest.approx(t1, abs=1e-6)
    found = log.read_log(tmp_path / "Data.fs", starts)
    assert [t.tid for t in found.transactions] == [first, second]
    assert len(found.base) == 1


def test_a_tid_that_no_request_explains_starts_at_its_commit(tmp_path):
    root, [(_first, c1), (second, c2)] = tids(tmp_path / "Data.fs", 2)
    t1 = (root + c1) / 2
    probes = tmp_path / "instance0.jsonl"
    request = {"kind": "request", "t": t1, "seconds": c1 - t1 + 0.0001, "finishes": 1}
    probes.write_text(json.dumps(request) + "\n")
    starts, matched = log.starts_of_probes(tmp_path / "Data.fs", [probes])
    assert matched == {"matched": 1, "unmatched": 1, "requests": 1}
    assert starts[second] == pytest.approx(c2)


def test_a_run_that_replays_no_transaction_does_not_count():
    run = {
        "stack": "zeo",
        "speed": 1,
        "repetition": 1,
        "transactions": 0,
        "conflict_errors": 0,
        "counts": True,
    }
    other = dict(run, stack="zoe-native")
    found = gate({"speeds": [1], "runs": [run, other]})
    assert found.excluded == [("zeo", 1, 1), ("zoe-native", 1, 1)]
    assert found.missing == [("zeo", 1), ("zoe-native", 1)]
    assert found.verdict == "fail"


def test_speeds_must_be_positive_and_distinct(tmp_path):
    empty = log.Log([], [])
    for speeds in [(0,), (1, 1), (-4,)]:
        with pytest.raises(ValueError):
            runner.run(empty, workdir=tmp_path, speeds=speeds)


def test_the_command_refuses_a_run_without_probe_records(tmp_path, capsys):
    tids(tmp_path / "Data.fs", 1)
    (tmp_path / "probe").mkdir()
    line = ["run", "--store", str(tmp_path / "Data.fs"), "--probes"]
    line += [str(tmp_path / "probe"), "--workdir", str(tmp_path / "work")]
    line += ["--out", str(tmp_path / "result.json")]
    assert command.main(line) == 2
    assert "no probe records" in capsys.readouterr().err
    # Probe records of another time explain no transaction.
    probe = {"kind": "request", "t": 0.0, "seconds": 1.0, "finishes": 1}
    (tmp_path / "probe" / "instance0.jsonl").write_text(json.dumps(probe) + "\n")
    assert command.main(line) == 2
    assert "explain no transaction" in capsys.readouterr().err
    assert not (tmp_path / "result.json").exists()


def test_the_next_transaction_of_a_request_starts_at_its_previous_commit(tmp_path):
    root, [(first, c1), (second, c2)] = tids(tmp_path / "Data.fs", 2)
    t1 = (root + c1) / 2
    probes = tmp_path / "instance0.jsonl"
    request = {"kind": "request", "t": t1, "seconds": c2 - t1 + 0.001, "finishes": 2}
    probes.write_text(json.dumps(request) + "\n")
    starts, matched = log.starts_of_probes(tmp_path / "Data.fs", [probes])
    assert matched == {"matched": 2, "unmatched": 0, "requests": 1}
    assert starts[first] == pytest.approx(t1, abs=1e-6)
    assert starts[second] == pytest.approx(c1, abs=1e-6)


def test_the_software_is_unknown_when_git_and_zoe_fail(monkeypatch):
    def fails(*args, **kwargs):
        raise subprocess.CalledProcessError(128, args[0])

    monkeypatch.setattr(servers.subprocess, "run", fails)
    # Python raises ImportError when sys.modules holds None for a module.
    monkeypatch.setitem(sys.modules, "zoe", None)
    monkeypatch.setitem(sys.modules, "zoe._zoe", None)
    found = servers.software()
    assert (found["commit"], found["dirty"], found["zoe_version"]) == (None, None, None)
    assert (found["zoe_package"], found["zoe_extension_sha256"]) == (None, None)


def test_the_extension_is_the_one_that_the_interpreter_loaded(tmp_path, monkeypatch):
    # A package with the extensions of two versions of Python. The one that
    # the interpreter loaded does not sort first.
    package = tmp_path / "zoe"
    package.mkdir()
    (package / "__init__.py").write_text("")
    (package / "_zoe.cpython-312-darwin.so").write_bytes(b"other")
    loaded = package / "_zoe.cpython-313-darwin.so"
    loaded.write_bytes(b"loaded")
    extension = types.SimpleNamespace(__file__=str(loaded))
    zoe = types.SimpleNamespace(__file__=str(package / "__init__.py"), _zoe=extension)
    monkeypatch.setitem(sys.modules, "zoe", zoe)
    monkeypatch.setitem(sys.modules, "zoe._zoe", extension)
    found = servers.software()["zoe_extension_sha256"]
    assert found == hashlib.sha256(b"loaded").hexdigest()


def test_the_extension_of_the_install_is_hashed():
    extension = pytest.importorskip("zoe._zoe")
    digest = hashlib.sha256(Path(extension.__file__).read_bytes()).hexdigest()
    assert servers.software()["zoe_extension_sha256"] == digest


@pytest.mark.parametrize("changed", [False, True])
def test_a_result_records_the_software_again_when_it_changed(
    tmp_path, monkeypatch, changed
):
    before = servers.software()
    after = dict(before, dirty=not before["dirty"]) if changed else dict(before)
    reads = iter([before, after])
    monkeypatch.setattr(servers, "software", lambda serve_args=(): next(reads))
    result = runner.run(log.Log([], []), workdir=tmp_path, stacks=(), speeds=(1,))
    assert result["software"] == before
    assert result.get("software_after") == (after if changed else None)
    path = tmp_path / "result.json"
    path.write_text(json.dumps(result))
    assert report.load(path) == result
