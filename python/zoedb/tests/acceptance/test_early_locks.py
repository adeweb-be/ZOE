"""Acceptance test of the extension `lock_intent`: early write-intent locks.

A client that declared `lock_intent` in `register` asks for exclusive
intent locks on the oids of the objects that its transaction changes,
before its vote. The lock manager grants them in oid order:

- a request for an oid that another client holds waits until that client
  commits or aborts, and then gets the lock;
- a request for an oid lower than an oid that the client holds fails with
  `LockOrder`; the client releases its set and asks again for the whole
  set in oid order;
- `lock-timeout` ends a wait with `ConflictError`;
- an abort and a disconnect release the intent locks.

A stock client declares nothing: it sees no extension, and it commits as
before. The vote of the holder takes the intent locks that it holds: it
does not wait for its own locks.

The test runs through `zoe serve` with `zoe.zeo.ClientStorage`, the native
client: one client for each holder of locks, each with one transaction at a
time. `just test` runs it. Each test imports ZODB and ZEO itself, so that
the module collects before they are installed.
"""

import contextlib
import threading
import time

import pytest
from support import TIMEOUT, ZERO, record

from zoe.testing import ZoeServer

# The name of the extension in `register` and in `get_info`. The
# specification names it.
EXTENSION = "lock_intent"
# The lock timeout of a server whose test waits for the timeout, in seconds.
SHORT = 2
# The lock timeout of a server whose test must never reach it, in seconds.
LONG = 20
# The time in which a grant comes after the holder ends, in seconds. It is
# far below LONG, so a grant in this time is not the end of a timeout.
PROMPT = 5
# The time that a waiter must stay blocked before the holder ends.
BLOCKED = 0.5


def lock_order():
    """Return the exception that a request out of oid order raises.

    The specification maps `LockOrder` of the core to
    `StorageTransactionError`. The ADR of `lock_intent` names the exception
    that a client gets. The human approves a change of this function with
    the ADR.
    """
    from ZODB.POSException import StorageTransactionError

    return StorageTransactionError


class Client:
    """One client of the server that runs one transaction at a time.

    `lock` and `release` are the calls of the extension. The ADR of
    `lock_intent` names them, the owner of the intent locks before
    `tpc_begin`, and how `tpc_begin` gives them to the transaction. The
    human approves a change of `lock` and `release` with the ADR. Until
    then, they take the calls of the specification: `lock_intent(oids)`
    and `unlock_intent()` of the client that holds the locks.
    """

    def __init__(self, address):
        import zoe.zeo

        self.storage = zoe.zeo.ClientStorage(address, storage="1", wait_timeout=TIMEOUT)
        self.txn = None

    def lock(self, oids):
        """Ask for the intent locks of oids. Return when they are granted."""
        self.storage.lock_intent(list(oids))

    def release(self):
        """End the transaction before `tpc_begin`: an abort of ZODB."""
        self.storage.unlock_intent()

    def begin(self):
        """Begin the transaction of the storage."""
        from ZODB.Connection import TransactionMetaData

        self.txn = TransactionMetaData(b"user", b"description")
        self.storage.tpc_begin(self.txn)

    def abort(self):
        """Abort the transaction of the storage."""
        txn, self.txn = self.txn, None
        self.storage.tpc_abort(txn)

    def commit(self, stores):
        """Commit (oid, serial, data) items. Return the tid."""
        if self.txn is None:
            self.begin()
        txn = self.txn
        try:
            for oid, serial, data in stores:
                self.storage.store(oid, serial, data, "", txn)
            self.storage.tpc_vote(txn)
            tid = self.storage.tpc_finish(txn)
        except BaseException:
            self.storage.tpc_abort(txn)
            raise
        finally:
            self.txn = None
        return tid

    def serial(self, oid):
        """Return the current serial of oid."""
        return self.storage.load(oid)[1]

    def close(self):
        self.storage.close()


class Waiter:
    """A thread that asks for intent locks and keeps the outcome."""

    def __init__(self, client, oids):
        self.client = client
        self.oids = list(oids)
        self.error = None
        self.elapsed = None
        self._thread = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        started = time.monotonic()
        try:
            self.client.lock(self.oids)
        except Exception as error:
            self.error = error
        self.elapsed = time.monotonic() - started

    def start(self):
        self._thread.start()
        return self

    def done(self):
        return not self._thread.is_alive()

    def join(self, timeout=TIMEOUT):
        self._thread.join(timeout)
        assert not self._thread.is_alive(), "the request for the locks hangs"


@contextlib.contextmanager
def serving(tmp_path, lock_timeout):
    """Run `zoe serve` with lock_timeout. Yield the server."""
    args = ["--lock-timeout", str(lock_timeout)]
    with ZoeServer(str(tmp_path / "store"), args=args) as server:
        yield server


@contextlib.contextmanager
def clients(server, count):
    """Open count clients of server. Close them at the end."""
    opened = []
    try:
        for _ in range(count):
            opened.append(Client(server.address))
        yield opened
    finally:
        for client in opened:
            with contextlib.suppress(Exception):
                client.close()


def objects(client, count):
    """Commit count new objects through client. Return their oids in order."""
    oids = sorted(client.storage.new_oid() for _ in range(count))
    client.commit([(oid, ZERO, record(f"v{n}")) for n, oid in enumerate(oids)])
    return oids


def test_a_stock_client_sees_nothing_new(tmp_path):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.Connection import TransactionMetaData

    with serving(tmp_path, LONG) as server, clients(server, 1) as (ours,):
        # The server serves the extension to a client that declares it.
        assert EXTENSION in ours.storage._info["extensions"]
        stock = ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
        try:
            assert "extensions" not in stock._info
            with pytest.raises(Exception):  # noqa: B017
                stock._call(EXTENSION, [bytes(8)])
            # The stock client commits and loads as before.
            oid = stock.new_oid()
            txn = TransactionMetaData()
            stock.tpc_begin(txn)
            stock.store(oid, ZERO, record("stock"), "", txn)
            stock.tpc_vote(txn)
            tid = stock.tpc_finish(txn)
            assert stock.load(oid) == (record("stock"), tid)
        finally:
            stock.close()


def test_the_holder_commits_its_writes_without_a_wait(tmp_path):
    with serving(tmp_path, LONG) as server, clients(server, 1) as (holder,):
        oids = objects(holder, 3)
        holder.lock(oids)
        started = time.monotonic()
        tid = holder.commit(
            [(oid, holder.serial(oid), record("holder")) for oid in oids]
        )
        # The vote takes the intent locks that the client holds.
        assert time.monotonic() - started < PROMPT
        for oid in oids:
            assert holder.storage.load(oid) == (record("holder"), tid)


def test_a_writer_waits_for_the_holder_and_then_commits(tmp_path):
    from ZODB.POSException import ConflictError

    with serving(tmp_path, LONG) as server, clients(server, 2) as (holder, writer):
        (oid,) = objects(holder, 1)
        holder.lock([oid])
        waiter = Waiter(writer, [oid]).start()
        time.sleep(BLOCKED)
        assert not waiter.done(), "the writer got a lock that another client holds"

        ended = time.monotonic()
        first = holder.commit([(oid, holder.serial(oid), record("first"))])
        waiter.join()
        assert waiter.error is None, waiter.error
        assert time.monotonic() - ended < PROMPT
        # The writer reads the object after the grant, so it reads the
        # commit of the holder, and its commit has no conflict.
        serial = writer.serial(oid)
        assert serial == first
        try:
            second = writer.commit([(oid, serial, record("second"))])
        except ConflictError as error:
            pytest.fail(f"the writer that waited got a conflict: {error}")
        assert holder.storage.load(oid) == (record("second"), second)


def test_a_lower_oid_gets_lock_order_and_then_the_whole_set_in_order(tmp_path):
    from ZODB.POSException import ConflictError

    with serving(tmp_path, SHORT) as server, clients(server, 2) as (client, other):
        low, middle, high = objects(client, 3)
        # A request above the oids that the client holds passes.
        client.lock([low])
        client.lock([middle])
        client.release()

        client.lock([high])
        with pytest.raises(lock_order()):
            client.lock([low])
        # The client releases its set and asks for the whole set in order.
        client.release()
        client.lock([low, high])

        # The client holds both locks: another client waits for each one
        # until the lock timeout.
        for oid in (low, high):
            with pytest.raises(ConflictError):
                other.lock([oid])
        # The client commits both.
        tid = client.commit(
            [(oid, client.serial(oid), record("set")) for oid in (low, high)]
        )
        assert client.storage.load(low) == (record("set"), tid)
        assert client.storage.load(high) == (record("set"), tid)


def test_a_wait_ends_at_the_lock_timeout(tmp_path):
    from ZODB.POSException import ConflictError

    with serving(tmp_path, SHORT) as server, clients(server, 2) as (holder, writer):
        (oid,) = objects(holder, 1)
        holder.lock([oid])
        waiter = Waiter(writer, [oid]).start()
        waiter.join()
        assert isinstance(waiter.error, ConflictError), waiter.error
        assert SHORT <= waiter.elapsed < SHORT + PROMPT
        # The holder keeps its lock and commits.
        holder.commit([(oid, holder.serial(oid), record("holder"))])


def end(client, how):
    """End the transaction of client that holds intent locks."""
    if how == "abort":
        client.release()
    elif how == "tpc_abort":
        client.abort()
    else:
        client.close()


@pytest.mark.parametrize("how", ["abort", "tpc_abort", "disconnect"])
def test_an_abort_and_a_disconnect_release_the_intent_locks(tmp_path, how):
    with serving(tmp_path, LONG) as server, clients(server, 2) as (holder, writer):
        oids = objects(writer, 2)
        holder.lock(oids)
        if how == "tpc_abort":
            # The transaction of the storage begins with the locks.
            holder.begin()
        waiter = Waiter(writer, oids).start()
        time.sleep(BLOCKED)
        assert not waiter.done(), "the writer got locks that another client holds"

        ended = time.monotonic()
        end(holder, how)
        waiter.join()
        assert waiter.error is None, waiter.error
        assert time.monotonic() - ended < PROMPT
        writer.commit([(oid, writer.serial(oid), record("writer")) for oid in oids])
