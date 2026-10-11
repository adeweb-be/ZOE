"""Acceptance test of iteration, restore and zodbconvert in both directions.

`zoe.Storage` provides `IStorageIteration`, `IStorageCurrentRecordIteration`
and `IBlobStorageRestoreable` in this process:

- `iterator(start, stop)` gives the transactions in tid order, each with its
  metadata and its records;
- `record_iternext` walks the current record of each object;
- `tpc_begin(transaction, tid, status)` begins a restore transaction, and
  `restore` and `restoreBlob` copy a record into it unchanged;
- `history` and `loadSerial` give each revision of an object.

zodbconvert copies a FileStorage with blobs to ZOE, then back to a new
FileStorage. Each copy has the tids, the transaction metadata, the pickles
and the blob bytes of the source, byte for byte.

The conversion test does the steps of zodbconvert itself: it opens the
source and the destination from a ZConfig file with the `<zoe>` section,
checks through the iterator that the destination is empty, and calls
`copyTransactionsFrom` of the destination. The test does not import
zodbconvert. Its package, RelStorage, loads a native library that the
interpreters of the shell do not find.

The corpus is synthetic. ZODB writes it into a FileStorage in each test.

Each test imports ZODB, transaction and persistent itself, so that the
module collects before these packages are installed.
"""

import io
from dataclasses import dataclass
from pathlib import Path

import zoe

# The number of keys in the tree of the corpus. The tree splits into many
# buckets, so the corpus has many objects.
KEYS = 200
# The bytes of the blob of the corpus, in its two revisions.
FIRST = b"first revision\n" * 64
SECOND = bytes(range(256)) * 16

# The schema of zodbconvert, without the import of RelStorage.
SCHEMA = """
<schema>
  <import package="ZODB"/>
  <section type="ZODB.storage" name="source" attribute="source"
    required="yes" />
  <section type="ZODB.storage" name="destination" attribute="destination"
    required="yes" />
</schema>
"""


@dataclass(frozen=True)
class Transaction:
    """A transaction of a storage, as plain values.

    `records` holds one tuple `(oid, tid, data, blob)` per record, in oid
    order. `blob` is the content of the blob file of the record, or None
    when the record is not a blob record.
    """

    tid: bytes
    status: str
    user: bytes
    description: bytes
    extension: dict
    records: tuple


@dataclass(frozen=True)
class Corpus:
    """The FileStorage of the corpus, and its transactions in tid order."""

    path: Path
    blob_dir: Path
    transactions: list


def write_corpus(path, blob_dir):
    """Write the synthetic corpus with ZODB into a FileStorage with blobs."""
    import transaction
    import ZODB
    from BTrees.OOBTree import OOBTree
    from persistent.mapping import PersistentMapping
    from ZODB.blob import Blob
    from ZODB.FileStorage import FileStorage

    db = ZODB.DB(FileStorage(str(path), blob_dir=str(blob_dir)))
    manager = transaction.TransactionManager()
    conn = db.open(manager)

    def commit(user, note, step):
        txn = manager.get()
        txn.user = user
        txn.note(note)
        txn.extension["step"] = step
        manager.commit()

    try:
        root = conn.root()
        root["folder"] = PersistentMapping(title="folder")
        root["index"] = OOBTree()
        commit("editor", "Add the folder and the index", 1)

        root["folder"]["file"] = Blob(FIRST)
        root["folder"]["empty"] = Blob(b"")
        commit("editor", "Add two blobs", 2)

        for n in range(KEYS):
            root["index"][f"key-{n:04d}"] = n
        with root["folder"]["file"].open("w") as file:
            file.write(SECOND)
        commit("loader", "Fill the index and change a blob", 3)

        root["folder"]["raw"] = bytes(range(256))
        root["folder"]["title"] = "folder ✓"
        del root["index"]["key-0000"]
        commit("", "", 4)
    finally:
        conn.close()
        db.close()


def dump(storage):
    """Return the transactions of storage as a list of `Transaction`."""
    from ZODB.blob import is_blob_record

    transactions = []
    iterator = storage.iterator()
    try:
        for txn in iterator:
            records = []
            for record in txn:
                blob = None
                if is_blob_record(record.data):
                    with open(storage.loadBlob(record.oid, record.tid), "rb") as file:
                        blob = file.read()
                records.append((record.oid, record.tid, record.data, blob))
            transactions.append(
                Transaction(
                    tid=txn.tid,
                    status=txn.status,
                    user=txn.user,
                    description=txn.description,
                    extension=dict(txn.extension),
                    records=tuple(sorted(records, key=lambda r: r[0])),
                )
            )
    finally:
        close = getattr(iterator, "close", None)
        if close is not None:
            close()
    return transactions


def corpus_at(directory):
    """Write the corpus into directory and read it back."""
    from ZODB.FileStorage import FileStorage

    directory.mkdir()
    path = directory / "Data.fs"
    blob_dir = directory / "blobs"
    write_corpus(path, blob_dir)
    source = FileStorage(str(path), blob_dir=str(blob_dir), read_only=True)
    try:
        transactions = dump(source)
    finally:
        source.close()
    return Corpus(path, blob_dir, transactions)


def restore_corpus(storage, corpus, scratch):
    """Copy each transaction of corpus into storage with restore.

    Return the tids that `tpc_finish` returns. A blob goes through a new
    file in scratch, which the storage takes.
    """
    from ZODB.Connection import TransactionMetaData
    from ZODB.utils import u64

    finished = []
    for txn in corpus.transactions:
        meta = TransactionMetaData(txn.user, txn.description, txn.extension)
        storage.tpc_begin(meta, txn.tid, txn.status)
        for oid, tid, data, blob in txn.records:
            if blob is None:
                storage.restore(oid, tid, data, "", None, meta)
            else:
                path = scratch / f"{u64(oid)}-{u64(tid)}.blob"
                path.write_bytes(blob)
                storage.restoreBlob(oid, tid, data, str(path), None, meta)
        storage.tpc_vote(meta)
        finished.append(storage.tpc_finish(meta))
    return finished


def restored(tmp_path):
    """Return the corpus and an open `zoe.Storage` that holds a copy of it."""
    corpus = corpus_at(tmp_path / "source")
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    storage = zoe.Storage(str(tmp_path / "store"))
    try:
        restore_corpus(storage, corpus, scratch)
    except BaseException:
        storage.close()
        raise
    return corpus, storage


def has_data(storage):
    """Tell if storage has a transaction, as zodbconvert does."""
    iterator = storage.iterator()
    try:
        try:
            next(iterator)
        except (IndexError, StopIteration):
            return False
        return True
    finally:
        close = getattr(iterator, "close", None)
        if close is not None:
            close()


def zodbconvert(config_path, config):
    """Copy the source storage of config into its destination.

    Do the steps of zodbconvert without options: open both storages from the
    configuration file, refuse a destination that has data, and call
    `copyTransactionsFrom` of the destination.
    """
    import ZConfig

    config_path.write_text(config)
    schema = ZConfig.loadSchemaFile(io.StringIO(SCHEMA))
    with open(config_path) as file:
        loaded, _ = ZConfig.loadConfigFile(schema, file)
    source = loaded.source.open()
    try:
        destination = loaded.destination.open()
        try:
            assert not has_data(destination), "the destination has data"
            destination.copyTransactionsFrom(source)
        finally:
            destination.close()
    finally:
        source.close()


def test_the_storage_provides_iteration_and_restore(tmp_path):
    from ZODB.interfaces import (
        IBlobStorage,
        IBlobStorageRestoreable,
        IStorageCurrentRecordIteration,
        IStorageIteration,
        IStorageRestoreable,
    )
    from zope.interface.verify import verifyObject

    storage = zoe.Storage(str(tmp_path / "store"))
    try:
        for interface in (
            IStorageIteration,
            IStorageCurrentRecordIteration,
            IStorageRestoreable,
            IBlobStorage,
            IBlobStorageRestoreable,
        ):
            assert interface.providedBy(storage), interface.__name__
            assert verifyObject(interface, storage)
    finally:
        storage.close()


def test_restore_copies_each_transaction_under_its_own_tid(tmp_path):
    corpus = corpus_at(tmp_path / "source")
    scratch = tmp_path / "scratch"
    scratch.mkdir()
    tids = [txn.tid for txn in corpus.transactions]
    blobs = sum(
        1 for txn in corpus.transactions for r in txn.records if r[3] is not None
    )
    # The corpus must hold the cases that the copy must keep.
    assert len(tids) == 5
    assert blobs == 3

    storage = zoe.Storage(str(tmp_path / "store"))
    try:
        assert restore_corpus(storage, corpus, scratch) == tids
        assert storage.lastTransaction() == tids[-1]
        assert dump(storage) == corpus.transactions
    finally:
        storage.close()
    # Each file that restoreBlob got now belongs to the storage.
    assert list(scratch.iterdir()) == []

    # The copy survives a new open.
    storage = zoe.Storage(str(tmp_path / "store"), read_only=True)
    try:
        assert dump(storage) == corpus.transactions
    finally:
        storage.close()


def test_the_iterator_gives_the_transactions_from_start_to_stop(tmp_path):
    from ZODB.utils import p64, u64

    corpus, storage = restored(tmp_path)
    try:
        tids = [txn.tid for txn in corpus.transactions]

        def tids_of(*args):
            return [txn.tid for txn in storage.iterator(*args)]

        assert tids_of() == tids
        assert tids_of(tids[1], tids[3]) == tids[1:4]
        assert tids_of(tids[2]) == tids[2:]
        assert tids_of(None, tids[1]) == tids[:2]
        # The bounds need not be tids of the storage.
        assert tids_of(p64(u64(tids[1]) + 1), p64(u64(tids[3]) - 1)) == tids[2:3]
        assert tids_of(p64(u64(tids[-1]) + 1)) == []
        assert tids_of(None, p64(u64(tids[0]) - 1)) == []

        for txn in storage.iterator():
            for record in txn:
                assert record.tid == txn.tid
                assert record.data_txn is None
    finally:
        storage.close()


def test_record_iternext_gives_the_current_record_of_each_object(tmp_path):
    corpus, storage = restored(tmp_path)
    try:
        current = {}
        for txn in corpus.transactions:
            for oid, tid, data, _ in txn.records:
                current[oid] = (tid, data)

        found = {}
        next_oid = None
        while True:
            oid, tid, data, next_oid = storage.record_iternext(next_oid)
            assert oid not in found
            found[oid] = (tid, data)
            if next_oid is None:
                break

        assert found == current
    finally:
        storage.close()


def test_history_and_load_serial_give_each_revision_unchanged(tmp_path):
    from ZODB.FileStorage import FileStorage

    corpus, storage = restored(tmp_path)
    source = FileStorage(
        str(corpus.path), blob_dir=str(corpus.blob_dir), read_only=True
    )
    try:
        revisions = {}
        for txn in corpus.transactions:
            for oid, tid, data, _ in txn.records:
                revisions.setdefault(oid, []).append((tid, data))
        assert max(len(r) for r in revisions.values()) >= 3

        def fields(history):
            return [
                (h["tid"], h["user_name"], h["description"], h["size"]) for h in history
            ]

        for oid, versions in revisions.items():
            for tid, data in versions:
                assert storage.loadSerial(oid, tid) == data
            size = len(versions) + 1
            history = storage.history(oid, size)
            assert [h["tid"] for h in history] == [tid for tid, _ in versions[::-1]]
            assert fields(history) == fields(source.history(oid, size))
    finally:
        source.close()
        storage.close()


def test_zodbconvert_copies_a_file_storage_with_blobs_to_zoe_and_back(tmp_path):
    from ZODB.FileStorage import FileStorage

    corpus = corpus_at(tmp_path / "source")
    store = tmp_path / "zoe"
    back = tmp_path / "back"
    back.mkdir()

    zodbconvert(
        tmp_path / "to-zoe.conf",
        f"""\
%import zoe
<filestorage source>
  path {corpus.path}
  blob-dir {corpus.blob_dir}
</filestorage>
<zoe destination>
  path {store}
  blob-dir {store / "blobs"}
</zoe>
""",
    )
    zodbconvert(
        tmp_path / "from-zoe.conf",
        f"""\
%import zoe
<zoe source>
  path {store}
  blob-dir {store / "blobs"}
</zoe>
<filestorage destination>
  path {back / "Data.fs"}
  blob-dir {back / "blobs"}
</filestorage>
""",
    )

    copy = zoe.Storage(str(store), read_only=True, blob_dir=str(store / "blobs"))
    try:
        assert copy.lastTransaction() == corpus.transactions[-1].tid
        assert dump(copy) == corpus.transactions
    finally:
        copy.close()

    copy_back = FileStorage(
        str(back / "Data.fs"), blob_dir=str(back / "blobs"), read_only=True
    )
    try:
        assert copy_back.lastTransaction() == corpus.transactions[-1].tid
        assert dump(copy_back) == corpus.transactions
    finally:
        copy_back.close()
