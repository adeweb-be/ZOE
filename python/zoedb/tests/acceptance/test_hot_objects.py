"""Acceptance test of the writers of a hot object.

Each save of a Plone site writes the same `BTrees.Length` counters. A
commit holds the lock of each oid that it writes from its vote until it is
durable, so the writers of a hot object go one by one through the sync.
Two mechanisms change that:

- The lock hand-off (ADR 0044). A vote that merges natively onto a commit
  whose finish has started takes the lock of the oid at once. So the
  commits of a hot `Length` can share one sync.
- Early write-intent locks, with `early-locks on`. The client asks for the
  lock of an object when its transaction first changes the object. So the
  writers of an object that cannot merge wait instead of failing. Early
  locks stay off by default.

The test checks:

- 64 writers on one `Length` through `zoe serve`, with early locks on,
  raise no `ConflictError`, the counter holds each increment, and the mean
  of `zoe_fsync_batch_size` of `GET /metrics` over the run is above 1;
- 64 writers on one object that cannot merge, a `PersistentMapping`, raise
  `ConflictError` with early locks off, the default, as today. With early
  locks on, they raise no `ConflictError`. In both cases the counter holds
  exactly the increments that committed: no update is lost.

A writer does not retry, so the test counts each `ConflictError` that a
transaction raises. The writers of a round begin their transactions, wait
for each other, change the object, work a short time and commit.

`early-locks` is a key of `<zoe>` and of `<zoeclient>`. The test runs
in-process on `<zoe>`, and through `zoe serve` on `<zoeclient>`, the native
client. `just test` runs it.
"""

import contextlib
import os
import subprocess
import sys
import threading
import time
import urllib.request

import pytest

from zoe.testing import ZoeServer

# The longest time that a test waits for the server or a thread.
TIMEOUT = 120
# The writers of a hot object.
WRITERS = 64
# The transactions of each writer on the hot `Length`.
LENGTH_ROUNDS = 4
# The transactions of each writer on the object that cannot merge.
OBJECT_ROUNDS = 2
# The work of a transaction after its change, in seconds.
WORK = 0.005
# The lock timeout, in seconds. A run never waits as long, so no
# `ConflictError` comes from a timeout.
LOCK_TIMEOUT = 120
# The modes of the object that cannot merge.
MODES = ("in-process", "zoe-serve")


def database(section, early_locks):
    """Open a database on the storage section `section`.

    `section` is the text of a `<zoe>` or `<zoeclient>` section without
    its key `early-locks`. `early_locks` is "on", "off", or None for the
    default.

    The specification names the key `early-locks` of `<zoe>` and of
    `<zoeclient>`. The ADR of `lock_intent` names how the key selects the
    connection class of the database. The human approves a change of this
    function with the ADR.
    """
    import ZODB.config

    lines = section.strip().splitlines()
    if early_locks is not None:
        lines.insert(-1, f"  early-locks {early_locks}")
    if early_locks == "on":
        lines.insert(-1, "  early-lock-classes persistent.mapping.PersistentMapping")
    body = "\n".join(f"  {line}" for line in lines)
    text = f"%import zoe\n<zodb>\n  pool-size {WRITERS}\n{body}\n</zodb>\n"
    return ZODB.config.databaseFromString(text)


def in_process(path):
    """Return the `<zoe>` section of the store in path."""
    return f"<zoe>\n  path {path}\n  lock-timeout {LOCK_TIMEOUT}s\n</zoe>"


def through(server):
    """Return the `<zoeclient>` section of the storage 1 of server."""
    host, port = server.address
    return (
        f"<zoeclient>\n  server {host}:{port}\n  storage 1\n"
        f"  wait-timeout {TIMEOUT}\n</zoeclient>"
    )


class Server:
    """A `zoe serve` process with an admin port, to read `/metrics`.

    `zoe.testing.ZoeServer` keeps only the line `listening ADDRESS` of the
    server. This class also keeps the line `admin ADDRESS` before it.
    """

    def __init__(self, path):
        self.path = path
        self.address = None
        self.admin = None
        self._process = None

    def __enter__(self):
        command = [
            ZoeServer.binary(), "serve", "--path", str(self.path),
            "--listen", "127.0.0.1:0", "--admin-listen", "127.0.0.1:0",
            "--lock-timeout", str(LOCK_TIMEOUT),
        ]  # fmt: skip
        environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        process = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            env=environ,
            text=True,
        )
        # The kill closes stdout, so the read below ends.
        timer = threading.Timer(TIMEOUT, process.kill)
        timer.start()
        try:
            for line in process.stdout:
                word, _, value = line.strip().partition(" ")
                if word == "admin":
                    self.admin = value
                elif word == "listening":
                    host, port = value.rsplit(":", 1)
                    self.address = (host.strip("[]"), int(port))
                    self._process = process
                    return self
        finally:
            timer.cancel()
        code = process.wait()
        process.stdout.close()
        raise RuntimeError(f"zoe serve stopped with {code} before it listened")

    def __exit__(self, *exc_info):
        process, self._process = self._process, None
        if process is not None:
            process.kill()
            process.wait()
            process.stdout.close()

    def batches(self):
        """Return the sum and the count of `zoe_fsync_batch_size`."""
        url = f"http://{self.admin}/metrics"
        with urllib.request.urlopen(url, timeout=TIMEOUT) as reply:
            text = reply.read().decode()
        found = {}
        for part in ("sum", "count"):
            prefix = f'zoe_fsync_batch_size_{part}{{storage="1"}} '
            for line in text.splitlines():
                if line.startswith(prefix):
                    found[part] = float(line.split()[-1])
            if part not in found:
                raise AssertionError(f"/metrics has no {prefix.strip()}:\n{text}")
        return found["sum"], found["count"]


@contextlib.contextmanager
def opened(mode, tmp_path, early_locks):
    """Open a database of mode with early_locks. Close it at the end."""
    path = tmp_path / "store"
    if mode == "in-process":
        db = database(in_process(path), early_locks)
        try:
            yield db
        finally:
            db.close()
        return
    with Server(path) as server:
        db = database(through(server), early_locks)
        try:
            yield db
        finally:
            db.close()


class Outcome:
    """What the writers of a run did."""

    def __init__(self):
        self.commits = 0
        self.conflicts = 0
        self.errors = []
        self._lock = threading.Lock()

    def add(self, name):
        with self._lock:
            setattr(self, name, getattr(self, name) + 1)

    def fail(self, error):
        with self._lock:
            self.errors.append(error)


def race(db, change, rounds):
    """Run WRITERS writers of rounds transactions each, with change.

    Each transaction calls change(root) on its own connection. Return the
    Outcome.
    """
    import transaction
    from ZODB.POSException import ConflictError

    outcome = Outcome()
    barrier = threading.Barrier(WRITERS)

    def writer():
        manager = transaction.TransactionManager()
        conn = db.open(transaction_manager=manager)
        try:
            for _ in range(rounds):
                manager.begin()
                barrier.wait(TIMEOUT)
                try:
                    change(conn.root())
                    time.sleep(WORK)
                    manager.commit()
                except ConflictError:
                    manager.abort()
                    outcome.add("conflicts")
                else:
                    outcome.add("commits")
        except Exception as error:
            barrier.abort()
            outcome.fail(error)
        finally:
            with contextlib.suppress(Exception):
                manager.abort()
            conn.close()

    threads = [threading.Thread(target=writer, daemon=True) for _ in range(WRITERS)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(TIMEOUT)
    assert not any(thread.is_alive() for thread in threads), "a writer hangs"
    assert outcome.errors == [], outcome.errors[:3]
    assert outcome.commits + outcome.conflicts == WRITERS * rounds
    return outcome


def increment_length(root):
    root["length"].change(1)


def increment_mapping(root):
    hot = root["hot"]
    hot["n"] = hot["n"] + 1


@pytest.mark.skipif(
    sys.platform != "linux",
    reason="fsync on macOS is faster than a client round trip, "
    "so commits do not wait in batches",
)
def test_writers_on_a_hot_length_share_the_syncs(tmp_path):
    from BTrees.Length import Length

    with Server(tmp_path / "store") as server:
        db = database(through(server), "on")
        try:
            with db.transaction() as conn:
                conn.root()["length"] = Length(0)
            before = server.batches()
            outcome = race(db, increment_length, LENGTH_ROUNDS)
            after = server.batches()
            with db.transaction() as conn:
                value = conn.root()["length"]()
        finally:
            db.close()
    assert outcome.conflicts == 0
    assert value == WRITERS * LENGTH_ROUNDS
    commits = after[0] - before[0]
    syncs = after[1] - before[1]
    assert commits >= WRITERS * LENGTH_ROUNDS
    assert syncs > 0
    assert commits / syncs > 1, f"{commits} commits in {syncs} syncs"


@pytest.mark.parametrize("mode", MODES)
def test_writers_on_an_object_that_cannot_merge_wait_with_early_locks(mode, tmp_path):
    from persistent.mapping import PersistentMapping

    results = {}
    for early_locks in (None, "on"):
        store = tmp_path / ("default" if early_locks is None else early_locks)
        store.mkdir()
        with opened(mode, store, early_locks) as db:
            with db.transaction() as conn:
                conn.root()["hot"] = PersistentMapping(n=0)
            outcome = race(db, increment_mapping, OBJECT_ROUNDS)
            with db.transaction() as conn:
                value = conn.root()["hot"]["n"]
        # No update is lost: the counter holds each commit.
        assert value == outcome.commits
        results[early_locks] = outcome

    # Off by default, the writers conflict as today.
    assert results[None].conflicts > 0
    # With early locks on, they wait instead.
    assert results["on"].conflicts == 0
    assert results["on"].commits == WRITERS * OBJECT_ROUNDS
