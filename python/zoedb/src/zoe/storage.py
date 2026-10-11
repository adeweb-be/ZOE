"""The in-process storage: `zoe.Storage` implements the ZODB storage interfaces.

The extension class `zoe._zoe.Storage` calls the core. This module adds the
parts that need ZODB: the interface declaration, the history records and the
`WrongThread` exception.
"""

import zope.interface
from ZODB.interfaces import IStorage
from ZODB.POSException import StorageTransactionError
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
class Storage(_zoe.Storage):
    """A ZODB storage in the directory `path`, in this process.

    `Storage(path, *, read_only=False, lock_timeout=None)` opens the store in
    `path` and creates the directory when it is missing. `lock_timeout` is
    the longest time, in seconds, that a vote waits for the lock of an
    object. A longer wait raises `ConflictError`. `None` keeps the default of
    the store. A read-only storage refuses `tpc_begin` and `new_oid` with
    `ReadOnlyError`.

    All the connections of a process share one storage. The thread that
    begins a transaction stores and votes in it; another thread gets
    `WrongThread`. The storage releases the GIL in every call into the
    store.
    """

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
        invalidations of each commit from the `tpc_finish` callback.
        """

    def supportsUndo(self):
        """Return False: the storage does not undo transactions."""
        return False
