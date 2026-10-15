"""Acceptance test of the close of the stores on SIGTERM and SIGHUP.

When `zoe.Storage` opens a store, the extension installs a handler of
SIGTERM and of SIGHUP, each only when the action of that signal is still the
default. On the signal, a thread of the extension stops new commits, waits a
bounded time for a commit in progress, closes each open store cleanly, then
raises the signal again with its default action. So the process ends with
the status of the signal, as without the handler, and a tool that reads the
store needs no repair.

Each case runs a child interpreter that opens stores, commits and waits on
its stdin. The test sends the signal and checks the exit status. Then it
checks that each store was closed cleanly: `python -m zoe verify` accepts
it and changes no byte of its database file. A store that a writer left
open fails this check, whether `zoe verify` refuses it or repairs it.

A handler that the application sets, before or after the open, wins. An
action that is not the default, such as `SIG_IGN`, stays. A child that
`os.fork()` makes after the open ends on SIGTERM, as without the extension.

Each test imports ZODB itself, so that the module collects before ZODB is
installed.
"""

import os
import queue
import signal
import subprocess
import sys
import tempfile
import textwrap
import threading
import time

import pytest

# The longest time that a child takes to start, to write a line or to end.
TIMEOUT = 60
# The longest time from the signal to the end of a child whose store a call
# holds: the bound of the close, 5 seconds, and a margin for a slow machine.
HELD_STOP = 30

# The start of each child: the imports, and the functions that talk to the
# test.
PRELUDE = """\
import os
import signal
import sys
import threading
import time

import ZODB
import zoe


def ready():
    print("ready", flush=True)


def wait_for_the_test():
    sys.stdin.read()
"""


def environment():
    """Return the environment of a child, without the ZOE_* variables."""
    return {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}


class Child:
    """A child interpreter that runs code, with its arguments.

    The child reads its stdin until the test closes it, so it waits for the
    signal there. A thread of the test reads the lines of its stdout, so
    the pipe never fills.
    """

    def __init__(self, code, *args):
        self.stderr_file = tempfile.TemporaryFile(mode="w+")
        self.process = subprocess.Popen(
            [
                sys.executable,
                # A fork in a process with threads warns from CPython 3.12.
                "-W",
                "ignore::DeprecationWarning",
                "-c",
                PRELUDE + textwrap.dedent(code),
                *map(str, args),
            ],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            text=True,
            env=environment(),
        )
        self.lines = queue.Queue()
        self.reader = threading.Thread(target=self._read, daemon=True)
        self.reader.start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.put(line.rstrip("\n"))
        self.lines.put(None)

    def stderr(self):
        """Return what the child wrote to stderr so far."""
        self.stderr_file.seek(0)
        return self.stderr_file.read()

    def next_line(self):
        """Return the next line of the stdout of the child."""
        try:
            line = self.lines.get(timeout=TIMEOUT)
        except queue.Empty:
            self.process.kill()
            pytest.fail(f"the child wrote no line in {TIMEOUT} s:\n{self.stderr()}")
        if line is None:
            self.process.wait()
            pytest.fail(f"the child ended early:\n{self.stderr()}")
        return line

    def expect_ready(self):
        assert self.next_line() == "ready"

    def rest(self):
        """Return the lines that the child wrote after the last line read.

        Call it after the end of the child.
        """
        self.reader.join(TIMEOUT)
        lines = []
        while True:
            line = self.lines.get_nowait()
            if line is None:
                return lines
            lines.append(line)

    def send(self, signum):
        self.process.send_signal(signum)

    def running(self):
        return self.process.poll() is None

    def close_stdin(self):
        self.process.stdin.close()

    def wait(self, timeout=TIMEOUT):
        """Wait for the end of the child, and return its exit status.

        Its stdin stays open, so the end comes from the signal, not from
        the end of the input.
        """
        try:
            return self.process.wait(timeout)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            pytest.fail(f"the child did not end in {timeout} s:\n{self.stderr()}")


def assert_closed_cleanly(path):
    """Check that the store in path was closed cleanly.

    `zoe verify` opens the store read-only. A store that a writer left open
    fails the check: `zoe verify` refuses it, or repairs it and so changes
    its database file.
    """
    database = path / "zoe.redb"
    before = database.read_bytes()
    result = subprocess.run(
        [sys.executable, "-m", "zoe", "verify", "--path", str(path)],
        capture_output=True,
        text=True,
        env=environment(),
        check=False,
        timeout=TIMEOUT,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    assert database.read_bytes() == before, (
        "zoe verify changed the database file: the store was not closed cleanly"
    )


def stored_value(path, key):
    """Return the value of key in the root object of the store in path."""
    import ZODB

    import zoe

    db = ZODB.DB(zoe.Storage(str(path)))
    try:
        with db.transaction() as connection:
            return connection.root()[key]
    finally:
        db.close()


@pytest.mark.parametrize(
    "signum", [signal.SIGTERM, signal.SIGHUP], ids=["SIGTERM", "SIGHUP"]
)
def test_the_signal_closes_each_store_and_ends_the_process_with_its_status(
    tmp_path, signum
):
    # Two stores, as an instance with a mount point has.
    paths = [tmp_path / "main", tmp_path / "mounted"]
    child = Child(
        """
        dbs = [ZODB.DB(zoe.Storage(path)) for path in sys.argv[1:]]
        for db, path in zip(dbs, sys.argv[1:]):
            with db.transaction() as connection:
                connection.root()["value"] = path
        ready()
        wait_for_the_test()
        """,
        *paths,
    )
    child.expect_ready()

    child.send(signum)

    assert child.wait() == -signum, child.stderr()
    for path in paths:
        assert_closed_cleanly(path)
        assert stored_value(path, "value") == str(path)


def test_each_commit_that_returned_before_the_signal_stays(tmp_path):
    path = tmp_path / "store"
    child = Child(
        """
        db = ZODB.DB(zoe.Storage(sys.argv[1]))


        def write():
            n = 0
            while True:
                n += 1
                try:
                    with db.transaction() as connection:
                        connection.root()["counter"] = n
                except Exception:
                    return
                # The commit returned: the test counts it as acknowledged.
                print(n, flush=True)


        ready()
        threading.Thread(target=write, daemon=True).start()
        wait_for_the_test()
        """,
        path,
    )
    child.expect_ready()
    acknowledged = 0
    while acknowledged < 20:
        acknowledged = int(child.next_line())

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    for line in child.rest():
        acknowledged = int(line)
    assert_closed_cleanly(path)
    assert stored_value(path, "counter") >= acknowledged


def test_a_call_that_holds_a_store_does_not_keep_the_process_alive(tmp_path):
    held, idle = tmp_path / "held", tmp_path / "idle"
    child = Child(
        """
        import pickle

        from persistent.mapping import PersistentMapping
        from ZODB.Connection import TransactionMetaData
        from ZODB.utils import z64

        held = zoe.Storage(sys.argv[1])
        idle = ZODB.DB(zoe.Storage(sys.argv[2]))
        with idle.transaction() as connection:
            connection.root()["value"] = "idle"

        # A commit whose callback never returns. A call that waits for the
        # publication of that commit holds the store.
        txn = TransactionMetaData()
        held.tpc_begin(txn)
        data = pickle.dumps(PersistentMapping, 3) + pickle.dumps({"data": {}}, 3)
        held.store(held.new_oid(), z64, data, "", txn)
        held.tpc_vote(txn)
        durable = threading.Event()


        def callback(tid):
            durable.set()
            time.sleep(3600)


        threading.Thread(
            target=held.tpc_finish, args=(txn, callback), daemon=True
        ).start()
        durable.wait(60)
        threading.Thread(target=held.lastTransaction, daemon=True).start()
        time.sleep(0.5)
        ready()
        wait_for_the_test()
        """,
        held,
        idle,
    )
    child.expect_ready()

    started = time.monotonic()
    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    took = time.monotonic() - started
    assert took < HELD_STOP, f"the child took {took:.1f} s to end"
    # A store that a call holds does not keep the other stores open.
    assert_closed_cleanly(idle)
    assert stored_value(idle, "value") == "idle"


@pytest.mark.parametrize("when", ["before", "after"])
def test_a_handler_of_the_application_wins(tmp_path, when):
    path = tmp_path / "store"
    child = Child(
        """
        def handler(signum, frame):
            # The handler of the application takes its time. So a handler
            # of the extension that also ran would end the process first.
            time.sleep(1)
            sys.exit(7)


        when, path = sys.argv[1:]
        if when == "before":
            signal.signal(signal.SIGTERM, handler)
        db = ZODB.DB(zoe.Storage(path))
        with db.transaction() as connection:
            connection.root()["value"] = when
        if when == "after":
            signal.signal(signal.SIGTERM, handler)
        ready()
        try:
            wait_for_the_test()
        finally:
            db.close()
        """,
        when,
        path,
    )
    child.expect_ready()

    child.send(signal.SIGTERM)

    assert child.wait() == 7, child.stderr()
    assert_closed_cleanly(path)
    assert stored_value(path, "value") == when


def test_an_ignored_signal_stays_ignored(tmp_path):
    path = tmp_path / "store"
    child = Child(
        """
        signal.signal(signal.SIGTERM, signal.SIG_IGN)
        db = ZODB.DB(zoe.Storage(sys.argv[1]))
        with db.transaction() as connection:
            connection.root()["value"] = "ignored"
        ready()
        wait_for_the_test()
        db.close()
        print("closed", flush=True)
        """,
        path,
    )
    child.expect_ready()

    child.send(signal.SIGTERM)
    time.sleep(1)

    assert child.running(), child.stderr()
    child.close_stdin()
    assert child.next_line() == "closed"
    assert child.wait() == 0, child.stderr()
    assert_closed_cleanly(path)


def test_a_child_of_fork_ends_on_sigterm(tmp_path):
    path = tmp_path / "store"
    child = Child(
        """
        db = ZODB.DB(zoe.Storage(sys.argv[1]))
        with db.transaction() as connection:
            connection.root()["value"] = "forked"
        ready()
        r, w = os.pipe()
        pid = os.fork()
        if pid == 0:
            # The child of the fork says that it runs, then waits.
            os.close(r)
            os.write(w, b"x")
            time.sleep(60)
            os._exit(0)
        os.close(w)
        os.read(r, 1)
        os.kill(pid, signal.SIGTERM)
        deadline = time.monotonic() + 20
        while True:
            done, status = os.waitpid(pid, os.WNOHANG)
            if done:
                break
            if time.monotonic() > deadline:
                os.kill(pid, signal.SIGKILL)
                done, status = os.waitpid(pid, 0)
                break
            time.sleep(0.05)
        print(f"status {os.waitstatus_to_exitcode(status)}", flush=True)
        db.close()
        """,
        path,
    )
    child.expect_ready()

    assert child.next_line() == f"status {-signal.SIGTERM}"
    assert child.wait() == 0, child.stderr()
    assert_closed_cleanly(path)
