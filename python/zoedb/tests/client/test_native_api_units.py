"""Unit tests of the rest of the storage API of the native client.

`just test` runs them, against `zoe serve`. They cover what the acceptance
test of `test_native_api.py` does not: the exceptions of `_call`, the
calls that the listener refuses, the sizes of `len()` and `getSize()`,
`record_iternext`, `history`, `pack`, `restore`, `server_status`, the
interfaces that the client declares and the serial of a store.
"""

import pytest
import ZODB.interfaces
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import POSKeyError, StorageTransactionError
from ZODB.utils import p64, z64

import zoe.testing
import zoe.zeo
from zoe.testing import ZoeServer

TIMEOUT = 30


@pytest.fixture
def storage(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        native = zoe.testing.native_client(
            server.address, storage="1", wait_timeout=TIMEOUT
        )
        try:
            yield native
        finally:
            native.close()


def commit(storage, records, tid=None):
    """Store `records` (oid: (serial, data)) in one transaction."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    for oid, (serial, data) in records.items():
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def test_the_label_and_the_server_sync_are_built():
    assert "client_label" not in zoe.zeo._NOT_BUILT
    assert "server_sync" not in zoe.zeo._NOT_BUILT


def test_the_serial_of_a_store_takes_the_str_of_zeo_tests():
    assert zoe.zeo._serial(None) == z64
    assert zoe.zeo._serial(b"") == z64
    assert zoe.zeo._serial("\0" * 8) == z64
    assert zoe.zeo._serial(p64(5)) == p64(5)


def test_the_client_declares_the_interfaces_of_the_server(storage):
    for iface in (
        ZODB.interfaces.IStorageIteration,
        ZODB.interfaces.IStorageCurrentRecordIteration,
        ZODB.interfaces.IStorageRestoreable,
        ZODB.interfaces.IExternalGC,
    ):
        assert iface.providedBy(storage), iface
    # The server does not undo.
    assert not ZODB.interfaces.IStorageUndoable.providedBy(storage)
    assert not storage.supportsUndo()


def test_an_error_of_the_server_raises_as_zeo_raises_it(storage):
    with pytest.raises(POSKeyError) as raised:
        storage.history(p64(42))
    # The oid stays bytes, as in ZEO.
    assert raised.value.args == (p64(42),)
    with pytest.raises(POSKeyError) as raised:
        storage.loadSerial(p64(42), p64(1))
    assert raised.value.args == (p64(42),)


def test_the_listener_refuses_a_call_of_a_transaction(storage):
    for method in ("tpc_begin", "storea", "vote", "tpc_finish"):
        with pytest.raises(StorageTransactionError):
            storage._call(method, 1)
        with pytest.raises(StorageTransactionError):
            storage._async(method, 1)


def test_len_and_get_size_follow_the_commits(storage):
    assert len(storage) == 0
    oids = [storage.new_oid() for _ in range(3)]
    commit(storage, {oid: (z64, b"x") for oid in oids})
    assert len(storage) == 3
    assert storage.getSize() > 0


def test_history_record_iternext_and_server_status(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"a")})
    second = commit(storage, {oid: (first, b"b")})
    history = storage.history(oid, 5)
    assert [entry["tid"] for entry in history] == [second, first]
    assert storage.record_iternext() == (oid, second, b"b", None)
    assert isinstance(storage.server_status(), dict)


def test_a_pack_runs_on_the_server(storage):
    oid = storage.new_oid()
    first = commit(storage, {oid: (z64, b"a")})
    second = commit(storage, {oid: (first, b"b")})
    assert storage.loadSerial(oid, first) == b"a"
    storage.pack(None, None)
    # No root reaches the object, so the pack removes each of its records.
    for serial in (first, second):
        with pytest.raises(POSKeyError):
            storage.loadSerial(oid, serial)


def test_a_restore_copies_a_record_and_a_deletion(storage):
    oid, gone = storage.new_oid(), storage.new_oid()
    last = commit(storage, {gone: (z64, b"x")})
    tid = p64(int.from_bytes(last, "big") + 1)
    txn = TransactionMetaData()
    storage.tpc_begin(txn, tid)
    storage.restore(oid, tid, b"copied", "", None, txn)
    storage.restore(gone, tid, None, "", None, txn)
    storage.tpc_vote(txn)
    assert storage.tpc_finish(txn) == tid
    assert storage.load(oid) == (b"copied", tid)
    with pytest.raises(POSKeyError):
        storage.load(gone)


def test_a_dropped_iterator_is_released_at_the_end_of_a_transaction(storage):
    commit(storage, {storage.new_oid(): (z64, b"a")})
    iterator = storage.iterator()
    next(iterator)
    iid = iterator._iid
    assert iid in storage._iterator_ids
    del iterator
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.tpc_abort(txn)
    assert iid not in storage._iterator_ids


def test_the_loss_of_the_listener_forgets_the_iterators(storage):
    commit(storage, {storage.new_oid(): (z64, b"a")})
    iterator = storage.iterator()
    next(iterator)
    storage._iterator_gc(True)
    assert not storage._iterator_ids
    from ZEO.Exceptions import ClientDisconnected

    with pytest.raises(ClientDisconnected):
        next(iterator)
