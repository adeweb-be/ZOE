"""The in-process storage: `zoe.Storage` implements the ZODB storage interfaces.

The extension class `zoe._zoe.Storage` calls the core. This module adds the
parts that need ZODB: the interface declaration, the history records, the
conflict resolution and the `WrongThread` exception.
"""

import contextlib

import zope.interface
from ZODB.ConflictResolution import ConflictResolvingStorage
from ZODB.interfaces import IStorage
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


@zope.interface.implementer(IStorage)
class Storage(_zoe.Storage, ConflictResolvingStorage):
    """A ZODB storage in the directory `path`, in this process.

    `Storage(path, *, read_only=False, lock_timeout=None)` opens the store in
    `path` and creates the directory when it is missing. `lock_timeout` is
    the longest time, in seconds, that a vote waits for the lock of an
    object. A longer wait raises `ConflictError`. `None` keeps the default of
    the store. A read-only storage refuses `tpc_begin`, `new_oid` and `store`
    with `ReadOnlyError`, and it does not create a missing directory.

    All the connections of a process share one storage. The thread that
    begins a transaction stores and votes in it; another thread gets
    `WrongThread`. The storage releases the GIL in every call into the
    store.

    The vote resolves a conflict in this process, through the
    `_p_resolveConflict` method of the class of the object, as ZODB's
    `ConflictResolvingStorage` does.
    """

    def tpc_vote(self, transaction):
        """Vote on `transaction`, and resolve its conflicts.

        The vote locks the objects of the transaction. Then, for each object
        that another transaction changed, `tryToResolveConflict` runs the
        `_p_resolveConflict` method of its class. The vote keeps the locks
        while the method runs. It stores the resolved state over the
        committed serial and votes again. Return the oids of the resolved
        objects.

        An object that does not resolve raises `ConflictError`, and the vote
        releases the locks. The error names the class of the object. A
        read-current object is never resolved: it raises `ReadConflictError`.
        """
        conflicts = self._vote_resolving(transaction, self._crs_untransform_record_data)
        if not conflicts:
            return []
        try:
            for oid, committed, read, committed_data, data in conflicts:
                try:
                    data = self.tryToResolveConflict(
                        oid, committed, read, data, committed_data
                    )
                except ConflictError:
                    raise
                except Exception:
                    # ZODB cannot read the class name from the pickle, so
                    # the error comes without it.
                    raise ConflictError(oid=oid, serials=(committed, read)) from None
                self.store(oid, committed, data, "", transaction)
        except BaseException:
            # An oid that did not resolve is still stale. So the second vote
            # fails, and it releases the locks.
            with contextlib.suppress(Exception):
                super().tpc_vote(transaction)
            raise
        super().tpc_vote(transaction)
        return [conflict[0] for conflict in conflicts]

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
        """Refuse to pack: the store does not pack yet."""
        raise NotImplementedError("zoe.Storage does not pack yet")

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
