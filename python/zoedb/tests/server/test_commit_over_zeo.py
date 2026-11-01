"""Acceptance test of the commit methods of `zoe serve`.

Through an unmodified `ZEO.ClientStorage`:

- `checkCurrentSerialInTransaction` takes a shared lock, and a stale
  read-current serial fails the vote with `ReadConflictError` on its oid;
- `deleteObject` writes a deletion: the object no longer loads, and
  `record_iternext` skips it;
- a stale written serial fails the vote with `ConflictError`, with `oid`
  and `serials == (current, read)`;
- a lock wait longer than `--lock-timeout` fails the vote with
  `ConflictError`;
- `tpc_begin` with a tid begins a restore transaction that commits under
  that tid;
- the locks are per oid: a transaction that holds the lock of one oid
  between its vote and its finish does not stop a commit on another oid,
  and an abort after the vote releases its locks.

The stale serial and the abort after the vote run twice: through a stock
client and through the native client of `zoe.zeo`.
"""

import threading
import time

import pytest
from support import TIMEOUT, ZERO, client, commit, native, record

from zoe.testing import ZoeServer

# The clients of the tests that run through both kinds of client.
OPENERS = {"zeo-client": client, "native-client": native}


def begin(storage, tid=None):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(b"user", b"description")
    if tid is None:
        storage.tpc_begin(txn)
    else:
        storage.tpc_begin(txn, tid)
    return txn


def after(tid):
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store"), args=["--lock-timeout", "1"]) as server:
        yield server


@pytest.fixture
def storage(server):
    storage = client(server)
    yield storage
    storage.close()


@pytest.fixture
def other(server):
    storage = client(server)
    yield storage
    storage.close()


@pytest.fixture(params=list(OPENERS))
def pair(request, server):
    """Two clients of `server` of one kind: stock or native."""
    opened = []
    try:
        for _ in range(2):
            opened.append(OPENERS[request.param](server))
        yield opened
    finally:
        for storage in opened:
            storage.close()


def test_a_stale_read_current_serial_is_a_read_conflict(storage):
    from ZODB.POSException import ReadConflictError

    read, written = storage.new_oid(), storage.new_oid()
    first = commit(storage, [(read, ZERO, record("a")), (written, ZERO, record("b"))])
    commit(storage, [(read, first, record("a2"))])

    txn = begin(storage)
    storage.checkCurrentSerialInTransaction(read, first, txn)
    storage.store(written, first, record("b2"), "", txn)
    with pytest.raises(ReadConflictError) as caught:
        storage.tpc_vote(txn)
    storage.tpc_abort(txn)
    assert caught.value.oid == read


def test_a_current_read_serial_commits(storage):
    read, written = storage.new_oid(), storage.new_oid()
    first = commit(storage, [(read, ZERO, record("a")), (written, ZERO, record("b"))])
    txn = begin(storage)
    storage.checkCurrentSerialInTransaction(read, first, txn)
    storage.store(written, first, record("b2"), "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    assert storage.load(written) == (record("b2"), tid)
    assert storage.load(read) == (record("a"), first)


def test_delete_object_writes_a_deletion(storage):
    from ZODB.POSException import POSKeyError

    kept, gone = storage.new_oid(), storage.new_oid()
    first = commit(storage, [(kept, ZERO, record("k")), (gone, ZERO, record("g"))])
    txn = begin(storage)
    storage.deleteObject(gone, first, txn)
    storage.tpc_vote(txn)
    deleted = storage.tpc_finish(txn)
    assert deleted > first

    with pytest.raises(POSKeyError):
        storage.load(gone)
    # The revision before the deletion still loads.
    assert storage.loadSerial(gone, first) == record("g")
    oid, _, _, next_oid = storage.record_iternext()
    assert oid == kept
    assert next_oid is None


def test_a_stale_serial_is_a_conflict_with_its_serials(pair):
    from ZODB.POSException import ConflictError

    storage, other = pair
    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record(1))])
    second = commit(other, [(oid, first, record(2))])

    # The client raises at the store or at the vote.
    with pytest.raises(ConflictError) as caught:
        commit(storage, [(oid, first, record(3))])
    assert caught.value.oid == oid
    assert caught.value.serials == (second, first)
    assert tuple(storage.load(oid)) == (record(2), second)


def test_a_lock_wait_past_the_timeout_is_a_conflict(storage, other):
    from ZODB.POSException import ConflictError

    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record(1))])

    # `storage` votes and keeps the lock of oid.
    holder = begin(storage)
    storage.store(oid, first, record(2), "", holder)
    storage.tpc_vote(holder)

    started = time.monotonic()
    with pytest.raises(ConflictError) as caught:
        commit(other, [(oid, first, record(3))])
    waited = time.monotonic() - started
    assert caught.value.oid == oid
    assert 0.5 <= waited < TIMEOUT

    tid = storage.tpc_finish(holder)
    assert storage.load(oid) == (record(2), tid)


def test_locks_are_per_oid(storage, other):
    held, free = storage.new_oid(), storage.new_oid()
    first = commit(storage, [(held, ZERO, record("h")), (free, ZERO, record("f"))])

    holder = begin(storage)
    storage.store(held, first, record("h2"), "", holder)
    storage.tpc_vote(holder)

    # A commit on another oid does not wait for the held lock.
    done = []
    thread = threading.Thread(
        target=lambda: done.append(commit(other, [(free, first, record("f2"))])),
        daemon=True,
    )
    thread.start()
    thread.join(TIMEOUT)
    assert done, "a commit on another oid waited for a lock it does not need"

    storage.tpc_finish(holder)


def test_an_abort_after_the_vote_releases_the_locks(pair):
    storage, other = pair
    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record(1))])

    txn = begin(storage)
    try:
        storage.store(oid, first, record("aborted"), "", txn)
        storage.tpc_vote(txn)
    finally:
        storage.tpc_abort(txn)

    # With the locks still held, the vote of the other client fails after
    # the lock timeout of the server.
    started = time.monotonic()
    tid = commit(other, [(oid, first, record(2))])
    assert time.monotonic() - started < 0.5
    assert tid > first
    assert tuple(other.load(oid)) == (record(2), tid)


def test_tpc_begin_with_a_tid_commits_under_that_tid(storage):
    from ZODB.POSException import StorageTransactionError

    oid = storage.new_oid()
    first = commit(storage, [(oid, ZERO, record(1))])
    wanted = after(after(first))

    txn = begin(storage, wanted)
    storage.store(oid, first, record(2), "", txn)
    storage.tpc_vote(txn)
    assert storage.tpc_finish(txn) == wanted
    assert storage.load(oid) == (record(2), wanted)
    assert storage.lastTransaction() == wanted

    # A tid that is not after the last tid is refused.
    with pytest.raises(StorageTransactionError):
        txn = begin(storage, first)
        try:
            storage.store(oid, wanted, record(3), "", txn)
            storage.tpc_vote(txn)
        finally:
            storage.tpc_abort(txn)
