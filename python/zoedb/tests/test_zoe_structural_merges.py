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


# The shapes of production that the acceptance test of the merges does not
# build: buckets of a tree of many buckets, whose state names the next
# bucket, and buckets whose values are persistent references.


def last_records(storage):
    """Return the oids and the records of the last transaction of storage."""
    records = []
    for txn in storage.iterator():
        records = sorted((record.oid, record.data) for record in txn)
    return records


def differential(tmp_path, name, make, first, second):
    """Run the same race on FileStorage and on zoe.Storage. Return the
    outcome and the records of the last commit of each."""
    from ZODB.FileStorage import FileStorage

    results = []
    for storage in (
        FileStorage(str(tmp_path / f"{name}.fs")),
        zoe.Storage(str(tmp_path / f"{name}-zoe")),
    ):
        outcome, _ = race(storage, make, first, second)
        # race closed the database, and so the storage: open it again.
        if isinstance(storage, zoe.Storage):
            storage = zoe.Storage(str(tmp_path / f"{name}-zoe"))
        else:
            storage = FileStorage(str(tmp_path / f"{name}.fs"), read_only=True)
        try:
            results.append((outcome, last_records(storage)))
        finally:
            storage.close()
    return results


def split_tree():
    from BTrees.OOBTree import OOBTree

    # An OOBucket holds 30 items: 200 keys make a tree of many buckets.
    return OOBTree({f"k{i:03}": i for i in range(200)})


def with_references():
    from BTrees.OOBTree import OOBTree
    from persistent.mapping import PersistentMapping

    shared = PersistentMapping({"v": "s"})
    return OOBTree({"a": PersistentMapping(), "b": shared, "c": shared, "d": 1})


def keywords():
    from BTrees.IOBTree import IOBucket

    return IOBucket({1: ["x", "y"], 2: ["z"], 3: []})


def mixed_sets():
    from BTrees.IIBTree import IITreeSet
    from BTrees.OOBTree import OOBTree

    return OOBTree({"a": 1, "b": IITreeSet([1, 2]), "c": 3})


def setting(key, value):
    return lambda obj: obj.__setitem__(key, value)


def making(key, factory):
    return lambda obj: obj.__setitem__(key, factory())


@pytest.mark.parametrize(
    ("name", "make", "first", "second", "merges"),
    [
        ("split-tree", split_tree, setting("k050", -1), setting("k051", -2), True),
        (
            "split-tree-same-key",
            split_tree,
            setting("k050", -1),
            setting("k050", -2),
            False,
        ),
        (
            "references",
            with_references,
            making("e", lambda: __import__("persistent").mapping.PersistentMapping()),
            setting("f", 2),
            True,
        ),
        ("keywords", keywords, setting(4, ["a"]), setting(5, ["b", "c"]), True),
        ("keywords-change", keywords, setting(1, ["x"]), setting(5, ["b"]), True),
        (
            "mixed-sets",
            mixed_sets,
            setting("d", 4),
            making("e", lambda: __import__("BTrees").IIBTree.IITreeSet([7])),
            True,
        ),
    ],
)
def test_merges_of_production_shapes_equal_the_merges_of_zodb(
    tmp_path, calls, name, make, first, second, merges
):
    (theirs, their_records), (ours, our_records) = differential(
        tmp_path, name, make, first, second
    )
    assert ours == theirs == ("merged" if merges else "conflict")
    # The last commit of each store is the second change, or the first one
    # when the second conflicts.
    assert our_records == their_records
    # FileStorage tried its conflict in Python once, and zoe.Storage never.
    assert calls["calls"] == 1
