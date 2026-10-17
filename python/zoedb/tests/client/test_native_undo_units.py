"""Unit tests of the undo of the native client against `zoe serve`.

`just test` runs this module. The acceptance test is `test_undo.py`. These
tests compare the native client with `ZEO.ClientStorage` on the same
server: the error of an undo that does not merge, and an undo id as a
`str`.
"""

import pickle

import pytest
from ZODB.Connection import TransactionMetaData
from ZODB.POSException import UndoError
from ZODB.utils import z64

import zoe.testing
from zoe.testing import ZoeServer

TIMEOUT = 30


def mapping(value):
    """A record of a `PersistentMapping`, which no tier merges."""
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"v": value}}, 3)


def commit(storage, oid, serial, data):
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def undo(storage, undo_id):
    """Undo the transaction of `undo_id`. Return the oids of the vote."""
    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    try:
        storage.undo(undo_id, txn)
        oids = storage.tpc_vote(txn)
        storage.tpc_finish(txn)
        return oids
    except BaseException:
        storage.tpc_abort(txn)
        raise


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


def clients(server):
    from ZEO.ClientStorage import ClientStorage

    kw = dict(storage="1", wait_timeout=TIMEOUT)
    return (
        ClientStorage(server.address, **kw),
        zoe.testing.native_client(server.address, **kw),
    )


def test_an_undo_that_does_not_merge_raises_the_error_of_the_stock_client(server):
    stock, native = clients(server)
    try:
        oid = stock.new_oid()
        first = commit(stock, oid, z64, mapping(1))
        second = commit(stock, oid, first, mapping(2))
        commit(stock, oid, second, mapping(3))
        undo_id = [e["id"] for e in stock.undoLog(0, 3)][1]
        errors = []
        for storage in (stock, native):
            with pytest.raises(UndoError) as raised:
                undo(storage, undo_id)
            errors.append(raised.value)
        theirs, ours = errors
        assert type(ours) is type(theirs)
        assert ours._errs == theirs._errs
        assert str(ours) == str(theirs)
        assert ours._errs[0][0] == oid
    finally:
        stock.close()
        native.close()


def test_the_native_client_takes_an_undo_id_as_a_str(server):
    stock, native = clients(server)
    try:
        oid = native.new_oid()
        first = commit(native, oid, z64, mapping(1))
        commit(native, oid, first, mapping(2))
        undo_id = native.undoLog(0, 1)[0]["id"]
        assert list(undo(native, undo_id.decode("ascii"))) == [oid]
        data, _ = native.load(oid)
        assert data == mapping(1)
    finally:
        stock.close()
        native.close()
