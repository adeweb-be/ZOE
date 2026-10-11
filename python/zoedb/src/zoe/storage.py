"""The in-process storage: `zoe.Storage` implements the ZODB storage interfaces.

The extension class `zoe._zoe.Storage` calls the core. This module adds the
parts that need ZODB: the interface declarations, the history records, the
conflict resolution, the committed blob files and the `WrongThread`
exception.
"""

import time

import zope.interface
from ZODB.blob import BlobFile
from ZODB.ConflictResolution import ConflictResolvingStorage
from ZODB.interfaces import IBlobStorage, IStorage
from ZODB.POSException import ConflictError, StorageTransactionError
from ZODB.TimeStamp import TimeStamp

from zoe import _zoe


class WrongThread(StorageTransactionError):
    """A thread that did not begin a transaction called a method of it.

    Only the thread that calls `tpc_begin` can call `store`,
    `checkCurrentSerialInTransaction` and `tpc_vote` in the transaction. Any
    thread can call `tpc_finish` and `tpc_abort`. A refused call changes
    nothing.
    """


@zope.interface.implementer(IStorage, IBlobStorage)
class Storage(_zoe.Storage, ConflictResolvingStorage):
    """A ZODB storage in the directory `path`, in this process.

    `Storage(path, *, read_only=False, lock_timeout=None, blob_dir=None)`
    opens the store in `path` and creates the directory when it is missing.
    `lock_timeout` is the longest time, in seconds, that a vote waits for the
    lock of an object. A longer wait raises `ConflictError`. `None` keeps the
    default of the store. A read-only storage refuses `tpc_begin`, `new_oid`,
    `store`, `pack`, `storeBlob` and `temporaryDirectory` with
    `ReadOnlyError`, and it does not create a missing directory.

    `blob_dir` is the directory of the blob files, in ZODB's BushyLayout.
    The store keeps them in `path/blobs`. Another `blob_dir` becomes the
    symbolic link `path/blobs` to it, and the link stays, so a later open
    without `blob_dir` finds the blobs. A store that already has a blob
    directory refuses another one with `StorageError`, and so does a new
    link to a `blob_dir` that is not empty.

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
    Reads and commits go on while it runs.
    """

    def tpc_vote(self, transaction):
        """Vote on `transaction`, and resolve its conflicts.

        The vote locks the objects of the transaction. Then, for each object
        that another transaction changed, `tryToResolveConflict` runs the
        `_p_resolveConflict` method of its class. The vote keeps the locks
        while the method runs. It stores the resolved state over the
        committed serial and votes again. Return the oids of the resolved
        objects.

        An object that does not resolve raises `ConflictError`. The error
        names the class of the object. A read-current object is never
        resolved: it raises `ReadConflictError`. A failed first vote
        releases the locks. An error during the resolution or the second
        vote aborts the transaction, and that releases the locks.
        """
        conflicts = self._vote_resolving(transaction, self._crs_untransform_record_data)
        if not conflicts:
            return []
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
        return [conflict[0] for conflict in conflicts]

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

    def pack(self, pack_time, referencesf):
        """Pack the store as of `pack_time`, in seconds since the epoch.

        The pack keeps every record after `pack_time`. For each object that
        a reader at `pack_time` or later can reach, it also keeps the newest
        record at or before `pack_time`. It removes the other records, with
        their blob files. The pack time becomes a tid as in ZODB's own
        storages: the `TimeStamp` of the time in UTC. The store finds the
        references in the records itself, so the pack ignores `referencesf`
        and never calls it.

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
        """Return False: the storage does not undo transactions."""
        return False
