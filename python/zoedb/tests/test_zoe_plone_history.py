"""Unit tests of the append-only workflow history of `zoe.plone` and of the
derivation of the merge policy (ADR 0046, section 9), without Plone."""

import sys

import pytest
import transaction
import ZODB

import zoe
from zoe.plone import history, merges


def test_a_workflow_history_is_a_mapping_whose_state_is_the_mapping():
    wfh = history.WorkflowHistory({"one": (1,)})
    wfh["two"] = (2,)
    assert dict(wfh.items()) == {"one": (1,), "two": (2,)}
    assert wfh.get("three", ()) == ()
    assert "one" in wfh
    assert len(wfh) == 2
    assert sorted(wfh) == ["one", "two"]
    del wfh["one"]
    assert list(wfh.keys()) == ["two"]
    assert wfh.__getstate__() == {"two": (2,)}


def test_a_change_of_an_item_commits(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            conn.root()["h"] = history.WorkflowHistory()
        with db.transaction() as conn:
            conn.root()["h"]["wf"] = ({"action": None},)
        with db.transaction() as conn:
            assert conn.root()["h"]["wf"] == ({"action": None},)
    finally:
        db.close()


def test_the_keys_of_a_loaded_history_are_interned():
    wfh = history.WorkflowHistory()
    # A str that no code holds, built at run time.
    key = "".join(["zoe-", "workflow-", "x"])
    wfh.__setstate__({key: ()})
    assert next(iter(wfh)) is sys.intern(key)


def test_the_policy_of_the_history_appends_each_workflow():
    assert history.policy() == {
        "classes": {
            "zoe.plone.history.WorkflowHistory": {"tier2": "on", "default": "append"}
        }
    }


def start_before_end(data):
    if data.start > data.end:
        raise ValueError("the end is before the start")


def nested(data):
    def check():
        return data.title

    return check()


def opaque(data):
    return all(data)


def test_an_invariant_couples_the_fields_that_it_reads():
    names = {"start", "end", "title"}
    assert merges.invariant_fields(start_before_end, names) == {"start", "end"}
    assert merges.invariant_fields(nested, names) == {"title"}
    assert merges.invariant_fields(opaque, names) == set()


def test_the_entries_of_the_classes_of_the_types():
    typed = [
        ("m.Page", {"title", "text"}, []),
        ("m.Event", {"title", "start", "end"}, [start_before_end]),
        # A second type of the class takes the same group once.
        ("m.Event", {"start", "end"}, [start_before_end]),
        ("m.Odd", {"title"}, [opaque]),
    ]
    entries = merges.class_entries(typed)
    assert entries == {
        "m.Page": {
            "tier2": "shadow",
            "default": "strict",
            "fields": {"modification_date": "max"},
        },
        "m.Event": {
            "tier2": "shadow",
            "default": "strict",
            "fields": {"modification_date": "max"},
            "coupled": [["end", "start"]],
        },
    }
    assert merges.class_entries(typed[:1], "on")["m.Page"]["tier2"] == "on"


def test_the_overrides_change_and_remove_entries():
    derived = {
        "classes": {
            "m.Page": {
                "tier2": "shadow",
                "default": "strict",
                "fields": {"modification_date": "max"},
            },
            "m.Old": {"tier2": "shadow"},
        }
    }
    overrides = {
        "posting_dicts": "on",
        "classes": {
            "m.Page": {"tier2": "on", "fields": {"subject": "union"}},
            "m.Old": None,
        },
    }
    assert merges.apply_overrides(derived, overrides) == {
        "posting_dicts": "on",
        "classes": {
            "m.Page": {
                "tier2": "on",
                "default": "strict",
                "fields": {"modification_date": "max", "subject": "union"},
            }
        },
    }
    with pytest.raises(ValueError):
        merges.apply_overrides(derived, {"colour": "on"})
    with pytest.raises(ValueError):
        merges.apply_overrides(derived, {"classes": {"m.Page": {"tier2": "maybe"}}})


def test_concurrent_transitions_of_a_history_merge_in_process(tmp_path):
    from zoe import policy

    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            policy.install(conn, history.policy())
            conn.root()["h"] = history.WorkflowHistory({"wf": ("created",)})
        managers = [transaction.TransactionManager() for _ in range(3)]
        conns = [db.open(transaction_manager=tm) for tm in managers]
        for number, (tm, conn) in enumerate(zip(managers, conns, strict=True)):
            tm.begin()
            wfh = conn.root()["h"]
            wfh["wf"] = (*wfh["wf"], f"transition {number}")
        for tm in managers:
            tm.commit()
        for conn in conns:
            conn.close()
        with db.transaction() as conn:
            assert conn.root()["h"]["wf"] == (
                "created",
                "transition 0",
                "transition 1",
                "transition 2",
            )
    finally:
        db.close()
