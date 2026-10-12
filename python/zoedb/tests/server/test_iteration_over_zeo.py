"""Acceptance test of the copy, backup and pack methods of `zoe serve`.

Through an unmodified `ZEO.ClientStorage`:

- `iterator(start, stop)` gives each transaction of the range with its tid,
  user, description and extension, and each of its records, through
  `iterator_start`, `iterator_next`, `iterator_record_start`,
  `iterator_record_next` and `iterator_gc`. The extension comes as the
  bytes that the store keeps, which ZODB unpickles.
- `restore` (`restorea`) copies a transaction under its own tid, so
  `copyTransactionsFrom` and `zodbconvert` copy a FileStorage into the
  server and the server into a FileStorage, with the same tids and pickles.
- `pack(t)` runs the online pack of the store at the tid of the time `t`,
  as FileStorage computes it, and removes what no reader at or after it can
  reach.
"""

import pytest

from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30


def client(server):
    from ZEO.ClientStorage import ClientStorage

    return ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)


def commit(storage, stores, user=b"user", description=b"description", extension=None):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData(user, description, extension)
    storage.tpc_begin(txn)
    for oid, serial, data in stores:
        storage.store(oid, serial, data, "", txn)
    storage.tpc_vote(txn)
    return storage.tpc_finish(txn)


def transactions(storage, start=None, stop=None):
    """Return each transaction of the iterator as a plain tuple."""
    result = []
    for txn in storage.iterator(start, stop):
        records = [(r.oid, r.tid, r.data) for r in txn]
        result.append((txn.tid, txn.user, txn.description, txn.extension, records))
    return result


@pytest.fixture
def server(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        yield server


@pytest.fixture
def storage(server):
    storage = client(server)
    yield storage
    storage.close()


def test_the_iterator_gives_each_transaction_with_its_records(storage):
    a, b = storage.new_oid(), storage.new_oid()
    first = commit(storage, [(a, ZERO, b"a1")], extension={"note": "first"})
    second = commit(
        storage,
        [(a, first, b"a2"), (b, ZERO, b"b1")],
        user=b"editor",
        description=b"second",
    )

    found = transactions(storage)
    assert found == [
        (first, b"user", b"description", {"note": "first"}, [(a, first, b"a1")]),
        (second, b"editor", b"second", {}, [(a, second, b"a2"), (b, second, b"b1")]),
    ]
    assert [t[0] for t in transactions(storage, second)] == [second]
    assert [t[0] for t in transactions(storage, None, first)] == [first]


def test_an_abandoned_iterator_does_not_disturb_the_next(storage):
    oids = [storage.new_oid() for _ in range(3)]
    tids = [commit(storage, [(oid, ZERO, b"x")]) for oid in oids]
    for _ in range(3):
        iterator = iter(storage.iterator())
        next(iterator)
        del iterator
    assert [t[0] for t in transactions(storage)] == tids


def test_a_filestorage_copies_into_the_server_and_back(storage, tmp_path):
    from ZODB.FileStorage import FileStorage

    source = FileStorage(str(tmp_path / "source.fs"))
    try:
        a, b = source.new_oid(), source.new_oid()
        first = commit(source, [(a, ZERO, b"a1")], extension={"by": "source"})
        commit(source, [(a, first, b"a2"), (b, ZERO, b"b1")])
        expected = transactions(source)

        # restorea: the server takes each transaction under its own tid.
        storage.copyTransactionsFrom(source)
    finally:
        source.close()
    assert transactions(storage) == expected
    assert storage.lastTransaction() == expected[-1][0]

    # The iterator of the server feeds a FileStorage, as zodbconvert does.
    target = FileStorage(str(tmp_path / "target.fs"))
    try:
        target.copyTransactionsFrom(storage)
        assert transactions(target) == expected
    finally:
        target.close()


def test_pack_removes_what_no_reader_can_reach(server):
    import time

    import ZODB
    from persistent.mapping import PersistentMapping
    from ZODB.POSException import POSKeyError

    db = ZODB.DB(client(server))
    try:
        with db.transaction() as conn:
            conn.root()["kept"] = PersistentMapping(n=1)
            conn.root()["dropped"] = PersistentMapping(n=2)
        with db.transaction() as conn:
            dropped = conn.root()["dropped"]._p_oid
            dropped_serial = conn.root()["dropped"]._p_serial
            del conn.root()["dropped"]
            conn.root()["kept"]["n"] = 3
        time.sleep(0.01)
        db.pack(time.time())

        storage = db.storage
        with pytest.raises(POSKeyError):
            storage.loadSerial(dropped, dropped_serial)
        with db.transaction() as conn:
            assert conn.root()["kept"]["n"] == 3
            assert "dropped" not in conn.root()
    finally:
        db.close()
