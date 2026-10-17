"""Acceptance test of the KV namespaces (ADR 0043).

A namespace holds keys beside the object graph, in one of two flavours:
`ephemeral`, where each key has a TTL, and `durable`. Neither keeps
history, and the pack never reads them. `zoe.kv.Namespace(connection,
flavour, name)` writes in the transaction of the connection: `set(key,
value, ttl=None)` and `delete(key)` commit or abort with it. `get`, `scan`
and `generation` read the last committed values, not the values at the
snapshot of the connection. `zoe.Storage` and `zoe.zeo.ClientStorage` have
the calls of the extension `kv` as methods: `kv_get`, `kv_scan`,
`kv_generation`, `kv_set` and `kv_delete`.

The test checks:

- an ephemeral key reads as absent after its TTL, without a pack, also
  after a restart; a key with a long TTL stays;
- a namespace keeps no history: a connection whose snapshot is older than
  two writes of a key reads the second value, and the commits of the writes
  hold no record;
- a durable key survives a restart and a crash, and the generation of its
  namespace is the tid of the last commit that wrote it;
- a write of each flavour commits or aborts with its ZODB transaction:
  with a change of an object, alone, after a failed vote, and with a
  savepoint; a transaction reads its own writes first;
- a stock `ZEO.ClientStorage` against a server with the extension sees
  nothing new: no extension, a refused call, and a commit of namespace
  writes as a transaction without records.

The test runs in-process on `zoe.Storage`, and through `zoe serve` with
`zoe.zeo.ClientStorage`, with the native client inside and with the
asyncio client of ZEO inside. `just test` runs it.
"""

import itertools
import pickle
import subprocess
import sys
import textwrap
import time

import pytest

import zoe
from zoe.testing import ZoeServer

# The longest time that a test waits for the server or a child.
TIMEOUT = 60
# The modes: in-process, and through zoe serve with each client of zoe.
MODES = ("in-process", "native-client", "asyncio-client")
FLAVOURS = ("ephemeral", "durable")
# The TTL of a key that expires in the test, and of a key that stays, in
# seconds.
SHORT = 3
LONG = 3600


def record(value):
    """Return a ZODB record of a persistent mapping that holds value."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


def ttl(flavour):
    """Return the TTL of a write of flavour: a long one, or none."""
    return LONG if flavour == "ephemeral" else None


class Place:
    """The store of a test in one mode, and the server of the store."""

    def __init__(self, mode, tmp_path):
        self.mode = mode
        self.path = tmp_path / "store"
        self.server = None

    def __enter__(self):
        if self.mode != "in-process":
            self.server = ZoeServer(str(self.path)).start(TIMEOUT)
        return self

    def __exit__(self, *exc_info):
        if self.server is not None:
            self.server.kill()

    def open(self):
        """Open a new database on the store."""
        import ZODB

        if self.mode == "in-process":
            storage = zoe.Storage(str(self.path))
        else:
            from zoe.zeo import ClientStorage

            storage = ClientStorage(
                self.server.address,
                storage="1",
                wait_timeout=TIMEOUT,
                native=self.mode == "native-client",
            )
        return ZODB.DB(storage)

    def restart(self):
        """Stop the server cleanly and start it again."""
        if self.server is not None:
            assert self.server.stop(TIMEOUT) == 0
            self.server.start(TIMEOUT)

    def crash(self, write):
        """Run write(db) and crash the writer once the commit returned.

        In-process, a child interpreter writes and dies by SIGKILL. Through
        zoe serve, the server dies by SIGKILL. Return the tid of the commit.
        """
        if self.mode == "in-process":
            return crash_a_child(self.path, write.__name__)
        db = self.open()
        try:
            write(db)
            tid = db.lastTransaction()
        finally:
            db.close()
        self.server.kill()
        self.server.start(TIMEOUT)
        return tid


def namespace(conn, flavour, name="test"):
    import zoe.kv

    return zoe.kv.Namespace(conn, flavour, name)


def pairs(scanned):
    """Return the pairs of a scan as tuples."""
    return [tuple(pair) for pair in scanned]


def write_first(db):
    with db.transaction() as conn:
        namespace(conn, "durable", "registry").set(b"first", b"one")


def write_second(db):
    with db.transaction() as conn:
        namespace(conn, "durable", "registry").set(b"second", b"two")


CHILD = textwrap.dedent(
    """\
    import sys

    import ZODB

    import test_kv_namespaces as test
    import zoe

    db = ZODB.DB(zoe.Storage(sys.argv[1]))
    getattr(test, sys.argv[2])(db)
    print("committed", db.lastTransaction().hex(), flush=True)
    sys.stdin.read()
    """
)


def crash_a_child(path, function):
    """Run the write function of this module in a child interpreter on the
    store in path, and kill the child after the commit. Return the tid."""
    import os

    environ = dict(os.environ)
    here = os.path.dirname(os.path.abspath(__file__))
    environ["PYTHONPATH"] = os.pathsep.join(
        filter(None, [here, environ.get("PYTHONPATH")])
    )
    process = subprocess.Popen(
        [sys.executable, "-c", CHILD, str(path), function],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        env=environ,
        text=True,
    )
    try:
        line = process.stdout.readline()
    finally:
        process.kill()
        process.wait()
        process.stdout.close()
        process.stdin.close()
    word, _, tid = line.strip().partition(" ")
    assert word == "committed", f"the child did not commit: {line!r}"
    return bytes.fromhex(tid)


@pytest.mark.parametrize("mode", MODES)
def test_an_ephemeral_key_expires_without_a_pack(mode, tmp_path):
    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            started = time.time()
            with db.transaction() as conn:
                sessions = namespace(conn, "ephemeral", "sessions")
                sessions.set(b"short", b"one", ttl=SHORT)
                sessions.set(b"long", b"two", ttl=LONG)
            committed = time.time()
            with db.transaction() as conn:
                sessions = namespace(conn, "ephemeral", "sessions")
                assert time.time() - started < SHORT, "the commit took the whole TTL"
                assert sessions.get(b"short") == b"one"
            time.sleep(max(0.0, committed + SHORT + 1 - time.time()))
            with db.transaction() as conn:
                sessions = namespace(conn, "ephemeral", "sessions")
                assert sessions.get(b"short") is None
                assert pairs(sessions.scan()) == [(b"long", b"two")]
            assert db.storage.kv_get("ephemeral", "sessions", b"short") is None
        finally:
            db.close()
        # The expired key stays absent after a restart, without a pack.
        place.restart()
        db = place.open()
        try:
            assert db.storage.kv_get("ephemeral", "sessions", b"short") is None
            assert db.storage.kv_get("ephemeral", "sessions", b"long") == b"two"
        finally:
            db.close()


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("flavour", FLAVOURS)
def test_a_namespace_keeps_no_history(mode, flavour, tmp_path):
    import transaction

    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            start = db.lastTransaction()
            manager = transaction.TransactionManager()
            reader = db.open(transaction_manager=manager)
            manager.begin()
            reader.root()
            for value in (b"one", b"two"):
                with db.transaction() as conn:
                    namespace(conn, flavour).set(b"key", value, ttl=ttl(flavour))
            # The reader began before both writes, and reads the last value.
            assert namespace(reader, flavour).get(b"key") == b"two"
            manager.abort()
            reader.close()
            written = [txn for txn in db.storage.iterator(start) if txn.tid != start]
            assert len(written) == 2
            assert all(list(txn) == [] for txn in written)
        finally:
            db.close()


@pytest.mark.parametrize("mode", MODES)
def test_a_durable_key_survives_a_restart_and_a_crash(mode, tmp_path):
    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            write_first(db)
        finally:
            db.close()
        place.restart()
        db = place.open()
        try:
            assert db.storage.kv_get("durable", "registry", b"first") == b"one"
        finally:
            db.close()

        tid = place.crash(write_second)
        db = place.open()
        try:
            storage = db.storage
            assert storage.kv_get("durable", "registry", b"first") == b"one"
            assert storage.kv_get("durable", "registry", b"second") == b"two"
            assert storage.kv_generation("registry") == tid
        finally:
            db.close()


def visible(db, flavour, key):
    """Return the value of key that a new transaction reads."""
    with db.transaction() as conn:
        return namespace(conn, flavour).get(key)


@pytest.mark.parametrize("mode", MODES)
@pytest.mark.parametrize("flavour", FLAVOURS)
def test_a_write_commits_or_aborts_with_its_transaction(mode, flavour, tmp_path):
    import transaction
    from persistent.mapping import PersistentMapping
    from ZODB.POSException import ConflictError

    keys = (b"k%d" % n for n in itertools.count())
    with Place(mode, tmp_path) as place:
        db = place.open()
        try:
            with db.transaction() as conn:
                conn.root()["object"] = PersistentMapping(v=0)
            manager = transaction.TransactionManager()
            conn = db.open(transaction_manager=manager)

            # With a change of an object: commit, then abort.
            committed, aborted = next(keys), next(keys)
            manager.begin()
            namespace(conn, flavour).set(committed, b"yes", ttl=ttl(flavour))
            conn.root()["object"]["v"] = 1
            # The transaction reads its own write first.
            assert namespace(conn, flavour).get(committed) == b"yes"
            manager.commit()
            manager.begin()
            namespace(conn, flavour).set(aborted, b"no", ttl=ttl(flavour))
            conn.root()["object"]["v"] = 2
            manager.abort()
            assert visible(db, flavour, committed) == b"yes"
            assert visible(db, flavour, aborted) is None
            with db.transaction() as other:
                assert other.root()["object"]["v"] == 1

            # Alone, without a change of an object.
            alone, alone_aborted = next(keys), next(keys)
            manager.begin()
            namespace(conn, flavour).set(alone, b"yes", ttl=ttl(flavour))
            manager.commit()
            manager.begin()
            namespace(conn, flavour).set(alone_aborted, b"no", ttl=ttl(flavour))
            manager.abort()
            assert visible(db, flavour, alone) == b"yes"
            assert visible(db, flavour, alone_aborted) is None

            # After a vote that fails with a conflict.
            failed = next(keys)
            manager.begin()
            namespace(conn, flavour).set(failed, b"no", ttl=ttl(flavour))
            conn.root()["object"]["v"] = 3
            with db.transaction() as other:
                other.root()["object"]["v"] = 4
            with pytest.raises(ConflictError):
                manager.commit()
            manager.abort()
            assert visible(db, flavour, failed) is None

            # A delete commits or aborts too.
            manager.begin()
            namespace(conn, flavour).delete(committed)
            manager.abort()
            assert visible(db, flavour, committed) == b"yes"
            manager.begin()
            namespace(conn, flavour).delete(committed)
            assert namespace(conn, flavour).get(committed) is None
            manager.commit()
            assert visible(db, flavour, committed) is None

            # A savepoint rolls back the writes after it.
            kept, rolled_back = next(keys), next(keys)
            manager.begin()
            namespace(conn, flavour).set(kept, b"yes", ttl=ttl(flavour))
            savepoint = manager.savepoint()
            namespace(conn, flavour).set(rolled_back, b"no", ttl=ttl(flavour))
            savepoint.rollback()
            manager.commit()
            assert visible(db, flavour, kept) == b"yes"
            assert visible(db, flavour, rolled_back) is None
            conn.close()
        finally:
            db.close()


def test_a_stock_client_sees_nothing_new(tmp_path):
    from ZEO.ClientStorage import ClientStorage
    from ZODB.Connection import TransactionMetaData
    from ZODB.utils import z64

    with Place("native-client", tmp_path) as place:
        db = place.open()
        stock = ClientStorage(place.server.address, storage="1", wait_timeout=TIMEOUT)
        try:
            # The server serves the extension to a client that declares it.
            assert "kv" in db.storage._info["extensions"]
            assert "extensions" not in stock._info
            with pytest.raises(Exception):  # noqa: B017
                stock._call("kv_get", "durable", "registry", b"key")

            with db.transaction() as conn:
                namespace(conn, "durable", "registry").set(b"key", b"value")
            tid = db.lastTransaction()
            # The stock client sees a transaction without records.
            (txn,) = list(stock.iterator(tid, tid))
            assert list(txn) == []

            # It commits and loads as before.
            oid = stock.new_oid()
            data = record("stock")
            meta = TransactionMetaData()
            stock.tpc_begin(meta)
            stock.store(oid, z64, data, "", meta)
            stock.tpc_vote(meta)
            committed = stock.tpc_finish(meta)
            assert stock.load(oid) == (data, committed)
        finally:
            stock.close()
            db.close()
