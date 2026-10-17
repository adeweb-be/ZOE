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


@pytest.mark.parametrize(
    "options",
    [
        {"structural_merges": "shadow"},
        {"structural_merge_classes": {"BTrees.Length.Length": "shadow"}},
        {"posting_dicts": "shadow"},
    ],
)
def test_a_shadow_mode_without_a_resolution_log_raises_value_error(tmp_path, options):
    with pytest.raises(ValueError, match="resolution_log"):
        zoe.Storage(str(tmp_path / "store"), **options)
    log = tmp_path / "merges.log"
    zoe.Storage(str(tmp_path / "store"), resolution_log=str(log), **options).close()


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


def differential(tmp_path, name, make, first, second, **options):
    """Run the same race on FileStorage and on zoe.Storage with `options`.
    Return the outcome and the records of the last commit of each."""
    from ZODB.FileStorage import FileStorage

    results = []
    for storage in (
        FileStorage(str(tmp_path / f"{name}.fs")),
        zoe.Storage(str(tmp_path / f"{name}-zoe"), **options),
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


def postings():
    from BTrees.IOBTree import IOBucket

    return IOBucket({5: 1, 10: {1: 3}, 20: {1: 2}})


def deleting(key):
    return lambda obj: obj.__delitem__(key)


@pytest.mark.parametrize(
    ("name", "make", "first", "second", "merges"),
    [
        # A dict meets None, then an int: BTrees compares them and merges.
        (
            "dict-and-none",
            lambda: __import__("BTrees").IOBTree.IOBucket({1: None, 2: "a"}),
            setting(1, {7: 1}),
            setting(2, "b"),
            True,
        ),
        (
            "int-and-dict",
            lambda: __import__("BTrees").IOBTree.IOBucket({1: 5, 2: "a"}),
            setting(2, "b"),
            setting(1, {7: 1}),
            True,
        ),
        # Both sides change one posting: the rule would merge it.
        (
            "one-posting",
            postings,
            setting(10, {1: 3, 2: 1}),
            setting(10, {1: 3, 3: 4}),
            False,
        ),
        # A delete meets a dict that BTrees must compare.
        ("delete-and-change", postings, setting(5, 2), deleting(10), False),
    ],
)
def test_shadow_mode_of_the_posting_rule_ends_a_conflict_as_zodb_does(
    tmp_path, name, make, first, second, merges
):
    log = tmp_path / f"{name}.log"
    (theirs, their_records), (ours, our_records) = differential(
        tmp_path,
        name,
        make,
        first,
        second,
        posting_dicts="shadow",
        resolution_log=str(log),
    )
    assert ours == theirs == ("merged" if merges else "conflict")
    assert our_records == their_records


def small_tree():
    from BTrees.IIBTree import IIBTree

    # A tree of three items holds its one bucket inline.
    return IIBTree({1: 1, 2: 2, 3: 3})


@pytest.mark.parametrize(
    ("rule", "outcome", "value"),
    [
        ("off", "conflict", {2: 2, 3: 3}),
        ("on", "merged", {2: 2, 3: 3, 4: 4}),
    ],
)
def test_the_first_key_rule_merges_a_delete_of_the_first_key(
    tmp_path, calls, rule, outcome, value
):
    storage = zoe.Storage(str(tmp_path / "store"), posting_dicts=rule)
    found = race(storage, small_tree, deleting(1), setting(4, 4))
    assert found == (outcome, value)
    # BTrees refuses reason 13 in Rust too, so the client never resolves.
    assert calls["calls"] == 0


@pytest.mark.parametrize(
    ("name", "make", "first", "second", "value"),
    [
        (
            "tree-set",
            lambda: __import__("BTrees").IIBTree.IITreeSet([1, 2, 3]),
            lambda tree_set: tree_set.remove(1),
            lambda tree_set: tree_set.add(4),
            [2, 3, 4],
        ),
        (
            "str-keys",
            lambda: __import__("BTrees").OOBTree.OOBTree({"a": 1, "b": 2}),
            deleting("a"),
            setting("c", 3),
            {"b": 2, "c": 3},
        ),
    ],
)
def test_the_first_key_rule_merges_every_family(
    tmp_path, calls, name, make, first, second, value
):
    storage = zoe.Storage(str(tmp_path / name), posting_dicts="on")
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
        for tm in managers:
            tm.commit()
        for conn in conns:
            conn.close()
        with db.transaction() as conn:
            found = conn.root()["object"]
            found = list(found) if isinstance(value, list) else dict(found.items())
    finally:
        db.close()
    assert found == value
    assert calls["calls"] == 0


def test_a_reindex_that_drops_the_lowest_document_of_a_posting_tree_merges(
    tmp_path, calls
):
    from Products.ZCTextIndex import WidCode
    from Products.ZCTextIndex.Lexicon import CaseNormalizer, Lexicon, Splitter
    from Products.ZCTextIndex.OkapiIndex import OkapiIndex

    def index_of(documents):
        lexicon = Lexicon(Splitter(), CaseNormalizer())
        index = OkapiIndex(lexicon)
        for docid, document in documents.items():
            index.index_doc(docid, document)
        return index, lexicon

    def contents(index, lexicon):
        word = lexicon.get_word
        return {
            "wordinfo": {
                word(wid): dict(posting.items())
                for wid, posting in index._wordinfo.items()
            },
            "docweight": dict(index._docweight.items()),
            "docwords": {
                docid: [word(wid) for wid in WidCode.decode(code)]
                for docid, code in index._docwords.items()
            },
            "length": index.length(),
            "document_count": index.document_count(),
            "totaldoclen": index._totaldoclen(),
        }

    # Twelve documents hold the word "common": its postings are a tree of
    # one bucket, whose first key is the document 1.
    documents = {docid: "common alpha beta" for docid in range(1, 13)}
    final = {**documents, 1: "alpha beta", 13: "common alpha beta"}
    results = {}
    for rule in ("off", "on"):
        index, lexicon = index_of(documents)
        storage = zoe.Storage(str(tmp_path / rule), posting_dicts=rule)
        db = ZODB.DB(storage)
        try:
            with db.transaction() as conn:
                conn.root()["lexicon"] = lexicon
                conn.root()["index"] = index
            managers = [transaction.TransactionManager() for _ in range(2)]
            conns = [db.open(transaction_manager=tm) for tm in managers]
            for tm in managers:
                tm.begin()
            # A reindex removes "common" from the document of the lowest
            # id, while another transaction indexes a new document.
            conns[0].root()["index"].index_doc(1, final[1])
            conns[1].root()["index"].index_doc(13, final[13])
            managers[0].commit()
            try:
                managers[1].commit()
            except ConflictError:
                managers[1].abort()
                results[rule] = "conflict"
            else:
                results[rule] = "merged"
            for conn in conns:
                conn.close()
            with db.transaction() as conn:
                root = conn.root()
                stored = contents(root["index"], root["lexicon"])
        finally:
            db.close()
    assert results == {"off": "conflict", "on": "merged"}
    assert stored == contents(*index_of(final))
    # Tier 1 refuses with the rule off, so no client resolves.
    assert calls["calls"] == 0
