"""The storage `floor`: the floor of the phase 3 gate (ADR 0037).

`floor` is a ZODB storage of the harness, in the zodbshootout process,
with no server. Its time in a cell is ZODB, zodbshootout and the least
work of a storage in Python. The gate subtracts it from the time of the
other storages.

- It keeps each record in memory, and `loadBefore` gives back the same
  `bytes` object that `store` got.
- It keeps the current revision of each object and the one before it.
  A load for a snapshot older than both gets None, and ZODB raises
  ReadConflictError.
- The two-phase commit holds no lock across transactions. `store` checks
  the serial. `tpc_vote` claims each oid of the transaction in a dict and
  checks the serial again. `tpc_finish` holds the lock of the store only
  while it takes the next tid and publishes the records.

The databases of one path in one process share one store. A process
starts from the file at that path, when it exists, and reads a record of
the file at its first load. The floor writes the file when the populate
of zodbshootout closes its database. A process of zodbshootout that did
not populate starts from it: the pyperf worker, and each worker process
of the processes mode. In the threads mode, the populate of `add` keeps
the shared database open, and the floor writes no file.
"""

import functools
import os
import pickle
import threading

import zope.interface
from ZODB.interfaces import IStorage
from ZODB.POSException import (
    ConflictError,
    POSKeyError,
    ReadConflictError,
    StorageTransactionError,
)
from ZODB.utils import load_current, newTid, p64, u64, z64

# The file ends with the offset of its index, in 8 bytes.
TRAILER = 8

# The state of each thread: populating is true while the thread runs the
# populate of zodbshootout.
_local = threading.local()


class _Stored:
    """A record of the file that the process has not read yet."""

    __slots__ = ("offset", "length")

    def __init__(self, offset: int, length: int):
        self.offset = offset
        self.length = length


class _Store:
    """The records of one path in one process."""

    def __init__(self, path: str):
        self.path = path
        # oid -> (serial, data, previous). previous is (serial, data) of the
        # revision before, or None. data is bytes, or a _Stored of the file.
        self.records: dict[bytes, tuple] = {}
        # oid -> the pending transaction that claimed it in tpc_vote.
        self.claims: dict[bytes, object] = {}
        self.lock = threading.Lock()
        self.oid_lock = threading.Lock()
        self.ltid = z64
        self.oid = 0
        self.fd = None
        if os.path.exists(path):
            self._read_index()

    def _read_index(self) -> None:
        self.fd = os.open(self.path, os.O_RDONLY)
        size = os.fstat(self.fd).st_size
        offset = u64(os.pread(self.fd, TRAILER, size - TRAILER))
        index = pickle.loads(os.pread(self.fd, size - TRAILER - offset, offset))
        self.ltid = index["ltid"]
        self.oid = index["oid"]
        self.records = {
            oid: (serial, _Stored(start, length), None)
            for oid, (serial, start, length) in index["records"].items()
        }

    def data(self, data) -> bytes:
        """Return the bytes of data: data itself, or its record of the file."""
        if data.__class__ is bytes:
            return data
        return os.pread(self.fd, data.length, data.offset)

    def read(self, oid: bytes, record: tuple) -> bytes:
        """Read the current revision of a record of the file, once."""
        serial, stored, previous = record
        data = self.data(stored)
        with self.lock:
            # A commit can replace the record while the read runs.
            if self.records.get(oid) is record:
                self.records[oid] = (serial, data, previous)
        return data

    def write(self) -> None:
        """Write the current revision of each record to the file."""
        with self.lock:
            records = dict(self.records)
            ltid, oid = self.ltid, self.oid
        partial = f"{self.path}.partial"
        index = {}
        with open(partial, "wb") as out:
            for key, (serial, data, _) in records.items():
                data = self.data(data)
                index[key] = (serial, out.tell(), len(data))
                out.write(data)
            offset = out.tell()
            out.write(pickle.dumps({"ltid": ltid, "oid": oid, "records": index}))
            out.write(p64(offset))
        os.replace(partial, self.path)

    def zap(self) -> None:
        with self.lock:
            self.records = {}
            self.claims.clear()


# The store of each path in this process.
_stores: dict[str, _Store] = {}
_stores_lock = threading.Lock()


def _store(path: str) -> _Store:
    with _stores_lock:
        store = _stores.get(path)
        if store is None:
            store = _stores[path] = _Store(path)
        return store


class _Pending:
    """The state of one transaction between tpc_begin and its end."""

    __slots__ = ("stores", "reads", "claimed")

    def __init__(self):
        # oid -> (serial, data) of each store.
        self.stores: dict[bytes, tuple[bytes, bytes]] = {}
        # oid -> serial of each checkCurrentSerialInTransaction.
        self.reads: dict[bytes, bytes] = {}
        self.claimed: list[bytes] = []


def _pending(transaction, storage) -> _Pending | None:
    try:
        return transaction.data(storage)
    except KeyError:
        return None


@zope.interface.implementer(IStorage)
class FloorStorage:
    """A minimal storage in memory. See the module documentation."""

    def __init__(self, path: str, name: str = "floor"):
        self.__name__ = name
        self._store = _store(os.path.abspath(path))

    # The reads.

    def loadBefore(self, oid, tid):
        record = self._store.records.get(oid)
        if record is None:
            raise POSKeyError(oid)
        serial, data, previous = record
        if serial < tid:
            if data.__class__ is not bytes:
                data = self._store.read(oid, record)
            return data, serial, None
        if previous is not None and previous[0] < tid:
            return self._store.data(previous[1]), previous[0], serial
        return None

    load = load_current

    def loadSerial(self, oid, serial):
        record = self._store.records.get(oid)
        if record is not None:
            for revision in (record[:2], record[2]):
                if revision is not None and revision[0] == serial:
                    return self._store.data(revision[1])
        raise POSKeyError(oid, serial)

    def lastTransaction(self):
        return self._store.ltid

    def __len__(self):
        return len(self._store.records)

    def getSize(self):
        return sum(
            len(data) if data.__class__ is bytes else data.length
            # A copy, so that a commit can add a record during the sum.
            for _, data, _ in self._store.records.copy().values()
        )

    def history(self, oid, size=1):
        record = self._store.records.get(oid)
        if record is None:
            raise POSKeyError(oid)
        revisions = [r for r in (record[:2], record[2]) if r is not None]
        return [
            {"tid": serial, "serial": serial, "size": len(self._store.data(data))}
            for serial, data in revisions[:size]
        ]

    # The two-phase commit.

    def new_oid(self):
        store = self._store
        with store.oid_lock:
            store.oid += 1
            return p64(store.oid)

    def tpc_begin(self, transaction, tid=None, status=" "):
        if tid is not None:
            raise StorageTransactionError("floor chooses the tid of a transaction")
        if _pending(transaction, self) is not None:
            raise StorageTransactionError("duplicate tpc_begin of a transaction")
        transaction.set_data(self, _Pending())

    def _pending(self, transaction) -> _Pending:
        pending = _pending(transaction, self)
        if pending is None:
            raise StorageTransactionError(self, transaction)
        return pending

    def store(self, oid, serial, data, version, transaction):
        if version:
            raise StorageTransactionError("floor has no versions")
        pending = self._pending(transaction)
        self._check(oid, serial, data, ConflictError)
        pending.stores[oid] = (serial, data)

    def checkCurrentSerialInTransaction(self, oid, serial, transaction):
        pending = self._pending(transaction)
        self._check(oid, serial, None, ReadConflictError)
        pending.reads[oid] = serial

    def _check(self, oid, serial, data, error) -> None:
        record = self._store.records.get(oid)
        current = z64 if record is None else record[0]
        if serial != current:
            raise error(oid=oid, serials=(current, serial), data=data)

    def tpc_vote(self, transaction):
        pending = self._pending(transaction)
        checks = [
            (oid, serial, ConflictError) for oid, (serial, _) in pending.stores.items()
        ]
        checks += [
            (oid, serial, ReadConflictError)
            for oid, serial in pending.reads.items()
            if oid not in pending.stores
        ]
        claims = self._store.claims
        try:
            for oid, serial, error in checks:
                # setdefault is atomic, so one transaction at a time claims
                # an oid. Only the claimant can then change its record.
                if claims.setdefault(oid, pending) is not pending:
                    raise error(oid=oid)
                pending.claimed.append(oid)
                self._check(oid, serial, None, error)
        except BaseException:
            self._release(pending)
            raise

    def tpc_finish(self, transaction, func=lambda tid: None):
        pending = self._pending(transaction)
        store = self._store
        try:
            with store.lock:
                tid = newTid(store.ltid)
                records = store.records
                for oid, (_, data) in pending.stores.items():
                    record = records.get(oid)
                    previous = None if record is None else record[:2]
                    records[oid] = (tid, data, previous)
                # The records come first, then the invalidations, then the
                # tid. So a connection whose snapshot holds the tid finds the
                # records, and its invalidations come before its snapshot.
                func(tid)
                store.ltid = tid
        finally:
            self._release(pending)
            transaction.set_data(self, None)
        return tid

    def tpc_abort(self, transaction):
        pending = _pending(transaction, self)
        if pending is not None:
            self._release(pending)
            transaction.set_data(self, None)

    def _release(self, pending: _Pending) -> None:
        claims = self._store.claims
        for oid in pending.claimed:
            if claims.get(oid) is pending:
                del claims[oid]
        pending.claimed.clear()

    # The rest of IStorage, and what zodbshootout calls.

    def getName(self):
        return self.__name__

    def sortKey(self):
        return self._store.path

    def isReadOnly(self):
        return False

    def supportsUndo(self):
        return False

    def registerDB(self, db):
        pass

    def pack(self, t, referencesf):
        pass

    def zap_all(self):
        """Remove every record. zodbshootout calls it before each add."""
        self._store.zap()

    def close(self):
        if getattr(_local, "populating", False):
            self._store.write()


def _hook_populate() -> None:
    """Mark the thread that runs the populate of zodbshootout."""
    from zodbshootout.speedtest import SpeedTestData

    populate = SpeedTestData.populate
    if getattr(populate, "marks_floor", False):
        return

    @functools.wraps(populate)
    def marked(*args, **kwargs):
        _local.populating = True
        try:
            return populate(*args, **kwargs)
        finally:
            _local.populating = False

    marked.marks_floor = True
    SpeedTestData.populate = marked


# ZConfig imports this module in each process of zodbshootout before the
# populate: when it reads the configuration, or when a worker process
# unpickles its database factory.
_hook_populate()


class FloorConfig:
    """The ZConfig factory of the section `<floor>`."""

    def __init__(self, config):
        self.config = config
        self.name = config.getSectionName()

    def open(self):
        return FloorStorage(self.config.path, self.name or "floor")
