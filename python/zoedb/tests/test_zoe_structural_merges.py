"""Unit tests of the structural merges of `zoe.Storage` (tier 1, ADR 0042):
the modes, the mode of a class, a refusal, and the resolution log."""

import json
import threading

import pytest
import transaction
import ZODB
from BTrees.Length import Length
from BTrees.OOBTree import OOBucket
from ZODB.ConflictResolution import ConflictResolvingStorage
from ZODB.POSException import ConflictError

import zoe


@pytest.fixture
def calls(monkeypatch):
    """Count the calls of `tryToResolveConflict`, in every thread."""
    counted = {"calls": 0}
    lock = threading.Lock()
    original = ConflictResolvingStorage.tryToResolveConflict

    def counting(storage, *args, **kwargs):
        with lock:
            counted["calls"] += 1
        return original(storage, *args, **kwargs)

    monkeypatch.setattr(ConflictResolvingStorage, "tryToResolveConflict", counting)
    return counted


def race(storage, make, first, second):
    """Store make() in a new database on storage, then let two connections
    change it from the same snapshot: first commits, then second. Return
    the outcome of second and the value that a new transaction reads."""
    db = ZODB.DB(storage)
    try:
        with db.transaction() as conn:
            conn.root()["object"] = make()
        managers = [transaction.TransactionManager() for _ in range(2)]
        conns = [db.open(transaction_manager=tm) for tm in managers]
        for tm in managers:
            tm.begin()
        first(conns[0].root()["object"])
        second(conns[1].root()["object"])
        managers[0].get().setUser("first")
        managers[0].get().note("the first change")
        managers[0].commit()
        managers[1].get().setUser("second")
        managers[1].get().note("the second change")
        try:
            managers[1].commit()
        except ConflictError:
            managers[1].abort()
            outcome = "conflict"
        else:
            outcome = "merged"
        for conn in conns:
            conn.close()
        with db.transaction() as conn:
            value = conn.root()["object"]
            value = value() if isinstance(value, Length) else dict(value.items())
        return outcome, value
    finally:
        db.close()


def lines(path):
    """Return the JSON objects of the resolution log in path."""
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def plus(n):
    return lambda length: length.change(n)


def test_a_length_merges_in_the_vote_and_logs_one_line(tmp_path, calls):
    log = tmp_path / "merges.log"
    storage = zoe.Storage(str(tmp_path / "store"), resolution_log=str(log))
    assert race(storage, lambda: Length(10), plus(1), plus(2)) == ("merged", 13)
    assert calls["calls"] == 0
    (line,) = lines(log)
    assert line["class"] == "BTrees.Length.Length"
    assert (line["tier"], line["mode"], line["outcome"]) == (1, "on", "merged")
    assert line["applied"] is True
    assert line["storage"] is None
    # ZODB writes the user as the path of the user folder, a space, then
    # the user.
    assert line["user"].endswith("second")
    assert line["description"] == "the second change"
    assert line["committed_user"].endswith("first")
    assert line["committed_description"] == "the first change"


def test_shadow_mode_logs_the_merge_and_lets_python_resolve(tmp_path, calls):
    log = tmp_path / "merges.log"
    storage = zoe.Storage(
        str(tmp_path / "store"), structural_merges="shadow", resolution_log=str(log)
    )
    assert race(storage, lambda: Length(10), plus(1), plus(2)) == ("merged", 13)
    assert calls["calls"] == 1
    (line,) = lines(log)
    assert (line["mode"], line["outcome"], line["applied"]) == (
        "shadow",
        "merged",
        False,
    )


def test_a_class_off_runs_no_merge(tmp_path, calls):
    log = tmp_path / "merges.log"
    storage = zoe.Storage(
        str(tmp_path / "store"),
        structural_merge_classes={"BTrees.Length.Length": "off"},
        resolution_log=str(log),
    )
    assert race(storage, lambda: Length(10), plus(1), plus(2)) == ("merged", 13)
    assert calls["calls"] == 1
    assert lines(log) == []


def test_a_refusal_raises_conflict_error_without_python(tmp_path, calls):
    log = tmp_path / "merges.log"
    storage = zoe.Storage(str(tmp_path / "store"), resolution_log=str(log))

    def change(value):
        return lambda bucket: bucket.__setitem__("a", value)

    found = race(storage, lambda: OOBucket({"a": 1, "b": 2}), change(5), change(6))
    assert found == ("conflict", {"a": 5, "b": 2})
    assert calls["calls"] == 0
    (line,) = lines(log)
    assert (line["outcome"], line["code"], line["reason"]) == (
        "refused",
        1,
        "Conflicting changes",
    )


def test_a_bad_mode_raises_value_error(tmp_path):
    with pytest.raises(ValueError, match="structural_merges"):
        zoe.Storage(str(tmp_path / "one"), structural_merges="sometimes")
    with pytest.raises(ValueError, match="structural_merge_classes"):
        zoe.Storage(str(tmp_path / "two"), structural_merge_classes={"Length": "on"})
