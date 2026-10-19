"""Unit tests of the properties of the members and groups of `zoe.plone`
(ADR 0046, section 9), without Plone.

The tree of each test stands for `_storage` of a mutable property sheet of
PAS: an `OOBTree` of principal id to the properties of the principal. A
login, as `setPropertiesForUser` does it, changes the properties and sets
the key of the principal again."""

import sys

import pytest
import transaction
import ZODB
from BTrees.OOBTree import OOBTree
from ZODB.POSException import ConflictError

import zoe
from zoe import policy
from zoe.plone import properties
from zoe.plone.properties import PrincipalProperties

PRINCIPALS = ("one", "two")


def test_the_properties_are_a_mapping_whose_state_is_the_mapping():
    props = PrincipalProperties({"fullname": "One"})
    props["email"] = "one@example.invalid"
    assert dict(props.items()) == {"fullname": "One", "email": "one@example.invalid"}
    assert props.get("missing") is None
    assert "fullname" in props
    assert len(props) == 2
    props.update({"isGroup": False})
    assert props.setdefault("login_time", 1) == 1
    del props["email"]
    assert props.__getstate__() == {
        "fullname": "One",
        "isGroup": False,
        "login_time": 1,
    }
    # A copy is a plain dict: it is no new record.
    assert type(props.copy()) is dict
    assert props.copy() == props.__getstate__()
    assert "PrincipalProperties" in repr(props)


def test_the_keys_of_loaded_properties_are_interned():
    props = PrincipalProperties()
    key = "".join(["zoe-", "property-", "x"])
    props.__setstate__({key: 1})
    assert next(iter(props)) is sys.intern(key)


def test_the_policy_merges_the_times_of_a_login():
    assert properties.policy() == {
        "classes": {
            "zoe.plone.properties.PrincipalProperties": {
                "tier2": "on",
                "default": "strict",
                "fields": {"login_time": "max", "last_login_time": "max"},
            }
        }
    }
    entry = properties.policy()["classes"]["zoe.plone.properties.PrincipalProperties"]
    entry["fields"]["login_time"] = "lww"
    assert properties.ENTRY["fields"]["login_time"] == "max"


class Sheet:
    """Stands for a mutable property sheet: its `_storage`, and its mark
    when the import step moved it."""

    def __init__(self, storage):
        self._storage = storage


def log_in(storage, principal, now):
    """Write the times of a login of `principal` at `now`, as
    `setPropertiesForUser` does."""
    props = storage[principal]
    props.update({"login_time": now, "last_login_time": props["login_time"]})
    storage[principal] = storage[principal]


def store(tmp_path, values):
    """Return a database on a `zoe.Storage` with the policy of the
    properties and a tree of `values` at the root key `storage`."""
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    with db.transaction() as conn:
        policy.install(conn, properties.policy())
        conn.root()["storage"] = OOBTree(values)
    return db


def concurrent(db, *changes):
    """Run each of `changes` on the tree from one snapshot, each in its own
    transaction, and commit them in order. Return the outcome of each."""
    managers = [transaction.TransactionManager() for _ in changes]
    conns = [db.open(transaction_manager=tm) for tm in managers]
    try:
        for tm, conn, change in zip(managers, conns, changes, strict=True):
            tm.begin()
            change(conn.root()["storage"])
        outcomes = []
        for tm in managers:
            try:
                tm.commit()
            except ConflictError:
                tm.abort()
                outcomes.append("conflict")
            else:
                outcomes.append("committed")
        return outcomes
    finally:
        for conn in conns:
            conn.close()


def stored(db):
    with db.transaction() as conn:
        return {key: dict(value) for key, value in conn.root()["storage"].items()}


def principals(make):
    return {
        name: make({"fullname": name, "login_time": 0, "last_login_time": 0})
        for name in PRINCIPALS
    }


def test_two_logins_of_two_principals_merge(tmp_path):
    db = store(tmp_path, principals(PrincipalProperties))
    try:
        outcomes = concurrent(
            db,
            lambda tree: log_in(tree, "one", 10),
            lambda tree: log_in(tree, "two", 20),
        )
        assert outcomes == ["committed", "committed"]
        found = stored(db)
        assert found["one"]["login_time"] == 10
        assert found["two"]["login_time"] == 20
    finally:
        db.close()


def test_two_logins_of_one_principal_keep_the_later_time(tmp_path):
    db = store(tmp_path, principals(PrincipalProperties))
    try:
        outcomes = concurrent(
            db,
            lambda tree: log_in(tree, "one", 20),
            lambda tree: log_in(tree, "one", 10),
        )
        assert outcomes == ["committed", "committed"]
        assert stored(db)["one"] == {
            "fullname": "one",
            "login_time": 20,
            "last_login_time": 0,
        }
    finally:
        db.close()


def test_two_changes_of_another_property_of_one_principal_conflict(tmp_path):
    db = store(tmp_path, principals(PrincipalProperties))

    def rename(name):
        def change(tree):
            tree["one"]["fullname"] = name
            tree["one"] = tree["one"]

        return change

    try:
        assert concurrent(db, rename("A"), rename("B")) == ["committed", "conflict"]
        assert stored(db)["one"]["fullname"] == "A"
    finally:
        db.close()


def test_two_logins_of_two_principals_with_dicts_conflict(tmp_path):
    # The stock values: the resolver of BTrees does not order two dicts, so
    # it refuses the bucket. So the import step moves every value.
    db = store(tmp_path, principals(dict))
    try:
        outcomes = concurrent(
            db,
            lambda tree: log_in(tree, "one", 10),
            lambda tree: log_in(tree, "two", 20),
        )
        assert outcomes == ["committed", "conflict"]
    finally:
        db.close()


def move_all(tree):
    sheet = Sheet(tree)
    assert properties.move_properties(sheet) == len(PRINCIPALS)
    assert properties.marked(sheet)


def restore_all(tree):
    sheet = Sheet(tree)
    sheet._zoe_principal_properties = True
    assert properties.restore_properties(sheet) == len(PRINCIPALS)
    assert not properties.marked(sheet)


def stock_log_in(tree):
    # The stock write of a dict value: `update` of the dict, then the key
    # again.
    tree["one"].update({"login_time": 10, "last_login_time": 0})
    tree["one"] = tree["one"]


@pytest.mark.parametrize("first", ["login", "move"])
def test_a_move_and_a_login_from_one_snapshot_lose_no_write(tmp_path, first):
    db = store(tmp_path, principals(dict))
    try:
        changes = {"login": stock_log_in, "move": move_all}
        second = "move" if first == "login" else "login"
        assert concurrent(db, changes[first], changes[second]) == [
            "committed",
            "conflict",
        ]
        # The retry sees the commit of the other one.
        with db.transaction() as conn:
            tree = conn.root()["storage"]
            if second == "login":
                log_in(tree, "one", 10)
            else:
                move_all(tree)
        with db.transaction() as conn:
            tree = conn.root()["storage"]
            assert all(type(value) is PrincipalProperties for value in tree.values())
            assert tree["one"]["login_time"] == 10
    finally:
        db.close()


@pytest.mark.parametrize("first", ["login", "restore"])
def test_a_restore_and_a_login_from_one_snapshot_lose_no_write(tmp_path, first):
    db = store(tmp_path, principals(PrincipalProperties))
    try:
        changes = {
            "login": lambda tree: log_in(tree, "one", 10),
            "restore": restore_all,
        }
        second = "restore" if first == "login" else "login"
        # Whichever commits second conflicts: the restore writes the old
        # record with a state that tier 2 does not take.
        assert concurrent(db, changes[first], changes[second]) == [
            "committed",
            "conflict",
        ]
        with db.transaction() as conn:
            tree = conn.root()["storage"]
            if second == "login":
                stock_log_in(tree)
            else:
                restore_all(tree)
        with db.transaction() as conn:
            tree = conn.root()["storage"]
            assert all(type(value) is dict for value in tree.values())
            assert tree["one"]["login_time"] == 10
    finally:
        db.close()


def test_a_second_move_changes_nothing():
    tree = OOBTree(principals(dict))
    sheet = Sheet(tree)
    assert properties.move_properties(sheet) == 2
    assert properties.move_properties(sheet) == 0
    assert properties.restore_properties(sheet) == 2
    assert properties.restore_properties(sheet) == 0
    assert tree["one"] == {"fullname": "one", "login_time": 0, "last_login_time": 0}


def test_a_retired_value_holds_no_property():
    tree = OOBTree(principals(PrincipalProperties))
    old = tree["one"]
    assert properties.restore(tree, "one")
    assert dict(old) == {properties.RETIRED: None}
    assert type(tree["one"]) is dict


def test_the_moves_commit_in_batches(monkeypatch, tmp_path):
    monkeypatch.setattr(properties, "BATCH", 2)
    values = {f"p{number}": {"fullname": str(number)} for number in range(5)}
    db = store(tmp_path, values)
    try:
        conn = db.open()
        commits = []
        monkeypatch.setattr(transaction, "commit", lambda: commits.append(1))
        assert properties.move_properties(Sheet(conn.root()["storage"])) == 5
        # Two full batches commit. The last value stays in the transaction
        # of the caller.
        assert len(commits) == 2
        transaction.abort()
        conn.close()
    finally:
        db.close()
