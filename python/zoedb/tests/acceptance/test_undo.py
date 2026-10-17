"""Acceptance test of undo through the merge tiers.

An undo of the transaction T is a transaction of its own. For each oid of
T, it stores the state before T. When a later transaction changed the oid,
the undo is a three-way merge through the tiers: the state of T is the
base, the current state is ours, and the state before T is theirs. It
fails with `UndoError` when no tier resolves it. A blob undo points at the
earlier blob file, and no blob file is written again. Nothing is undoable
past the pack watermark.

The test checks:

- an undo of a transaction whose `Length` and `OOBucket` later merged
  natively commits, and gives the states and the records, byte for byte,
  that FileStorage with ZODB's resolver gives for the same transactions;
  ZOE calls `tryToResolveConflict` in the client for none of them; an undo
  that no tier resolves raises `UndoError` on both, and changes nothing;
- an undo is undoable, and the history of an object keeps every record;
- an undo of a transaction at or before the pack watermark raises
  `UndoError`; an undo of a later transaction still commits;
- a blob undo gives the earlier blob, in a new client too; the blob of the
  undone transaction stays in the history; no blob file of the store
  changes, and the blob files take no more bytes;
- a crash during the commit of an undo leaves the undo whole or absent,
  and an acknowledged undo survives: the kill loop kills the writer of
  undos, a child interpreter in-process or the server through `zoe serve`,
  at random times, and checks each object after the restart.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`ZEO.ClientStorage` and with `zoe.zeo.ClientStorage` with the native
client inside. `just test` runs it.
"""

import contextlib
import hashlib
import itertools
import random
import subprocess
import sys
import textwrap
import threading
import time

import pytest

import zoe
import zoe.testing
from zoe.testing import ZoeServer

# The largest tid. loadBefore(oid, MAX_TID) loads the current record.
MAX_TID = (2**63 - 1).to_bytes(8, "big")
# The longest time that a test waits for the server, a child or a thread.
TIMEOUT = 60
# The modes: in-process, and through zoe serve with each client.
MODES = ("in-process", "zeo-client", "native-client")
# The blobs of the blob undo: larger than many chunks of 64 KiB.
FIRST = b"first blob " * 30_000
SECOND = b"second blob " * 30_000
# The kill loop: its rounds, the objects of each undo, and its seed.
ROUNDS = 5
OBJECTS = 40
SEED = 4206


class Place:
    """The store of a test in one mode, and the server of the store.

    `open()` opens a new database on the store. In-process, only one
    database is open at a time.
    """

    def __init__(self, mode, tmp_path):
        self.mode = mode
        self.path = tmp_path / "store"
        self.caches = (tmp_path / f"cache-{n}" for n in itertools.count())
        self.server = None

    def __enter__(self):
        if self.mode != "in-process":
            self.server = ZoeServer(str(self.path)).start(TIMEOUT)
        return self

    def __exit__(self, *exc_info):
        if self.server is not None:
            self.server.kill()

    @property
    def blob_dir(self):
        """The blob directory of the store."""
        return self.path / "blobs"

    def storage(self, wait_timeout=TIMEOUT):
        if self.mode == "in-process":
            return zoe.Storage(str(self.path))
        kw = dict(
            storage="1",
            wait_timeout=wait_timeout,
            blob_dir=str(next(self.caches)),
            shared_blob_dir=False,
        )
        if self.mode == "zeo-client":
            from ZEO.ClientStorage import ClientStorage

            return ClientStorage(self.server.address, **kw)
        return zoe.testing.native_client(self.server.address, **kw)

    def open(self, **kw):
        import ZODB

        return ZODB.DB(self.storage(**kw))


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


def undo_id(db, description):
    """Return the undo id of the newest transaction with description."""
    for info in db.undoLog(0, 100):
        if info["description"] == description:
            return info["id"]
    raise AssertionError(f"undoLog has no transaction {description!r}")


def undo(db, description, note="undo"):
    """Undo the transaction description in a transaction of its own."""
    undo_by_id(db, undo_id(db, description), note)


def undo_by_id(db, transaction_id, note="undo"):
    """Undo the transaction of the undo id transaction_id in a transaction
    of its own."""
    import transaction

    manager = transaction.TransactionManager()
    manager.begin()
    db.undo(transaction_id, manager.get())
    manager.get().note(note)
    try:
        manager.commit()
    except BaseException:
        manager.abort()
        raise


def commit(db, description, change):
    """Run change(root) in a transaction with description."""
    with db.transaction(description) as conn:
        change(conn.root())


def records(db, names):
    """Return the current record of each object of the root in names."""
    with db.transaction() as conn:
        oids = {name: conn.root()[name]._p_oid for name in names}
    return {name: db.storage.loadBefore(oid, MAX_TID)[0] for name, oid in oids.items()}


def states(db):
    """Return the state of the objects of the merge case as plain data."""
    with db.transaction() as conn:
        root = conn.root()
        return {
            "n": root["n"](),
            "b": dict(root["b"].items()),
            "c": dict(root["c"]),
        }


def merge_case(db):
    """Commit the transactions of the merge case.

    "undone" changes three objects. Two transactions after it change the
    `Length` and the `OOBucket` from one snapshot, so the second of them
    merges with the first.
    """
    import transaction
    from BTrees.Length import Length
    from BTrees.OOBTree import OOBucket
    from persistent.mapping import PersistentMapping

    def create(root):
        root["n"] = Length(10)
        root["b"] = OOBucket({"a0": 1, "a1": 2})
        root["c"] = PersistentMapping(v="before")
        root["d"] = OOBucket({"a0": 1})

    def undone(root):
        root["n"].change(5)
        root["b"]["k1"] = "one"
        root["c"]["v"] = "after"

    commit(db, "create", create)
    commit(db, "undone", undone)
    managers = [transaction.TransactionManager() for _ in range(2)]
    conns = [db.open(transaction_manager=manager) for manager in managers]
    try:
        for manager in managers:
            manager.begin()
        for number, conn in enumerate(conns, start=2):
            conn.root()["n"].change(number)
            conn.root()["b"][f"k{number}"] = f"later {number}"
        for number, manager in enumerate(managers, start=2):
            manager.get().note(f"later {number}")
            manager.commit()
    finally:
        for conn in conns:
            conn.close()
    # The refusal: "set" adds a key that "changed" changes again.
    commit(db, "set", lambda root: root["d"].__setitem__("x", 1))
    commit(db, "changed", lambda root: root["d"].__setitem__("x", 2))


def run_merge_case(db):
    """Run the merge case on db. Return what it gives."""
    from ZODB.POSException import UndoError

    merge_case(db)
    undo(db, "undone")
    merged = (states(db), records(db, ["n", "b", "c"]))
    before = records(db, ["d"])
    try:
        undo(db, "set")
    except UndoError:
        refused = True
    else:
        refused = False
    return merged, refused, before == records(db, ["d"])


@pytest.mark.parametrize("mode", MODES)
def test_an_undo_after_native_merges_merges_through_the_tiers(
    mode, tmp_path, monkeypatch
):
    import ZODB
    from ZODB.FileStorage import FileStorage

    reference_db = ZODB.DB(FileStorage(str(tmp_path / "Data.fs")))
    try:
        reference = run_merge_case(reference_db)
    finally:
        reference_db.close()
    (reference_states, _), refused, unchanged = reference
    # The case is the one that the test means.
    assert reference_states == {
        "n": 15,
        "b": {"a0": 1, "a1": 2, "k2": "later 2", "k3": "later 3"},
        "c": {"v": "before"},
    }
    assert refused and unchanged

    calls = Calls(monkeypatch)
    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            ours = run_merge_case(db)
        finally:
            db.close()
    assert ours == reference
    assert calls.count == 0


@pytest.mark.parametrize("mode", MODES)
def test_an_undo_is_a_transaction_that_undoes(mode, tmp_path):
    from persistent.mapping import PersistentMapping

    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            commit(
                db,
                "create",
                lambda root: root.__setitem__("c", PersistentMapping(v="a")),
            )
            commit(db, "change", lambda root: root["c"].__setitem__("v", "b"))
            undo(db, "change", note="first undo")
            with db.transaction() as conn:
                assert conn.root()["c"]["v"] == "a"
                oid = conn.root()["c"]._p_oid
            undo(db, "first undo", note="second undo")
            with db.transaction() as conn:
                assert conn.root()["c"]["v"] == "b"
            history = db.storage.history(oid, 10)
            descriptions = [entry["description"] for entry in db.undoLog(0, 3)]
        finally:
            db.close()
    assert len(history) == 4
    assert descriptions == ["second undo", "first undo", "change"]


@pytest.mark.parametrize("mode", MODES)
def test_an_undo_past_the_pack_watermark_fails(mode, tmp_path):
    from persistent.mapping import PersistentMapping
    from ZODB.POSException import UndoError

    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            commit(
                db, "create", lambda root: root.__setitem__("c", PersistentMapping(v=1))
            )
            commit(db, "second", lambda root: root["c"].__setitem__("v", 2))
            time.sleep(0.2)
            pack_time = time.time()
            time.sleep(0.2)
            commit(db, "third", lambda root: root["c"].__setitem__("v", 3))
            second = undo_id(db, "second")
            db.pack(pack_time)

            with pytest.raises(UndoError):
                undo_by_id(db, second)
            with db.transaction() as conn:
                assert conn.root()["c"]["v"] == 3
            # A transaction after the watermark stays undoable.
            undo(db, "third")
            with db.transaction() as conn:
                assert conn.root()["c"]["v"] == 2
        finally:
            db.close()


def blob_files(directory):
    """Return the blob files under directory: path to (inode, size,
    mtime, sha256)."""
    files = {}
    for path in sorted(directory.rglob("*.blob")):
        stat = path.stat()
        digest = hashlib.sha256(path.read_bytes()).hexdigest()
        files[path] = (stat.st_ino, stat.st_size, stat.st_mtime_ns, digest)
    return files


def blob_bytes(files):
    """Return the bytes of the distinct files of blob_files."""
    return sum(size for inode, size in {(v[0], v[1]) for v in files.values()})


def read_blob(db, name):
    with db.transaction() as conn, conn.root()[name].open("r") as f:
        return f.read()


@pytest.mark.parametrize("mode", MODES)
def test_a_blob_undo_gives_the_earlier_blob(mode, tmp_path):
    from ZODB.blob import Blob

    def create(root):
        root["blob"] = Blob(FIRST)

    def change(root):
        with root["blob"].open("w") as f:
            f.write(SECOND)

    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            commit(db, "create", create)
            commit(db, "change", change)
            with db.transaction() as conn:
                oid = conn.root()["blob"]._p_oid
            changed = db.storage.loadBefore(oid, MAX_TID)[1]
            files = blob_files(place.blob_dir)
            undo(db, "change")
            assert read_blob(db, "blob") == FIRST
        finally:
            db.close()
        # A new client reads the blob of the undo from the store.
        db = place.open()
        try:
            assert read_blob(db, "blob") == FIRST
            # The undone blob stays in the history.
            with db.storage.openCommittedBlobFile(oid, changed) as f:
                assert f.read() == SECOND
        finally:
            db.close()
        after = blob_files(place.blob_dir)
    assert files, "the store has no blob file"
    for path, stat in files.items():
        assert after.get(path) == stat, f"the undo changed the blob file {path}"
    assert blob_bytes(after) == blob_bytes(files)


# The kill loop of the undos.

CHILD = textwrap.dedent(
    """\
    import sys

    import transaction
    import ZODB

    import zoe

    db = ZODB.DB(zoe.Storage(sys.argv[1]))
    manager = transaction.TransactionManager()
    conn = db.open(transaction_manager=manager)
    print("ready", flush=True)
    while True:
        manager.begin()
        last = db.undoLog(0, 1)[0]
        db.undo(last["id"], manager.get())
        manager.get().note("undo")
        manager.commit()
        tid = db.lastTransaction()
        manager.begin()
        value = conn.root()["item-0"]["v"]
        manager.abort()
        print("undone", tid.hex(), value, flush=True)
    """
)


def items(root):
    from persistent.mapping import PersistentMapping

    for number in range(OBJECTS):
        root[f"item-{number}"] = PersistentMapping(v="a")


def change_items(root):
    for number in range(OBJECTS):
        root[f"item-{number}"]["v"] = "b"


def check_after_a_crash(db, acknowledged):
    """Check the objects after a crash.

    `acknowledged` is the last acknowledged undo: its tid, and the value
    that it gave the objects.
    """
    tid, value = acknowledged
    with db.transaction() as conn:
        root = conn.root()
        values = [root[f"item-{number}"]["v"] for number in range(OBJECTS)]
    # The undo is whole or absent: each object has the same value.
    assert len(set(values)) == 1, f"a partial undo: {values}"
    # The acknowledged undo survives, whole.
    assert db.lastTransaction() >= tid
    conn = db.open(at=tid)
    try:
        root = conn.root()
        at = [root[f"item-{number}"]["v"] for number in range(OBJECTS)]
    finally:
        conn.close()
    assert at == [value] * OBJECTS
    if db.lastTransaction() == tid:
        assert values[0] == value


def read(process, lines):
    """Add each line of the stdout of process to lines."""
    for line in process.stdout:
        lines.append(line)


def prepare(db):
    commit(db, "items", items)
    commit(db, "change", change_items)


def test_a_crash_during_the_commit_of_an_undo_in_process(tmp_path):
    rng = random.Random(SEED)
    path = str(tmp_path / "store")
    db = zoe_db(path)
    try:
        prepare(db)
    finally:
        db.close()
    for round_number in range(ROUNDS):
        process = subprocess.Popen(
            [sys.executable, "-c", CHILD, path],
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            text=True,
        )
        lines = []
        reader = threading.Thread(target=read, args=(process, lines), daemon=True)
        reader.start()
        deadline = time.monotonic() + TIMEOUT
        wanted = 1 + rng.randrange(5)
        while time.monotonic() < deadline and process.poll() is None:
            if sum(line.startswith("undone") for line in lines) >= wanted:
                break
            time.sleep(0.005)
        time.sleep(rng.uniform(0.0, 0.05))
        process.kill()
        process.wait()
        reader.join(TIMEOUT)
        process.stdout.close()
        undone = [line.split() for line in lines if line.startswith("undone")]
        assert undone, f"round {round_number}: no undo committed: {lines}"
        _, tid, value = undone[-1]
        db = zoe_db(path)
        try:
            check_after_a_crash(db, (bytes.fromhex(tid), value))
        finally:
            db.close()


def zoe_db(path):
    import ZODB

    return ZODB.DB(zoe.Storage(path))


@pytest.mark.parametrize("mode", ["zeo-client", "native-client"])
def test_a_crash_during_the_commit_of_an_undo_through_zoe_serve(mode, tmp_path):
    from ZEO.Exceptions import ClientDisconnected

    rng = random.Random(SEED)
    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            prepare(db)
        finally:
            db.close()
        for round_number in range(ROUNDS):
            done = threading.Event()
            acknowledged = []
            errors = []

            def writer(done=done, acknowledged=acknowledged, errors=errors):
                db = place.open(wait_timeout=1)
                try:
                    while not done.is_set():
                        undo_by_id(db, db.undoLog(0, 1)[0]["id"])
                        tid = db.lastTransaction()
                        with db.transaction() as conn:
                            value = conn.root()["item-0"]["v"]
                        acknowledged.append((tid, value))
                except (ClientDisconnected, OSError):
                    pass
                except Exception as error:
                    if not done.is_set():
                        errors.append(error)
                finally:
                    with contextlib.suppress(Exception):
                        db.close()

            thread = threading.Thread(target=writer, daemon=True)
            thread.start()
            deadline = time.monotonic() + TIMEOUT
            wanted = 1 + rng.randrange(5)
            while len(acknowledged) < wanted and time.monotonic() < deadline:
                if not thread.is_alive():
                    break
                time.sleep(0.005)
            time.sleep(rng.uniform(0.0, 0.05))
            place.server.kill()
            done.set()
            thread.join(TIMEOUT)
            assert not thread.is_alive(), "the writer did not stop"
            assert errors == [], errors
            assert acknowledged, f"round {round_number}: no undo committed"
            place.server.start(TIMEOUT)
            db = place.open()
            try:
                check_after_a_crash(db, acknowledged[-1])
            finally:
                db.close()
