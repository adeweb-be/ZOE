"""Acceptance test of the workflow history that refuses two changes of
state from one state.

DCWorkflow keeps the history of an object in `workflow_history`, a mapping
of workflow id to a tuple of status dicts. Each status holds the state of
the object after the transition, under the state variable of the workflow
(`review_state` in the workflows of Plone), and the last status gives the
state of the object. Each transition reads the tuple of its workflow, adds
one status, and writes the tuple again (`DefaultWorkflowStatus.set` of
Products.CMFCore 3.7, copied below as `set_status`).

With the behaviour of `zoe.plone`, `workflow_history` is a
`zoe.plone.history.WorkflowHistory`, and the store merges two transitions
of one object from one snapshot at the vote (ADR 0046, tier 2). DCWorkflow
treats two transitions that change the state from one state as exclusive:
the second one starts from a state that is no longer the state of the
object. So the test checks that:

- two transitions from one snapshot that both change the state never both
  commit: the second one fails with `ConflictError`, and the history holds
  the status of the first one only;
- a transition that keeps the state (a comment) and a change of state from
  one snapshot both commit when the comment commits first, with no
  resolution in the client, and the record of the history equals, byte for
  byte, the record that the same transitions give one after the other on
  FileStorage;
- a comment from the old snapshot that commits after a change of state
  loses no state: it fails with `ConflictError`, or the history that it
  leaves still gives the state that the change of state set.

The acceptance tests of the append-only history (`test_append_fields.py`),
whose statuses all keep the state, stay green.

Plone is not installed in the environment of these tests. A persistent
class of this module stands for the Dexterity item, and `Moment` stands
for the `DateTime` of a status. The test runs in-process on `zoe.Storage`,
and through `zoe serve` with `ZEO.ClientStorage` and with
`zoe.zeo.ClientStorage` with the native client inside.
"""

import contextlib
import threading

import pytest
from acceptance_support import current_record as record
from persistent import Persistent

import zoe.testing
from zoe.testing import ZoeServer

# The longest time that a test waits for the server.
TIMEOUT = 60
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The workflow of the transitions, and its state variable.
WORKFLOW = "simple_publication_workflow"
STATE = "review_state"
# The state of the Document after its creation.
INITIAL = "private"


def plone_history():
    """Return the class of `zoe.plone` for `workflow_history`, and the
    policy that the profile of `zoe.plone` writes for it."""
    from zoe.plone import history

    return history.WorkflowHistory, history.policy()


class Document(Persistent):
    """Stands for a Dexterity item with the behaviour of `zoe.plone`."""


class Moment:
    """Stands for `DateTime`: an object of a class in a status."""

    def __init__(self, micros):
        self.micros = micros

    def __eq__(self, other):
        return type(other) is Moment and other.micros == self.micros


def set_status(document, wf_id, status):
    """Add status to the history of wf_id of document.

    The body of `DefaultWorkflowStatus.set` of Products.CMFCore 3.7.
    """
    from persistent.mapping import PersistentMapping

    history = getattr(document, "workflow_history", None)
    if history is None:
        history = document.workflow_history = PersistentMapping()
    wfh = list(history.get(wf_id, ()))
    wfh.append(status)
    history[wf_id] = tuple(wfh)


def status(number, action, state):
    """Return the status of the transition number, as DCWorkflow makes it:
    the action, and the state of the object after it."""
    return {
        "action": action,
        "actor": f"user-{number:03d}",
        "comments": f"transition {number}",
        STATE: state,
        "time": Moment(1_700_000_000_000_000 + number),
    }


#: The first status of the Document.
CREATED = status(0, None, INITIAL)
#: Two transitions that change the state from the initial state.
PUBLISH = status(1, "publish", "published")
SUBMIT = status(2, "submit", "pending")
#: A second publish, from another user.
PUBLISH_AGAIN = status(3, "publish", "published")
#: A transition that keeps the state: a comment.
COMMENT = status(4, "comment", INITIAL)


@contextlib.contextmanager
def opened(mode, path):
    """Open a database of mode on the store in path. Close it at the end."""
    import ZODB

    if mode == "in-process":
        db = ZODB.DB(zoe.Storage(str(path)))
        try:
            yield db
        finally:
            db.close()
        return
    with ZoeServer(str(path)) as server:
        if mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            storage = ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
        else:
            storage = zoe.testing.native_client(
                server.address, storage="1", wait_timeout=TIMEOUT
            )
        db = ZODB.DB(storage)
        try:
            yield db
        finally:
            db.close()


class Calls:
    """Count the calls of `tryToResolveConflict`, in every thread."""

    def __init__(self, monkeypatch):
        from ZODB.ConflictResolution import ConflictResolvingStorage

        self.count = 0
        lock = threading.Lock()
        original = ConflictResolvingStorage.tryToResolveConflict

        def counted(storage, *args, **kwargs):
            with lock:
                self.count += 1
            return original(storage, *args, **kwargs)

        monkeypatch.setattr(ConflictResolvingStorage, "tryToResolveConflict", counted)


def create(db, name, history):
    """Add a Document with history and its first status. Return the oid of
    the history."""
    with db.transaction() as conn:
        document = Document()
        document.workflow_history = history
        set_status(document, WORKFLOW, CREATED)
        conn.root()[name] = document
    with db.transaction() as conn:
        return conn.root()[name].workflow_history._p_oid


def statuses(db, name):
    """Return the statuses of the workflow of the Document name, as a new
    connection loads them."""
    with db.transaction() as conn:
        return list(conn.root()[name].workflow_history[WORKFLOW])


def one_after_the_other(tmp_path, history_class, transitions):
    """Return the record of the history after transitions, one transaction
    each, on FileStorage."""
    import ZODB
    from ZODB.FileStorage import FileStorage

    (tmp_path / "reference").mkdir(exist_ok=True)
    db = ZODB.DB(FileStorage(str(tmp_path / "reference" / "Data.fs")))
    try:
        oid = create(db, "document", history_class())
        for transition in transitions:
            with db.transaction() as conn:
                conn.cacheMinimize()
                set_status(conn.root()["document"], WORKFLOW, transition)
        return record(db, oid)
    finally:
        db.close()


def from_one_snapshot(db, name, transitions):
    """Run transitions on the Document name from one snapshot, and commit
    them in order. Return the transitions that committed and those that
    conflicted."""
    import transaction
    from ZODB.POSException import ConflictError

    managers = [transaction.TransactionManager() for _ in transitions]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        for manager, conn in zip(managers, conns, strict=True):
            manager.begin()
            conn.cacheMinimize()
        for transition, conn in zip(transitions, conns, strict=True):
            set_status(conn.root()[name], WORKFLOW, transition)
        committed, conflicted = [], []
        for transition, manager in zip(transitions, managers, strict=True):
            try:
                manager.commit()
            except ConflictError:
                manager.abort()
                conflicted.append(transition)
            else:
                committed.append(transition)
        return committed, conflicted
    finally:
        for conn in conns:
            conn.close()


@contextlib.contextmanager
def site(mode, tmp_path):
    """Open a database of mode with the policy of `zoe.plone` for the
    history, and a Document. Give the database, the oid of the history of
    the Document, and its class."""
    history_class, plone_policy = plone_history()
    with opened(mode, tmp_path / "store") as db:
        import zoe.policy

        with db.transaction() as conn:
            zoe.policy.install(conn, plone_policy)
        oid = create(db, "document", history_class())
        yield db, oid, history_class


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize(
    "second", [SUBMIT, PUBLISH_AGAIN], ids=["other-state", "same-state"]
)
def test_two_changes_of_state_from_one_state_conflict(mode, second, tmp_path):
    with site(mode, tmp_path) as (db, oid, history_class):
        committed, conflicted = from_one_snapshot(db, "document", [PUBLISH, second])
        stored = record(db, oid)
        after = statuses(db, "document")
    assert committed == [PUBLISH], "both changes of state committed"
    assert conflicted == [second]
    assert after == [CREATED, PUBLISH]
    assert stored == one_after_the_other(tmp_path, history_class, [PUBLISH])


@pytest.mark.parametrize("mode", MODES)
def test_a_comment_then_a_change_of_state_merge(mode, tmp_path, monkeypatch):
    with site(mode, tmp_path) as (db, oid, history_class):
        calls = Calls(monkeypatch)
        committed, conflicted = from_one_snapshot(db, "document", [COMMENT, PUBLISH])
        stored = record(db, oid)
        after = statuses(db, "document")
    assert conflicted == []
    assert committed == [COMMENT, PUBLISH]
    assert calls.count == 0
    assert after[-1][STATE] == "published"
    assert stored == one_after_the_other(tmp_path, history_class, [COMMENT, PUBLISH])


@pytest.mark.parametrize("mode", MODES)
def test_a_comment_after_a_change_of_state_loses_no_state(mode, tmp_path):
    with site(mode, tmp_path) as (db, _, _):
        committed, _ = from_one_snapshot(db, "document", [PUBLISH, COMMENT])
        after = statuses(db, "document")
    assert committed[0] == PUBLISH
    # The comment saw the old state. When it commits, the history still
    # gives the state that the change of state set.
    assert after[-1][STATE] == "published", (committed, after)


@pytest.mark.parametrize("mode", MODES)
def test_transitions_one_after_the_other_still_commit(mode, tmp_path):
    with site(mode, tmp_path) as (db, oid, history_class):
        for transition in (PUBLISH, status(5, "retract", INITIAL), SUBMIT):
            committed, _ = from_one_snapshot(db, "document", [transition])
            assert committed == [transition]
        stored = record(db, oid)
    assert stored == one_after_the_other(
        tmp_path, history_class, [PUBLISH, status(5, "retract", INITIAL), SUBMIT]
    )
