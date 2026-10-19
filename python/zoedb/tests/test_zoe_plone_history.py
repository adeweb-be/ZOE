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
            "zoe.plone.history.WorkflowHistory": {
                "tier2": "on",
                "default": "append",
                "options": {"append": {"state": "review_state"}},
            }
        }
    }
    # The policy is a copy: a change of it does not change `ENTRY`.
    history.policy()["classes"]["zoe.plone.history.WorkflowHistory"]["options"][
        "append"
    ]["state"] = "other"
    assert history.ENTRY["options"]["append"]["state"] == "review_state"


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


@pytest.mark.parametrize("first", ["transition", "restore"])
@pytest.mark.parametrize(
    "start",
    [{"wf": (0,)}, {}, {"other": (0,)}],
    ids=["same-workflow", "empty", "new-workflow"],
)
def test_a_restore_and_a_transition_from_one_snapshot_lose_no_status(
    tmp_path, first, start
):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            policy.install(conn, history.policy())
            doc = Doc()
            doc.workflow_history = history.WorkflowHistory(start)
            conn.root()["doc"] = doc
        names = ("transition", "restore")
        managers = {name: transaction.TransactionManager() for name in names}
        conns = {name: db.open(transaction_manager=managers[name]) for name in names}
        for tm in managers.values():
            tm.begin()
        add_status(conns["transition"].root()["doc"], 1)
        assert history.restore(conns["restore"].root()["doc"])
        second = "restore" if first == "transition" else "transition"
        managers[first].commit()
        # Whichever commits second conflicts: the restore writes the old
        # history with a state that tier 2 does not take.
        with pytest.raises(ConflictError):
            managers[second].commit()
        managers[second].abort()
        managers[second].begin()
        doc = conns[second].root()["doc"]
        if second == "transition":
            add_status(doc, 1)
        else:
            history.restore(doc)
        managers[second].commit()
        for conn in conns.values():
            conn.close()
        with db.transaction() as conn:
            restored = conn.root()["doc"].workflow_history
            assert type(restored) is history._stock()
            assert dict(restored) == {**start, "wf": (*start.get("wf", ()), 1)}
    finally:
        db.close()


def test_a_retired_history_holds_an_empty_workflow():
    doc = Doc()
    old = doc.workflow_history = history.WorkflowHistory({"wf": (0,)})
    assert history.restore(doc)
    assert dict(old) == {history.RETIRED: ()}
    assert all(statuses == () for statuses in old.values())
    assert "WorkflowHistory" in repr(old)


def test_move_leaves_an_object_without_a_history_as_it_is():
    from zope.interface import alsoProvides

    # An object of a type with the behaviour and without a workflow.
    doc = Doc()
    alsoProvides(doc, history.IAppendOnlyHistory)
    assert not history.move(doc)
    assert not hasattr(doc, "workflow_history")
    # An object with the behaviour and a stock history moves; one without
    # the behaviour stays.
    doc.workflow_history = PersistentMapping({"wf": (0,)})
    assert history.move(doc)
    other = Doc()
    other.workflow_history = PersistentMapping({"wf": (0,)})
    assert not history.move(other)


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


@pytest.mark.parametrize(
    "override", [{"tier2": "on"}, {"blocked": False}, {"blocked": False, "tier2": "on"}]
)
def test_an_override_does_not_unblock_a_class(caplog, override):
    result, _ = merges.apply_overrides(
        DERIVED, {"classes": {"m.Odd": override}}, {"m.Odd"}
    )
    assert result["classes"]["m.Odd"] == {"tier2": "off", "blocked": True}
    assert "stays blocked" in caplog.text


def test_a_class_stays_blocked_across_the_sites_of_one_database(caplog):
    # Site A blocks the class; site B removes it with a null override; site
    # C switches it on. The class stays blocked in the database.
    site_a = {"classes": {"m.Odd": {"tier2": "off", "blocked": True}}}
    stored = merges.settle(None, site_a, set())
    site_b, removed = merges.apply_overrides(
        {"classes": {"m.Odd": {"tier2": "shadow"}}}, {"classes": {"m.Odd": None}}
    )
    assert removed == {"m.Odd"}
    stored = merges.settle(stored, site_b, removed)
    assert stored["classes"]["m.Odd"]["blocked"] is True
    site_c = {"classes": {"m.Odd": {"tier2": "on"}}}
    stored = merges.settle(stored, site_c, set())
    assert stored["classes"]["m.Odd"] == {"tier2": "off", "blocked": True}
    assert "cannot unblock it" in caplog.text
    # A class that no site blocks goes when an override removes it.
    stored = merges.settle(
        {"classes": {"m.Page": {"tier2": "on"}}}, {"classes": {}}, {"m.Page"}
    )
    assert "m.Page" not in stored["classes"]


def test_a_class_stays_blocked_through_two_profiles(caplog):
    derived = merges.class_entries([("m.Odd", {"title"}, [(opaque, set())], {})])
    policy_ = {"classes": dict(derived)}
    blocked = {"m.Odd"}
    for overrides in (
        {"classes": {"m.Odd": None}},
        {"classes": {"m.Odd": {"tier2": "on"}}},
    ):
        policy_, removed = merges.apply_overrides(
            policy_, overrides, {"m.Odd"}, blocked
        )
        assert removed == set()
    assert policy_["classes"]["m.Odd"] == {"tier2": "off", "blocked": True}
    assert merges.compose(None, policy_)["classes"]["m.Odd"]["blocked"] is True
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


def test_folder_orders_takes_the_lowest_latest_mode_of_the_sites(caplog):
    modes = {}
    assert merges.lowest_mode(modes, "/a", "off") == "off"
    # The second site imports the profile: the off of the first site lasts,
    # with a warning.
    with caplog.at_level("WARNING", logger="zoe.plone"):
        assert merges.lowest_mode(modes, "/b", "on") == "off"
    assert "/b asks for folder_orders on" in caplog.text
    # The first site raises its own mode: its new mode replaces its earlier
    # one.
    assert merges.lowest_mode(modes, "/a", "on") == "on"
    assert modes == {"/a": "on", "/b": "on"}


def test_one_site_that_raises_folder_orders_raises_it():
    modes = {}
    assert merges.lowest_mode(modes, "/site", "shadow") == "shadow"
    assert merges.lowest_mode(modes, "/site", "on") == "on"
    assert merges.lowest_mode(modes, "/site", None) is None


class Writes(dict):
    """A mapping that counts its writes."""

    writes = 0

    def __setitem__(self, key, value):
        self.writes += 1
        super().__setitem__(key, value)

    def __delitem__(self, key):
        self.writes += 1
        super().__delitem__(key)

    def pop(self, *args):
        self.writes += 1
        return super().pop(*args)


def test_folder_orders_writes_the_modes_only_on_a_change():
    modes = Writes()
    assert merges.lowest_mode(modes, "/site", "on") == "on"
    assert modes.writes == 1
    # The same mode, and no mode for a site that has none, write nothing.
    assert merges.lowest_mode(modes, "/site", "on") == "on"
    assert merges.lowest_mode(modes, "/other", None) == "on"
    assert modes.writes == 1
    assert merges.lowest_mode(modes, "/site", "shadow") == "shadow"
    assert merges.lowest_mode(modes, "/site", None) is None
    assert modes.writes == 3
    assert modes == {}


def test_the_other_switches_take_the_new_value():
    stored = {"version": 2, "posting_dicts": "on", "classes": {}}
    assert (
        merges.compose(stored, {"classes": {}, "posting_dicts": "off"})["posting_dicts"]
        == "off"
    )
    assert merges.compose(stored, {"classes": {}})["posting_dicts"] == "on"


def test_the_sites_compose_folder_orders_and_split_merges():
    assert set(merges.SITE_SWITCHES) == {"folder_orders", "split_merges"}
    assert merges.SITE_MODES == merges.SITE_SWITCHES["folder_orders"]
    assert len(set(merges.SITE_SWITCHES.values())) == 2


def test_lowest_mode_names_its_switch(caplog):
    modes = {}
    assert merges.lowest_mode(modes, "/a", "off", "split_merges") == "off"
    with caplog.at_level("WARNING", logger="zoe.plone"):
        assert merges.lowest_mode(modes, "/b", "on", "split_merges") == "off"
    assert "/b asks for split_merges on" in caplog.text


FOLDER = "plone.app.contenttypes.content.Folder"
SITE_ROOT = "Products.CMFPlone.Portal.PloneSite"
DOCUMENT = "plone.app.contenttypes.content.Document"
ADDON = "m.AddonFolder"
CONTAINERS = (FOLDER, SITE_ROOT)


def profile_overrides():
    """Return the overrides of the profile ``zoe.plone:default``."""
    import importlib.resources
    import json

    path = importlib.resources.files("zoe.plone") / "profiles" / "default"
    return json.loads((path / merges.OVERRIDES).read_text())


def stock_derived(blocked=()):
    """Return a derived policy of a stock site with an add-on container
    class, as `merges.derive` gives it, and the names of its classes."""
    typed = [
        (name, {"title"}, [(opaque, set())] if name in blocked else [], {})
        for name in (FOLDER, SITE_ROOT, DOCUMENT, ADDON)
    ]
    classes = merges.class_entries(typed)
    return {"classes": classes}, set(classes)


def test_the_profile_switches_on_the_stock_containers_and_the_split_merge():
    derived, names = stock_derived()
    result, removed = merges.apply_overrides(derived, profile_overrides(), names)
    assert removed == set()
    assert result["folder_orders"] == "on"
    assert result["split_merges"] == "on"
    for name in CONTAINERS:
        assert result["classes"][name] == {
            **derived["classes"][name],
            "tier2": "on",
        }
        assert result["classes"][name]["fields"] == {"modification_date": "max"}
    # The other classes, an add-on container among them, keep shadow.
    assert result["classes"][DOCUMENT] == derived["classes"][DOCUMENT]
    assert result["classes"][ADDON] == derived["classes"][ADDON]
    assert result["classes"][DOCUMENT]["tier2"] == "shadow"


def test_the_profile_keeps_a_blocked_container_blocked(caplog):
    derived, names = stock_derived(blocked={FOLDER})
    result, _ = merges.apply_overrides(
        derived, profile_overrides(), names, blocked={FOLDER}
    )
    assert result["classes"][FOLDER] == {"tier2": "off", "blocked": True}
    assert result["classes"][SITE_ROOT]["tier2"] == "on"
    assert f"{FOLDER} stays blocked" in caplog.text


@pytest.mark.parametrize(
    "later, expected",
    [
        ({"tier2": "shadow"}, {FOLDER: "shadow", SITE_ROOT: "shadow"}),
        ({"tier2": "on"}, {FOLDER: "on", SITE_ROOT: "on", DOCUMENT: "on"}),
        ({"classes": {FOLDER: {"tier2": "off"}}}, {FOLDER: "off", SITE_ROOT: "on"}),
    ],
)
def test_a_later_profile_sets_the_mode_of_the_containers(later, expected):
    derived, names = stock_derived()
    policy_ = derived
    for overrides in (profile_overrides(), later):
        policy_, _ = merges.apply_overrides(policy_, overrides, names)
    found = {name: policy_["classes"][name]["tier2"] for name in expected}
    assert found == expected


def test_a_later_profile_switches_the_split_merge_off():
    derived, names = stock_derived()
    policy_ = derived
    for overrides in (profile_overrides(), {"split_merges": "off"}):
        policy_, _ = merges.apply_overrides(policy_, overrides, names)
    assert policy_["split_merges"] == "off"
    assert policy_["folder_orders"] == "on"


def test_the_profile_on_a_site_without_a_container_type_adds_a_bare_entry():
    derived, names = stock_derived()
    del derived["classes"][FOLDER]
    names.discard(FOLDER)
    result, _ = merges.apply_overrides(derived, profile_overrides(), names)
    # No fields: each field, `modification_date` too, is strict.
    assert result["classes"][FOLDER] == {"tier2": "on"}
    # The composition with a site that derives the class adds its fields.
    other, _ = merges.apply_overrides(stock_derived()[0], profile_overrides())
    composed = merges.compose(other, result)
    assert composed["classes"][FOLDER]["fields"] == {"modification_date": "max"}
    assert composed["classes"][FOLDER]["tier2"] == "on"


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


def status(action, state):
    """Return a status as DCWorkflow makes it: its action, and the state of
    the object after it."""
    return {"action": action, "review_state": state}


def transitions(tmp_path, statuses):
    """Run one transition for each of `statuses` on a history from one
    snapshot, and commit them in order. Return the outcome of each, and the
    statuses of the workflow after them."""
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        with db.transaction() as conn:
            policy.install(conn, history.policy())
            created = status("create", "private")
            conn.root()["h"] = history.WorkflowHistory({"wf": (created,)})
        managers = [transaction.TransactionManager() for _ in statuses]
        conns = [db.open(transaction_manager=tm) for tm in managers]
        for tm, conn, added in zip(managers, conns, statuses, strict=True):
            tm.begin()
            wfh = conn.root()["h"]
            wfh["wf"] = (*wfh["wf"], added)
        outcomes = []
        for tm in managers:
            try:
                tm.commit()
            except ConflictError:
                tm.abort()
                outcomes.append("conflict")
            else:
                outcomes.append("committed")
        for conn in conns:
            conn.close()
        with db.transaction() as conn:
            return outcomes, [added["action"] for added in conn.root()["h"]["wf"]]
    finally:
        db.close()


def test_concurrent_transitions_of_a_history_merge_in_process(tmp_path):
    comments = [status(f"comment {number}", "private") for number in range(3)]
    outcomes, actions = transitions(tmp_path, comments)
    assert outcomes == ["committed"] * 3
    assert actions == ["create", "comment 0", "comment 1", "comment 2"]


def test_two_changes_of_state_of_a_history_conflict_in_process(tmp_path):
    publish = status("publish", "published")
    submit = status("submit", "pending")
    outcomes, actions = transitions(tmp_path, [publish, submit])
    assert outcomes == ["committed", "conflict"]
    assert actions == ["create", "publish"]
