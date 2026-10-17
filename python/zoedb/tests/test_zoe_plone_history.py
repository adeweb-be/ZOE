"""Unit tests of the append-only workflow history of `zoe.plone` and of the
derivation of the merge policy (ADR 0046, section 9), without Plone."""

import enum
import sys

import persistent
import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping
from ZODB.POSException import ConflictError

import zoe
from zoe import policy
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


def test_a_workflow_history_has_the_methods_of_a_mapping():
    wfh = history.WorkflowHistory({"wf": (1,)})
    copy = wfh.copy()
    assert type(copy) is history.WorkflowHistory
    assert copy == {"wf": (1,)}
    assert wfh == {"wf": (1,)}
    wfh.update({"x": ()})
    assert wfh.setdefault("y", (2,)) == (2,)
    assert wfh.pop("wf") == (1,)
    assert wfh.popitem() in {("x", ()), ("y", (2,))}
    wfh.clear()
    assert len(wfh) == 0
    assert dict(PersistentMapping(history.WorkflowHistory({"a": ()}))) == {"a": ()}


def test_a_change_of_an_item_commits(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            conn.root()["h"] = history.WorkflowHistory()
        with db.transaction() as conn:
            conn.root()["h"]["wf"] = ({"action": None},)
        with db.transaction() as conn:
            conn.root()["h"].setdefault("other", ())
        with db.transaction() as conn:
            assert dict(conn.root()["h"]) == {"wf": ({"action": None},), "other": ()}
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


class Doc(persistent.Persistent):
    """Stands for a content object with a stock history."""


def add_status(doc, number):
    """Add a status as `DefaultWorkflowStatus.set` does."""
    statuses = list(doc.workflow_history.get("wf", ()))
    statuses.append(number)
    doc.workflow_history["wf"] = tuple(statuses)


@pytest.mark.parametrize("first", ["transition", "move"])
def test_a_move_and_a_transition_from_one_snapshot_lose_no_status(tmp_path, first):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            policy.install(conn, history.policy())
            doc = Doc()
            doc.workflow_history = PersistentMapping({"wf": (0,)})
            conn.root()["doc"] = doc
        names = ("transition", "move")
        managers = {name: transaction.TransactionManager() for name in names}
        conns = {name: db.open(transaction_manager=managers[name]) for name in names}
        for tm in managers.values():
            tm.begin()
        add_status(conns["transition"].root()["doc"], 1)
        assert history.ensure(conns["move"].root()["doc"])
        second = "move" if first == "transition" else "transition"
        managers[first].commit()
        # Whichever commits second conflicts, on the stock history.
        with pytest.raises(ConflictError):
            managers[second].commit()
        managers[second].abort()
        # Its retry sees the commit of the other one.
        managers[second].begin()
        doc = conns[second].root()["doc"]
        if second == "transition":
            add_status(doc, 1)
        else:
            history.ensure(doc)
        managers[second].commit()
        for conn in conns.values():
            conn.close()
        with db.transaction() as conn:
            moved = conn.root()["doc"].workflow_history
            assert type(moved) is history.WorkflowHistory
            assert moved["wf"] == (0, 1)
    finally:
        db.close()


def test_restore_gives_a_persistent_mapping_back():
    doc = Doc()
    doc.workflow_history = history.WorkflowHistory({"wf": (0,)})
    assert history.restore(doc)
    # The stock class: that of Zope's Persistence when it is there, as
    # Products.CMFCore uses it.
    assert type(doc.workflow_history) is history._stock()
    assert isinstance(doc.workflow_history, PersistentMapping)
    assert dict(doc.workflow_history) == {"wf": (0,)}
    assert not history.restore(doc)


def start_before_end(data):
    if data.start > data.end:
        raise ValueError("the end is before the start")


def nested(data):
    def check():
        return data.title

    return check()


def opaque(data):
    return all(data)


def test_an_invariant_reads_fields():
    names = {"start", "end", "title"}
    assert merges.invariant_fields(start_before_end, names) == {"start", "end"}
    assert merges.invariant_fields(nested, names) == {"title"}
    assert merges.invariant_fields(opaque, names) == set()


def test_an_invariant_couples_every_field_of_its_interface():
    typed = [
        ("m.Page", {"title", "text"}, [], {}),
        # The invariant reads no field by name, as one that calls a helper:
        # the group holds every field of its interface.
        ("m.Event", {"title", "start", "end"}, [(opaque, {"start", "end"})], {}),
        # A second type of the class takes the same group once.
        ("m.Event", {"start", "end"}, [(start_before_end, {"start", "end"})], {}),
        # An invariant of an interface without fields, that reads none.
        ("m.Odd", {"title"}, [(opaque, set())], {}),
        # A field stored under another name takes both names.
        (
            "m.Item",
            {"start", "end"},
            [(start_before_end, {"start"})],
            {"start": "start_date"},
        ),
    ]
    entries = merges.class_entries(typed)
    plain = {
        "tier2": "shadow",
        "default": "strict",
        "fields": {"modification_date": "max"},
    }
    assert entries["m.Page"] == plain
    assert entries["m.Event"] == {**plain, "coupled": [["end", "start"]]}
    assert entries["m.Odd"] == {"tier2": "off", "blocked": True}
    assert entries["m.Item"]["coupled"] == [["end", "start", "start_date"]]
    assert merges.class_entries(typed[:1], "on")["m.Page"]["tier2"] == "on"


DERIVED = {
    "classes": {
        "m.Page": {
            "tier2": "shadow",
            "default": "strict",
            "fields": {"modification_date": "max"},
            "coupled": [["a", "b"]],
        },
        "m.Old": {"tier2": "shadow"},
        "m.Odd": {"tier2": "off", "blocked": True},
    }
}


def test_the_overrides_change_add_and_remove_entries():
    overrides = {
        "posting_dicts": "on",
        "tier2": "on",
        "classes": {
            "m.Page": {"fields": {"subject": "union"}, "coupled": [["c", "d"]]},
            "m.Old": None,
        },
    }
    result, removed = merges.apply_overrides(DERIVED, overrides, {"m.Page", "m.Odd"})
    assert removed == {"m.Old"}
    assert result["posting_dicts"] == "on"
    assert result["classes"]["m.Page"] == {
        "tier2": "on",
        "default": "strict",
        "fields": {"modification_date": "max", "subject": "union"},
        # An override adds its groups to the derived ones.
        "coupled": [["a", "b"], ["c", "d"]],
    }
    assert "m.Old" not in result["classes"]


def test_an_override_does_not_unblock_a_class(caplog):
    result, _ = merges.apply_overrides(
        DERIVED, {"classes": {"m.Odd": {"tier2": "on"}}}, {"m.Odd"}
    )
    assert result["classes"]["m.Odd"] == {"tier2": "off", "blocked": True}
    assert "stays blocked" in caplog.text


@pytest.mark.parametrize(
    "bad",
    [
        [],
        {"colour": "on"},
        {"classes": []},
        {"classes": {"m.Page": ["on"]}},
        {"classes": {"m.Page": {"fields": []}}},
        {"classes": {"m.Page": {"coupled": {}}}},
        {"classes": {"m.Page": {"tier2": "maybe"}}},
    ],
)
def test_overrides_of_another_form_raise_value_error(bad):
    with pytest.raises(ValueError):
        merges.apply_overrides(DERIVED, bad)


def test_two_sites_in_one_database_compose():
    stored = {
        "version": 3,
        "posting_dicts": "on",
        "classes": {
            "m.Page": {"tier2": "on", "coupled": [["a", "b"]], "fields": {"x": "lww"}},
            "m.Other": {"tier2": "on"},
            "m.Odd": {"tier2": "off", "blocked": True},
        },
    }
    new = {
        "classes": {
            "m.Page": {
                "tier2": "shadow",
                "coupled": [["c", "d"]],
                "fields": {"y": "max"},
            },
            "m.Odd": {"tier2": "on"},
        }
    }
    assert merges.compose(stored, new) == {
        "posting_dicts": "on",
        "classes": {
            "m.Page": {
                "tier2": "shadow",
                "fields": {"x": "lww", "y": "max"},
                "coupled": [["a", "b"], ["c", "d"]],
            },
            "m.Other": {"tier2": "on"},
            # A blocked class stays blocked.
            "m.Odd": {"tier2": "off", "blocked": True},
        },
    }
    assert merges.compose(None, new)["classes"]["m.Odd"] == {"tier2": "on"}


class Mode(enum.StrEnum):
    ON = "on"


@pytest.mark.parametrize(
    "bad",
    [
        {"classes": {"m.Page": {"tier2": Mode.ON}}},
        {"posting_dicts": Mode.ON},
        {"classes": {"m.Page": {"fields": []}}},
        {"classes": {"m.Page": {"coupled": {}}}},
        {"classes": {"m.Page": {"blocked": "yes"}}},
    ],
)
def test_validate_takes_plain_values_only(bad):
    with pytest.raises(ValueError):
        policy.validate(bad)


def test_validate_stores_the_constants():
    mode = "".join(["o", "n"])
    checked = policy.validate({"classes": {"m.Page": {"tier2": mode, "fields": {}}}})
    assert checked["classes"]["m.Page"]["tier2"] is policy.MODES[0]
    assert "fields" not in checked["classes"]["m.Page"]


def test_concurrent_transitions_of_a_history_merge_in_process(tmp_path):
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
