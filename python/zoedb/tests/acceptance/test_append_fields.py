"""Acceptance test of the append-only workflow history.

DCWorkflow keeps the history of an object in `workflow_history`, a mapping
of workflow id to a tuple of status dicts. Each transition reads the tuple
of its workflow, adds one status, and writes the tuple again
(`DefaultWorkflowStatus.set` of Products.CMFCore 3.7, copied below as
`set_status`). In stock Plone the mapping is a `PersistentMapping`, so two
transitions of one Document from one snapshot conflict on it. With the
behaviour of `zoe.plone`, `workflow_history` is an object of a class of
`zoe.plone` whose merge policy is `append`, and the profile of `zoe.plone`
writes that policy into the store with `zoe.policy.install` (ADR 0046).

The test checks that concurrent transitions on one Document all commit,
with no resolution in the client, and that the record of the history then
equals, byte for byte, the record that the same transitions give one after
the other, in the order of their commits, on FileStorage. A stock
`PersistentMapping` history in the same run conflicts, so the transitions
are concurrent.

The bytes of a record depend on the objects that the client shares in
memory: an object that a connection keeps in its cache since an earlier
transaction pickles with other memo entries than an object that it loads
again. So each transition, in both runs, loads the history from its
record, as a request does whose cache does not hold it.

Plone is not installed in the environment of these tests. A persistent
class of this module stands for the Dexterity item, and `Moment` stands
for the `DateTime` of a status. The test runs in-process on `zoe.Storage`,
and through `zoe serve` with `ZEO.ClientStorage` and with
`zoe.zeo.ClientStorage` with the native client inside. `just test` runs it.
"""

import contextlib
import threading

import pytest
from persistent import Persistent

import zoe.testing
from zoe.testing import ZoeServer

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for the server or a thread.
TIMEOUT = 60
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The workflow of the transitions.
WORKFLOW = "simple_publication_workflow"
# The transitions from one snapshot, and the threads of the threaded run.
TRANSITIONS = 8
THREADS = 16


def plone_history():
    """Return the class of `zoe.plone` for `workflow_history`, and the
    policy that the profile of `zoe.plone` writes for it.

    The card names these classes at a checkpoint: they change how a site
    stores its history. The human approves a change of this function with
    that checkpoint.
    """
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


def status(number):
    """Return the status of the transition number, as DCWorkflow makes it."""
    return {
        "action": None if number == 0 else "comment",
        "actor": f"user-{number:03d}",
        "comments": f"transition {number}",
        "review_state": "private",
        "time": Moment(1_700_000_000_000_000 + number),
    }


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
        set_status(document, WORKFLOW, status(0))
        conn.root()[name] = document
    with db.transaction() as conn:
        return conn.root()[name].workflow_history._p_oid


def record(db, oid):
    return db.storage.loadBefore(oid, MAX_TID)[0]


def one_after_the_other(tmp_path, history_class, numbers):
    """Return the record of the history after the transitions numbers, one
    transaction each, on FileStorage."""
    import ZODB
    from ZODB.FileStorage import FileStorage

    db = ZODB.DB(FileStorage(str(tmp_path / "reference" / "Data.fs")))
    try:
        oid = create(db, "document", history_class())
        for number in numbers:
            with db.transaction() as conn:
                conn.cacheMinimize()
                set_status(conn.root()["document"], WORKFLOW, status(number))
        return record(db, oid)
    finally:
        db.close()


def from_one_snapshot(db, name, count):
    """Run count transitions of the Document name from one snapshot, and
    commit them in order. Return the numbers of the transitions that
    committed and those that conflicted."""
    import transaction
    from ZODB.POSException import ConflictError

    managers = [transaction.TransactionManager() for _ in range(count)]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        for manager, conn in zip(managers, conns, strict=True):
            manager.begin()
            conn.cacheMinimize()
        for number, conn in enumerate(conns, start=1):
            set_status(conn.root()[name], WORKFLOW, status(number))
        committed, conflicted = [], []
        for number, manager in enumerate(managers, start=1):
            try:
                manager.commit()
            except ConflictError:
                manager.abort()
                conflicted.append(number)
            else:
                committed.append(number)
        return committed, conflicted
    finally:
        for conn in conns:
            conn.close()


@pytest.mark.parametrize("mode", MODES)
def test_transitions_from_one_snapshot_all_commit(mode, tmp_path, monkeypatch):
    from persistent.mapping import PersistentMapping

    (tmp_path / "reference").mkdir()
    history_class, plone_policy = plone_history()
    with opened(mode, tmp_path / "store") as db:
        import zoe.policy

        with db.transaction() as conn:
            zoe.policy.install(conn, plone_policy)
        # A stock history conflicts: the transitions are concurrent.
        create(db, "stock", PersistentMapping())
        _, stock_conflicts = from_one_snapshot(db, "stock", 2)

        oid = create(db, "document", history_class())
        calls = Calls(monkeypatch)
        committed, conflicted = from_one_snapshot(db, "document", TRANSITIONS)
        stored = record(db, oid)
    assert stock_conflicts == [2]
    assert conflicted == []
    assert committed == list(range(1, TRANSITIONS + 1))
    assert calls.count == 0
    assert stored == one_after_the_other(tmp_path, history_class, committed)


@pytest.mark.parametrize("mode", MODES)
def test_concurrent_transitions_all_commit(mode, tmp_path, monkeypatch):
    import transaction
    from ZODB.POSException import ConflictError

    (tmp_path / "reference").mkdir()
    history_class, plone_policy = plone_history()
    with opened(mode, tmp_path / "store") as db:
        import zoe.policy

        with db.transaction() as conn:
            zoe.policy.install(conn, plone_policy)
        oid = create(db, "document", history_class())
        start = db.lastTransaction()
        calls = Calls(monkeypatch)
        barrier = threading.Barrier(THREADS)
        conflicts = []
        errors = []

        def transition(number):
            manager = transaction.TransactionManager()
            conn = db.open(transaction_manager=manager)
            try:
                manager.begin()
                conn.cacheMinimize()
                barrier.wait(TIMEOUT)
                set_status(conn.root()["document"], WORKFLOW, status(number))
                manager.get().note(f"transition {number}")
                manager.commit()
            except ConflictError as error:
                conflicts.append((number, error))
            except Exception as error:
                barrier.abort()
                errors.append(error)
            finally:
                with contextlib.suppress(Exception):
                    manager.abort()
                conn.close()

        threads = [
            threading.Thread(target=transition, args=(number,), daemon=True)
            for number in range(1, THREADS + 1)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
        assert not any(thread.is_alive() for thread in threads), "a transition hangs"
        # The commit order: the notes of the transactions after the start,
        # in tid order.
        order = [
            int(txn.description.decode().removeprefix("transition "))
            for txn in db.storage.iterator(start)
            if txn.tid != start
        ]
        stored = record(db, oid)
    assert errors == [], errors
    assert conflicts == [], conflicts
    assert calls.count == 0
    assert sorted(order) == list(range(1, THREADS + 1))
    assert stored == one_after_the_other(tmp_path, history_class, order)
