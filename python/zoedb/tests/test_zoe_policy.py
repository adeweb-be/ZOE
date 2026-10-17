"""Unit tests of the merge policy (ADR 0046): `zoe.policy`, the merges of
fields of `zoe.Storage` in-process, and the check of a merge of shadow mode
against the resolution of ZODB."""

import json

import persistent
import pytest
import transaction
import ZODB
from BTrees.Length import Length
from ZODB.POSException import ConflictError

import zoe
from zoe import policy


class Item(persistent.Persistent):
    """A persistent object with fields, as a content object keeps them."""

    def __init__(self, **fields):
        self.__dict__.update(fields)


ITEM = f"{Item.__module__}.Item"


def test_install_and_read_give_the_policy_back(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            assert policy.read(conn) is None
            version = policy.install(
                conn,
                {
                    "posting_dicts": "on",
                    "classes": {ITEM: {"tier2": "on", "fields": {"n": "sum"}}},
                },
            )
            assert version == 1
        with db.transaction() as conn:
            assert policy.read(conn) == {
                "version": 1,
                "posting_dicts": "on",
                "classes": {ITEM: {"tier2": "on", "fields": {"n": "sum"}}},
            }
            # A switch that the next policy does not give goes away.
            assert policy.install(conn, {"classes": {}}) == 2
        with db.transaction() as conn:
            assert policy.read(conn) == {"version": 2, "classes": {}}
            assert isinstance(conn.root()[policy.NAME], policy.MergePolicy)
    finally:
        db.close()


@pytest.mark.parametrize(
    "bad",
    [
        [],
        {"colour": "on"},
        {"posting_dicts": "sometimes"},
        {"classes": {"Item": {}}},
        {"classes": {ITEM: {"tier2": "maybe"}}},
        {"classes": {ITEM: {"fields": {"n": "average"}}}},
        {"classes": {ITEM: {"coupled": ["ab"]}}},
        {"classes": {ITEM: {"colour": "red"}}},
    ],
)
def test_a_policy_that_breaks_the_rules_raises_value_error(bad):
    with pytest.raises(ValueError):
        policy.validate(bad)


def race(storage, setup, first, second, make=None):
    """Store an `Item` (or make()) in a new database on storage, install
    the policy of `setup`, then let two connections change the object from
    the same snapshot: first commits, then second. Return the outcome of
    second and the fields that a new transaction reads."""
    db = ZODB.DB(storage)
    try:
        with db.transaction() as conn:
            if setup is not None:
                policy.install(conn, setup)
            conn.root()["object"] = make() if make else Item(a=1, b=2, n=10)
        managers = [transaction.TransactionManager() for _ in range(2)]
        conns = [db.open(transaction_manager=tm) for tm in managers]
        for tm in managers:
            tm.begin()
        first(conns[0].root()["object"])
        second(conns[1].root()["object"])
        managers[0].commit()
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
            value._p_activate()
            fields = value() if isinstance(value, Length) else dict(vars(value))
        return outcome, fields
    finally:
        db.close()


def setting(name, value):
    def change(item):
        setattr(item, name, value)

    return change


ON = {"classes": {ITEM: {"tier2": "on", "fields": {"n": "sum"}}}}


def test_two_changes_of_different_fields_both_commit(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    found = race(storage, ON, setting("a", 5), setting("b", 6))
    assert found == ("merged", {"a": 5, "b": 6, "n": 10})


def test_one_field_changed_differently_raises_conflict_error(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    found = race(storage, ON, setting("a", 5), setting("a", 6))
    assert found == ("conflict", {"a": 5, "b": 2, "n": 10})


def test_a_field_of_the_strategy_sum_adds_both_changes(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    found = race(storage, ON, setting("n", 11), setting("n", 13))
    assert found == ("merged", {"a": 1, "b": 2, "n": 14})


def test_without_a_policy_entry_the_class_never_merges(tmp_path):
    storage = zoe.Storage(str(tmp_path / "store"))
    found = race(storage, {"classes": {}}, setting("a", 5), setting("b", 6))
    assert found == ("conflict", {"a": 5, "b": 2, "n": 10})


def lines(path):
    if not path.exists():
        return []
    return [json.loads(line) for line in path.read_text().splitlines()]


def test_shadow_mode_checks_the_merge_against_the_resolver_of_zodb(tmp_path):
    # Tier 1 of `Length` in shadow mode: ZODB's resolver resolves, and the
    # second vote compares its record with the merge of tier 1.
    log = tmp_path / "merges.log"
    storage = zoe.Storage(str(tmp_path / "store"), resolution_log=str(log))
    setup = {"classes": {"BTrees.Length.Length": {"tier1": "shadow"}}}

    def plus(n):
        return lambda length: length.change(n)

    found = race(storage, setup, plus(1), plus(2), make=lambda: Length(10))
    assert found == ("merged", 13)
    merge, verified = lines(log)
    assert (merge["tier"], merge["mode"], merge["outcome"]) == (1, "shadow", "merged")
    assert merge["policy"] == 1
    assert (verified["outcome"], verified["same"], verified["tier"]) == (
        "verified",
        True,
        1,
    )
    assert verified["oid"] == merge["oid"]


def test_tier_2_in_shadow_mode_logs_the_merge_and_raises_the_conflict(tmp_path):
    log = tmp_path / "merges.log"
    storage = zoe.Storage(str(tmp_path / "store"), resolution_log=str(log))
    setup = {"classes": {ITEM: {"tier2": "shadow"}}}
    found = race(storage, setup, setting("a", 5), setting("b", 6))
    # `Item` has no `_p_resolveConflict`: the client raises, and nothing
    # checks the merge.
    assert found == ("conflict", {"a": 5, "b": 2, "n": 10})
    (line,) = lines(log)
    assert (line["tier"], line["mode"], line["outcome"], line["keys"]) == (
        2,
        "shadow",
        "merged",
        ["b"],
    )
