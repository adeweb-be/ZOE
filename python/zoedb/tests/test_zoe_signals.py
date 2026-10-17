"""Unit tests of the close of the stores on SIGTERM and SIGHUP.

The acceptance test is `acceptance/test_signal_close.py`. These tests check
the parts that it does not: which signals get a handler of the extension,
a later open, a read-only storage, an open transaction, the calls after the
signal, a thread that holds the GIL, the watcher thread and the child of a
fork.

Each test runs a child interpreter, because the handlers stay in a process.
A child reads the actions of its signals in `/proc/self/status`: a bit of
`SigCgt` is a signal with a handler, a bit of `SigIgn` an ignored signal.
It counts the watcher threads by their names in `/proc/self/task`. Only
Linux has `/proc`, so the tests that read it skip on other platforms.
"""

import os
import queue
import signal
import subprocess
import sys
import sysconfig
import tempfile
import textwrap
import threading
import time

import pytest
from ZODB.POSException import POSKeyError

import zoe

FREE_THREADED = bool(sysconfig.get_config_var("Py_GIL_DISABLED"))
# The longest time that a child takes to start, to write a line or to end.
TIMEOUT = 60
# The mark of a test whose child reads /proc.
READS_PROC = pytest.mark.skipif(
    sys.platform != "linux",
    reason="Linux only: the child reads its signal actions and threads in /proc",
)

# The start of each child: the functions that talk to the test and read the
# actions of the signals.
PRELUDE = """\
import os
import signal
import sys
import threading
import time


def ready():
    print("ready", flush=True)


def wait_for_the_test():
    sys.stdin.read()


def mask(name):
    with open("/proc/self/status") as status:
        for line in status:
            if line.startswith(name + ":"):
                return int(line.split()[1], 16)


def caught(signum):
    return bool(mask("SigCgt") >> (signum - 1) & 1)


def ignored(signum):
    return bool(mask("SigIgn") >> (signum - 1) & 1)


def watchers():
    count = 0
    for task in os.listdir("/proc/self/task"):
        with open(f"/proc/self/task/{task}/comm") as comm:
            count += comm.read().strip() == "zoe-signals"
    return count


def started_watchers():
    # A new thread sets its own name, so the name shows a short time after
    # the thread starts. Wait for it.
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        count = watchers()
        if count:
            return count
        time.sleep(0.01)
    return watchers()
"""


def environment():
    """Return the environment of a child, without the ZOE_* variables."""
    return {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}


class Child:
    """A child interpreter that runs code, with its arguments.

    The child can wait on its stdin until the test closes it. A thread of
    the test reads the lines of its stdout, so the pipe never fills.
    """

    def __init__(self, code, *args, flags=()):
        self.stderr_file = tempfile.TemporaryFile(mode="w+")
        self.process = subprocess.Popen(
            [
                sys.executable,
                # A fork in a process with threads warns from CPython 3.12.
                "-W",
                "ignore::DeprecationWarning",
                *flags,
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
        threading.Thread(target=self._read, daemon=True).start()

    def _read(self):
        for line in self.process.stdout:
            self.lines.put(line.rstrip("\n"))
        self.lines.put(None)

    def stderr(self):
        self.stderr_file.seek(0)
        return self.stderr_file.read()

    def next_line(self):
        try:
            line = self.lines.get(timeout=TIMEOUT)
        except queue.Empty:
            self.process.kill()
            pytest.fail(f"the child wrote no line in {TIMEOUT} s:\n{self.stderr()}")
        if line is None:
            self.process.wait()
            pytest.fail(f"the child ended early:\n{self.stderr()}")
        return line

    def expect(self, line):
        assert self.next_line() == line, self.stderr()

    def send(self, signum):
        self.process.send_signal(signum)

    def running(self):
        return self.process.poll() is None

    def close_stdin(self):
        self.process.stdin.close()

    def wait(self):
        try:
            return self.process.wait(TIMEOUT)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait()
            pytest.fail(f"the child did not end in {TIMEOUT} s:\n{self.stderr()}")


def assert_closed_cleanly(path):
    """Check that `zoe verify` accepts the store and changes no byte of it."""
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
    assert database.read_bytes() == before, "the store was not closed cleanly"


def create_store(path):
    """Create a store with one commit, in a child interpreter."""
    code = """
        import ZODB
        import zoe
        db = ZODB.DB(zoe.Storage(sys.argv[1]))
        with db.transaction() as connection:
            connection.root()["value"] = 1
        db.close()
        """
    child = Child(code, path)
    child.close_stdin()
    assert child.wait() == 0, child.stderr()


@READS_PROC
def test_the_open_installs_a_handler_of_each_signal_whose_action_is_the_default(
    tmp_path,
):
    child = Child(
        """
        import zoe
        print(caught(signal.SIGTERM), caught(signal.SIGHUP), watchers())
        storage = zoe.Storage(sys.argv[1])
        print(caught(signal.SIGTERM), caught(signal.SIGHUP), started_watchers())
        # Python still sees the default action.
        print(signal.getsignal(signal.SIGTERM) is signal.SIG_DFL)
        storage.close()
        """,
        tmp_path / "store",
    )
    child.expect("False False 0")
    child.expect("True True 1")
    child.expect("True")
    child.close_stdin()
    assert child.wait() == 0, child.stderr()


@READS_PROC
def test_one_watcher_thread_serves_every_storage(tmp_path):
    child = Child(
        """
        import zoe
        storages = [zoe.Storage(path) for path in sys.argv[1:]]
        print(started_watchers())
        ready()
        wait_for_the_test()
        """,
        *(tmp_path / name for name in ("a", "b", "c")),
    )
    child.expect("1")
    child.expect("ready")

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    for name in ("a", "b", "c"):
        assert_closed_cleanly(tmp_path / name)


@READS_PROC
def test_an_ignored_sighup_keeps_its_action_and_sigterm_still_closes(tmp_path):
    path = tmp_path / "store"
    child = Child(
        """
        import zoe
        # As nohup starts a process.
        signal.signal(signal.SIGHUP, signal.SIG_IGN)
        storage = zoe.Storage(sys.argv[1])
        print(ignored(signal.SIGHUP), caught(signal.SIGHUP), caught(signal.SIGTERM))
        ready()
        wait_for_the_test()
        """,
        path,
    )
    child.expect("True False True")
    child.expect("ready")

    child.send(signal.SIGHUP)
    time.sleep(1)
    assert child.running(), child.stderr()
    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    assert_closed_cleanly(path)


@READS_PROC
def test_a_later_open_installs_a_handler_that_an_earlier_open_did_not(tmp_path):
    first, second = tmp_path / "first", tmp_path / "second"
    child = Child(
        """
        import zoe
        signal.signal(signal.SIGTERM, lambda signum, frame: None)
        first = zoe.Storage(sys.argv[1])
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        print(caught(signal.SIGTERM))
        second = zoe.Storage(sys.argv[2])
        print(caught(signal.SIGTERM))
        ready()
        wait_for_the_test()
        """,
        first,
        second,
    )
    child.expect("False")
    child.expect("True")
    child.expect("ready")

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    assert_closed_cleanly(first)
    assert_closed_cleanly(second)


@READS_PROC
def test_the_extension_never_puts_its_handler_back(tmp_path):
    child = Child(
        """
        import zoe
        first = zoe.Storage(sys.argv[1])
        signal.signal(signal.SIGTERM, lambda signum, frame: None)
        signal.signal(signal.SIGTERM, signal.SIG_DFL)
        second = zoe.Storage(sys.argv[2])
        print(caught(signal.SIGTERM), caught(signal.SIGHUP))
        first.close()
        second.close()
        """,
        tmp_path / "first",
        tmp_path / "second",
    )
    child.expect("False True")
    child.close_stdin()
    assert child.wait() == 0, child.stderr()


@READS_PROC
def test_a_read_only_storage_installs_the_handlers_and_closes(tmp_path):
    path = tmp_path / "store"
    create_store(path)
    child = Child(
        """
        import zoe
        storage = zoe.Storage(sys.argv[1], read_only=True)
        print(caught(signal.SIGTERM), caught(signal.SIGHUP))
        ready()
        wait_for_the_test()
        """,
        path,
    )
    child.expect("True True")
    child.expect("ready")

    child.send(signal.SIGHUP)

    assert child.wait() == -signal.SIGHUP, child.stderr()
    assert_closed_cleanly(path)


@pytest.mark.parametrize(
    "signum", [signal.SIGTERM, signal.SIGHUP], ids=["SIGTERM", "SIGHUP"]
)
def test_the_signal_ends_a_process_whose_storages_are_closed(tmp_path, signum):
    path = tmp_path / "store"
    child = Child(
        """
        import zoe
        storage = zoe.Storage(sys.argv[1])
        storage.close()
        del storage
        ready()
        wait_for_the_test()
        """,
        path,
    )
    child.expect("ready")

    child.send(signum)

    assert child.wait() == -signum, child.stderr()
    assert_closed_cleanly(path)


def test_the_signal_aborts_the_open_transactions(tmp_path):
    path = tmp_path / "store"
    create_store(path)
    child = Child(
        """
        import pickle

        from persistent.mapping import PersistentMapping
        from ZODB.Connection import TransactionMetaData
        from ZODB.utils import z64

        import zoe

        storage = zoe.Storage(sys.argv[1])
        data = pickle.dumps(PersistentMapping, 3) + pickle.dumps({"data": {}}, 3)
        # One transaction that voted and holds its locks, one that did not
        # vote.
        voted, staged = TransactionMetaData(), TransactionMetaData()
        for txn in (voted, staged):
            oid = storage.new_oid()
            storage.tpc_begin(txn)
            storage.store(oid, z64, data, "", txn)
            print(oid.hex(), flush=True)
        storage.tpc_vote(voted)
        ready()
        wait_for_the_test()
        """,
        path,
    )
    oids = [bytes.fromhex(child.next_line()) for _ in range(2)]
    child.expect("ready")

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    assert_closed_cleanly(path)
    storage = zoe.Storage(str(path))
    try:
        for oid in oids:
            with pytest.raises(POSKeyError):
                storage.load(oid)
    finally:
        storage.close()


def test_after_the_signal_the_calls_raise_as_after_close(tmp_path):
    # A call that never returns holds the store, so the watcher waits for
    # it until the bound, and the child can make calls in that time.
    child = Child(
        """
        import pickle

        from persistent.mapping import PersistentMapping
        from ZODB.Connection import TransactionMetaData
        from ZODB.POSException import StorageError
        from ZODB.utils import z64

        import zoe

        storage = zoe.Storage(sys.argv[1])
        data = pickle.dumps(PersistentMapping, 3) + pickle.dumps({"data": {}}, 3)


        def staged():
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            storage.store(storage.new_oid(), z64, data, "", txn)
            return txn


        # A commit whose callback never returns, and a call that waits for
        # its publication.
        held = staged()
        storage.tpc_vote(held)
        durable = threading.Event()


        def callback(tid):
            durable.set()
            time.sleep(3600)


        threading.Thread(
            target=storage.tpc_finish, args=(held, callback), daemon=True
        ).start()
        durable.wait(60)
        threading.Thread(target=storage.lastTransaction, daemon=True).start()
        txn = staged()
        ready()
        while True:
            try:
                storage.new_oid()
            except StorageError:
                break
            time.sleep(0.01)
        for call in (
            lambda: storage.tpc_vote(txn),
            lambda: storage.tpc_finish(txn),
            lambda: storage.tpc_begin(TransactionMetaData()),
        ):
            try:
                call()
                print("no error", flush=True)
            except Exception as error:
                print(type(error).__name__, flush=True)
        wait_for_the_test()
        """,
        tmp_path / "store",
    )
    child.expect("ready")

    child.send(signal.SIGTERM)

    child.expect("StorageTransactionError")
    child.expect("StorageTransactionError")
    child.expect("StorageError")
    assert child.wait() == -signal.SIGTERM, child.stderr()


def test_the_close_needs_no_gil(tmp_path):
    # On a build with the GIL, the main thread holds the GIL in a long call
    # of C code that never lets it go. The watcher closes the store all the
    # same, because it never takes the GIL.
    path = tmp_path / "store"
    child = Child(
        """
        import ZODB
        import zoe
        db = ZODB.DB(zoe.Storage(sys.argv[1]))
        with db.transaction() as connection:
            connection.root()["value"] = 1
        ready()
        sum(range(10**18))
        """,
        path,
    )
    child.expect("ready")
    time.sleep(0.5)

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    assert_closed_cleanly(path)


@pytest.mark.skipif(not FREE_THREADED, reason="a free-threaded build only")
@READS_PROC
def test_the_install_keeps_the_gil_off(tmp_path):
    # The child imports only the extension, so no module of ZODB turns the
    # GIL back on.
    path = tmp_path / "store"
    child = Child(
        """
        from zoe import _zoe
        storage = _zoe.Storage(sys.argv[1])
        print(sys._is_gil_enabled(), caught(signal.SIGTERM))
        ready()
        wait_for_the_test()
        """,
        path,
    )
    child.expect("False True")
    child.expect("ready")

    child.send(signal.SIGTERM)

    assert child.wait() == -signal.SIGTERM, child.stderr()
    assert_closed_cleanly(path)


@READS_PROC
def test_the_child_of_a_fork_keeps_a_handler_of_the_application(tmp_path):
    path = tmp_path / "store"
    child = Child(
        """
        import zoe


        def handler(signum, frame):
            print("handler", flush=True)


        storage = zoe.Storage(sys.argv[1])
        signal.signal(signal.SIGTERM, handler)
        pid = os.fork()
        if pid == 0:
            print(
                caught(signal.SIGHUP),
                signal.getsignal(signal.SIGTERM) is handler,
                flush=True,
            )
            os.kill(os.getpid(), signal.SIGTERM)
            # The default action of SIGHUP ends the child. It does not wake
            # the watcher of the parent.
            os.kill(os.getpid(), signal.SIGHUP)
            time.sleep(60)
            os._exit(0)
        _, status = os.waitpid(pid, 0)
        print(f"status {os.waitstatus_to_exitcode(status)}", flush=True)
        time.sleep(0.5)
        print("parent", flush=True)
        storage.close()
        """,
        path,
    )
    child.expect("False True")
    child.expect("handler")
    child.expect(f"status {-signal.SIGHUP}")
    child.expect("parent")
    child.close_stdin()
    assert child.wait() == 0, child.stderr()
    assert_closed_cleanly(path)
