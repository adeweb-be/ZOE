"""The in-process storage: `zoe.Storage` implements the ZODB storage interfaces.

The extension class `zoe._zoe.Storage` calls the core. This module adds the
parts that need ZODB: the interface declarations, the history records, the
records of the iterator, the copy from another storage, the conflict
resolution, the undo log, the committed blob files and the `WrongThread`
exception.
"""

import base64
import binascii
import contextlib
import os
import shutil
import tempfile
import time

import zope.interface
from ZODB.BaseStorage import DataRecord, TransactionRecord
from ZODB.blob import BlobFile, is_blob_record
from ZODB.ConflictResolution import ConflictResolvingStorage
from ZODB.Connection import TransactionMetaData
from ZODB.interfaces import (
    IBlobStorageRestoreable,
    IExternalGC,
    IStorageCurrentRecordIteration,
    IStorageIteration,
    IStorageUndoable,
)
from ZODB.POSException import (
    ConflictError,
    POSKeyError,
    StorageTransactionError,
    UndoError,
)
from ZODB.TimeStamp import TimeStamp

from zoe import _zoe


class WrongThread(StorageTransactionError):
    """A thread that did not begin a transaction called a method of it.

    Only the thread that calls `tpc_begin` can call `store`,
    `checkCurrentSerialInTransaction` and `tpc_vote` in the transaction. Any
    thread can call `tpc_finish` and `tpc_abort`. A refused call changes
    nothing.
    """


# The most transactions that one read of the undo log takes.
_UNDO_LOG_CHUNK = 256

# The reason of an undo that a later transaction stops, as in FileStorage.
_CHANGED_LATER = "Some data were modified by a later transaction"


# `IBlobStorageRestoreable` extends `IBlobStorage` and
# `IStorageRestoreable`, and `IStorageCurrentRecordIteration`,
# `IStorageUndoable` and `IExternalGC` extend `IStorage`. `INTERFACES` of
# `zoe serve` lists these interfaces in the order of `__iro__`. A change
# here needs the same change there.
@zope.interface.implementer(
    IBlobStorageRestoreable,
    IStorageCurrentRecordIteration,
    IStorageIteration,
    IStorageUndoable,
    IExternalGC,
)
class Storage(_zoe.Storage, ConflictResolvingStorage):
    """A ZODB storage in the directory `path`, in this process.

    `Storage(path, *, read_only=False, lock_timeout=None, blob_dir=None,
    pack_gc=True, compression=None, structural_merges=None,
    structural_merge_classes=None, posting_dicts=None,
    resolution_log=None)` opens the store in `path` and creates the
    directory when it is missing.
    `lock_timeout` is the longest time, in seconds, that a vote waits for the
    lock of an object. A longer wait raises `ConflictError`. `None` keeps the
    default of the store. A read-only storage refuses `tpc_begin`, `new_oid`,
    `store`, `pack`, `restore`, `storeBlob`, `restoreBlob` and
    `temporaryDirectory` with `ReadOnlyError`, and it does not create a
    missing directory.

    `blob_dir` is the directory of the blob files, in ZODB's BushyLayout;
    `None` is `path/blobs`. The store does not record it, so each open must
    give the same one. A store with blobs refuses a `blob_dir` without the
    layout file of its blobs with `StorageError`: the option is missing or
    wrong, or the directory is not mounted. A `blob_dir` that is not a
    directory raises `OSError`, and the storage creates nothing.

    `structural_merges` is the mode of the structural merges (tier 1) of
    BTree buckets, sets and `Length`: `"on"` (`None` keeps it), `"shadow"`
    or `"off"`. `structural_merge_classes` maps a class, as `module.name`,
    to its own mode. `posting_dicts` is the mode of the posting-dict rule and
    the first-key rule of ADR 0042, off by default. `resolution_log` is a file that gets one JSON
    line for each result of the structural merges. A shadow mode without
    `resolution_log` raises `ValueError`: nothing else would tell what it
    found.

    `storeBlob` takes a file of `temporaryDirectory()` by rename, never by
    copy, and copies a file from elsewhere. `loadBlob` returns the path of a
    read-only file.

    All the connections of a process share one storage. The thread that
    begins a transaction stores and votes in it; another thread gets
    `WrongThread`. The storage releases the GIL in every call into the
    store.

    The vote resolves a conflict in this process, through the
    `_p_resolveConflict` method of the class of the object, as ZODB's
    `ConflictResolvingStorage` does.

    `pack` runs the online pack of the store, with garbage collection.
    Reads and commits go on while it runs. With `pack_gc=False`, a pack
    removes old revisions only, as the `pack-gc` of FileStorage does: it
    keeps the newest record at or before the pack time of each object, also
    when no reader can reach the object. An object whose newest record at or
    before the pack time is a deletion goes. The store does not record
    `pack_gc`, so each open gives it.

    `compression` is the compression of the records that the store writes:
    `"none"`, `"zstd"` or `"zstd:LEVEL"`, such as `"zstd:3"`. `None` keeps
    the default of the store, none: the records are raw. `"zstd"`
    compresses each record with zstd at level 3. A store reads every
    compression, so the setting can change between opens. Another value
    raises `ValueError`.

    `deleteObject` writes a deletion, so the class declares `IExternalGC`.

    `undo` undoes a transaction in the transaction of the call: for each
    object of the undone transaction, it stores the state before it. When a
    later transaction changed an object, the undo is a three-way merge: the
    core merges the classes of tier 1, and `_p_resolveConflict` of the class
    resolves the others in Python, as in FileStorage. An undo that does not
    merge raises `UndoError`, and so does an undo of a transaction at or
    before the last pack. A blob undo gives the blob file of the state
    before, by a hard link: no blob file is copied. `undoLog` and
    `undoInfo` give the transactions after the last pack, newest first. An
    undo id is the base64 text of the tid, as in FileStorage.

    `sequence(name).next()` returns the next value of the sequence `name` of
    the store: an `int`, unique and strictly increasing per store and per
    name. It is not part of a transaction, so an abort does not give the
    value back. Each reopen of the store can skip up to 100 values.

    `iterator` gives the transactions in tid order, and `record_iternext`
    the current record of each object. `tpc_begin(transaction, tid,
    status)` begins a restore transaction, which commits under `tid`, and
    `restore` and `restoreBlob` copy a record into it unchanged.
    `copyTransactionsFrom` copies every transaction of another storage with
    them, as `zodbconvert` asks.

    `cleanup` closes the storage and removes the store directory and the
    blob directory, as `cleanup` of FileStorage removes its files.
    """

    def __init__(self, path, *, blob_dir=None, **options):
        # The extension opens the store. Keep the blob directory for cleanup.
        self._blob_dir = None if blob_dir is None else os.path.abspath(blob_dir)

    def cleanup(self):
        """Close the storage and remove the files of the store.

        The storage removes the store directory and the blob directory. A
        missing directory is not an error. The storage stays closed.
        """
        self.close()
        for directory in (self.getName(), self._blob_dir):
            if directory is not None:
                with contextlib.suppress(FileNotFoundError):
                    shutil.rmtree(directory)

    def tpc_vote(self, transaction):
        """Vote on `transaction`, and resolve its conflicts.

        The vote locks the objects of the transaction. The core merges the
        conflicts of BTree buckets, sets and `Length` in Rust (tier 1), and
        a refusal of tier 1 raises `ConflictError`. Then, for each other
        object that another transaction changed, `tryToResolveConflict`
        runs the `_p_resolveConflict` method of its class. The vote keeps
        the locks while the method runs. It stores the resolved state over
        the committed serial and votes again. Return the oids of the
        resolved objects: those that the core merged, then those that
        Python resolved.

        An object that does not resolve raises `ConflictError`. The error
        names the class of the object. A read-current object is never
        resolved: it raises `ReadConflictError`. A failed first vote
        releases the locks. An error during the resolution or the second
        vote aborts the transaction, and that releases the locks.
        """
        conflicts = self._vote_resolving(transaction, self._crs_untransform_record_data)
        resolved = self._natively_resolved(transaction)
        if not conflicts:
            return resolved
        try:
            for oid, committed, read, committed_data, data in conflicts:
                data = self._resolve(oid, committed, read, data, committed_data)
                self.store(oid, committed, data, "", transaction)
            super().tpc_vote(transaction)
        except BaseException:
            # Release the locks on every error, also on an error that comes
            # after the last store. ZODB aborts the transaction after a
            # failed vote, and a second abort does nothing.
            self.tpc_abort(transaction)
            raise
        return resolved + [conflict[0] for conflict in conflicts]

    def _resolve(self, oid, committed, read, data, committed_data):
        """Return the resolved pickle of `oid`, or raise `ConflictError`.

        `tryToResolveConflict` merges the committed pickle and `data`, the
        pickle of the transaction, from the pickle at the serial `read`. If
        ZODB cannot read the class name from `data`, the error comes without
        it.
        """
        try:
            return self.tryToResolveConflict(oid, committed, read, data, committed_data)
        except ConflictError:
            raise
        except Exception:
            raise ConflictError(oid=oid, serials=(committed, read)) from None

    def openCommittedBlobFile(self, oid, serial, blob=None):
        """Open the blob file of `oid` at `serial` for reading.

        Return a `BlobFile` of `blob` when `blob` is given, otherwise a
        binary file. Raise `POSKeyError` when `serial` wrote no blob for
        `oid`.
        """
        path = self.loadBlob(oid, serial)
        if blob is None:
            return open(path, "rb")
        return BlobFile(path, "r", blob)

    def history(self, oid, size=1):
        """Return at most `size` records of `oid`, newest first."""
        return [
            {
                "time": TimeStamp(tid).timeTime(),
                "tid": tid,
                "serial": tid,
                "user_name": user_name,
                "description": description,
                "size": length,
            }
            for tid, user_name, description, length in self._history(oid, size)
        ]

    def iterator(self, start=None, stop=None):
        """Return an iterator over the transactions from `start` to `stop`.

        Both bounds are included, and `None` leaves a bound open. The
        iterator gives the transactions in tid order, up to the last
        transaction at the time of this call. Each transaction gives its
        records in oid order. A record has the pickle as `data`, or `None`
        for a deletion. Its `data_txn` is `None`, because the store keeps
        every pickle whole. The store keeps no transaction status, so the
        status is always `" "`.

        The iterator holds one transaction and a slice of tids at a time,
        whatever the length of the range.
        """
        return _transactions(self._iterator(start, stop))

    def copyTransactionsFrom(self, other):
        """Copy every transaction of `other` into this storage.

        Each transaction keeps its tid, its metadata and its records,
        byte for byte. Each tid must be after the last tid of this storage.
        A record with a blob gets a copy of the blob file of `other`. A
        blob record whose file `other` does not have is copied without its
        blob, as ZODB's own copy does. An error aborts the transaction that
        it stops, and the transactions before it stay.
        """
        transactions = other.iterator()
        try:
            for txn in transactions:
                self.tpc_begin(txn, txn.tid, txn.status)
                try:
                    for record in txn:
                        self._copy_record(other, record, txn)
                    self.tpc_vote(txn)
                    self.tpc_finish(txn)
                except BaseException:
                    self.tpc_abort(txn)
                    raise
        finally:
            close = getattr(transactions, "close", None)
            if close is not None:
                close()

    def _copy_record(self, other, record, transaction):
        """Restore `record` of the storage `other` into `transaction`.

        The blob file of `other` is copied into the temporary directory of
        this storage, and `restoreBlob` takes the copy by rename. A refused
        call removes the copy.
        """
        blob = None
        if is_blob_record(record.data):
            with contextlib.suppress(POSKeyError):
                blob = other.loadBlob(record.oid, record.tid)
        if blob is None:
            self.restore(
                record.oid, record.tid, record.data, "", record.data_txn, transaction
            )
            return
        fd, name = tempfile.mkstemp(suffix=".tmp", dir=self.temporaryDirectory())
        try:
            with open(fd, "wb") as target, open(blob, "rb") as source:
                shutil.copyfileobj(source, target)
            self.restoreBlob(
                record.oid, record.tid, record.data, name, record.data_txn, transaction
            )
        except BaseException:
            with contextlib.suppress(FileNotFoundError):
                os.remove(name)
            raise

    def pack(self, pack_time, referencesf):
        """Pack the store as of `pack_time`, in seconds since the epoch.

        The pack keeps every record after `pack_time`. For each object that
        a reader at `pack_time` or later can reach, it also keeps the newest
        record at or before `pack_time`. It removes the other records, with
        their blob files. The pack time becomes a tid as in ZODB's own
        storages: the `TimeStamp` of the time in UTC. The store finds the
        references in the records itself, so the pack ignores `referencesf`
        and never calls it.

        With `pack_gc=False`, the pack reads no reference. It keeps the
        newest record at or before `pack_time` of each object, also when no
        reader can reach the object, except a deletion: then the object
        goes.

        The whole pack runs without the GIL. Other threads read and commit
        while it runs. A pack at a time before the last pack does nothing.
        A record that the pack cannot read stops it with
        `StorageSystemError`. A read-only storage raises `ReadOnlyError`,
        and a closed storage raises `StorageError`.
        """
        stop = TimeStamp(*time.gmtime(pack_time)[:5] + (pack_time % 60,))
        self._pack(stop.raw())

    def registerDB(self, wrapper):
        """Accept the database. The storage sends it no invalidations.

        Only this process writes the store, and ZODB gets the
        invalidations of each commit from the `tpc_finish` callback. The
        conflict resolution takes the record transforms of the database.
        """
        super().registerDB(wrapper)

    def supportsUndo(self):
        """Return True: the storage undoes transactions."""
        return True

    def undo(self, transaction_id, transaction):
        """Undo the transaction of the undo id `transaction_id` in
        `transaction`. Return `(None, oids)`: the oids of the undone
        transaction. The commit of `transaction` gives them their state
        before the undone transaction.

        When a later transaction changed an object, the undo merges: tier 1
        of the core merges the classes of BTrees and `Length`, and
        `tryToResolveConflict` the others, with the state of the undone
        transaction as the base, the current state as the committed one,
        and the state before as the new one. An undo that no tier and no
        `_p_resolveConflict` merges raises `UndoError`. An undo id that
        names no transaction after the last pack raises `UndoError`.
        """
        tid = _undo_tid(transaction_id)
        oids, conflicts = self._undo(tid, transaction)
        for oid, serial, current, before in conflicts:
            try:
                data = self.tryToResolveConflict(oid, serial, tid, before, current)
            except ConflictError:
                raise UndoError(_CHANGED_LATER, oid) from None
            self.store(oid, serial, data, "", transaction)
        return None, oids

    def undoLog(self, first=0, last=-20, filter=None):
        """Return the transactions that an undo can undo, newest first,
        from the index `first` up to `last`, `last` excluded. A negative
        `last` gives at most `-last` transactions. `filter` keeps the
        descriptions for which it returns true.

        A description has the keys of FileStorage: `id`, `time`,
        `user_name` and `description`, with the keys of the extension of the
        transaction. The transactions at or before the last pack are not in
        the log.
        """
        if last < 0:
            last = first - last
        found = []
        index = 0
        before = None
        while index < last:
            rows = self._undo_log(before, _UNDO_LOG_CHUNK)
            if not rows:
                break
            for tid, user, description, extension in rows:
                info = {
                    "id": undo_id(tid),
                    "time": TimeStamp(tid).timeTime(),
                    "user_name": user,
                    "description": description,
                }
                info.update(TransactionMetaData(extension=extension).extension)
                if filter is None or filter(info):
                    if index >= first:
                        found.append(info)
                    index += 1
                    if index >= last:
                        break
            before = rows[-1][0]
        return found

    def undoInfo(self, first=0, last=-20, specification=None):
        """Return the transactions of `undoLog(first, last)` whose
        description holds each key of `specification` with its value."""
        if not specification:
            return self.undoLog(first, last)
        items = list(specification.items())

        def matches(info):
            return all(info.get(key) == value for key, value in items)

        return self.undoLog(first, last, matches)


def undo_id(tid):
    """Return the undo id of the transaction `tid`: the base64 text of its
    8 bytes, as FileStorage gives it."""
    return base64.encodebytes(tid).rstrip()


def _undo_tid(transaction_id):
    """Return the tid of the undo id `transaction_id`: the text of
    `undo_id`, or the 8 bytes of the tid. Another id raises `UndoError`."""
    if isinstance(transaction_id, str):
        transaction_id = transaction_id.encode("ascii", "replace")
    if len(transaction_id) == 8:
        return transaction_id
    try:
        tid = base64.b64decode(transaction_id.strip(), validate=True)
    except (binascii.Error, ValueError):
        tid = b""
    if len(tid) != 8:
        raise UndoError("the undo id names no transaction")
    return tid


class _Transaction(TransactionRecord):
    """A transaction that `Storage.iterator` gives.

    Each iteration gives its records as ZODB's `DataRecord`, with `None` as
    `data_txn`.
    """

    def __init__(self, tid, user, description, extension, records):
        super().__init__(tid, " ", user, description, extension)
        self._records = records

    def __iter__(self):
        for oid, tid, data in self._records:
            yield DataRecord(oid, tid, data, None)


def _transactions(rows):
    """Give a `_Transaction` for each row of `Storage._iterator`."""
    for tid, user, description, extension, records in rows:
        yield _Transaction(tid, user, description, extension, records)
