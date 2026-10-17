"""The runs of a replay.

A run starts a new server of its stack on a new store, copies the base of
the log into it, then replays the timed transactions on their schedule. A
transaction reads the serials and the states of its oids at its start, and
stores its records at its commit, so a faster replay overlaps more
transactions. One thread plays the events in time order through one
client, so the order of the events, and so the counts, do not depend on
the machine. A second client runs a read load beside it.

The log holds each record as the store committed it. So the record of the
second of two overlapping writes already holds the change of the first,
and stored as it is, it would conflict with that change. A transaction
stores its own change instead: the change from the record of the oid
before it in the log to its own record, applied to the state that it read
(`rebase`). This is not a conflict, and no count holds it.
"""

import random
import threading
import time
from io import BytesIO
from pathlib import Path

from ZODB.Connection import TransactionMetaData
from ZODB.POSException import ConflictError, POSKeyError
from ZODB.utils import get_pickle_metadata, z64

from replay import servers
from replay.log import Log, Record, Transaction

# The stacks: ZEO.ClientStorage to a ZEO 6 server on a FileStorage, and the
# native client to zoe serve.
STACKS = ("zeo", "zoe-native")
# The speeds of the replay, as multiples of the speed of the log.
SPEEDS = (1, 4, 16)
# What ended a conflict: zoe serve resolved it, the resolver of ZODB in the
# process of the replay resolved it, the resolver of ZODB in the ZEO server
# resolved it, or nothing resolved it and the commit raised ConflictError.
PATHS = ("native", "python-client", "python-server", "refused")
# The time between two loads of the read load, in seconds.
READ_PAUSE = 0.002


def check(storage, acknowledged) -> list[bytes]:
    """Return the tid of each acknowledged transaction of which a record
    does not load with `loadSerial`, in the order of `acknowledged`."""
    missing = []
    for tid, oids in acknowledged:
        for oid in oids:
            try:
                storage.loadSerial(oid, tid)
            except (POSKeyError, KeyError):
                missing.append(tid)
                break
    return missing


def class_of(data: bytes | None) -> str:
    """Return the class of a record as `module.name`."""
    if not data:
        return "<deletion>"
    try:
        return ".".join(get_pickle_metadata(data))
    except Exception:
        return "<unknown>"


def rebase(record: Record, read: bytes | None) -> bytes:
    """Return the record that a transaction stores: its own change, applied
    to the state `read` that it read.

    When the state that it read is the record before it in the log, or the
    oid has no record before it, it is its own record. Else the
    `_p_resolveConflict` of the class applies the change, with the record
    before it as the old state, the state that it read as the committed
    state and its own record as the new state. When that refuses, and the
    class is a bucket, a set or a BTree of one bucket of BTrees, the items
    that the transaction added, changed or deleted go into the items that
    it read (`apply_items`). When that fails too, it is its own record.
    """
    if record.previous is None or read is None or read == record.previous:
        return record.data
    try:
        return _resolve(record.previous, read, record.data)
    except Exception:
        return record.data


def _resolve(old: bytes, committed: bytes, new: bytes) -> bytes:
    """Run the resolver of the class of `new` on three records, as
    `tryToResolveConflict` of ZODB does, and return the resolved record."""
    from ZODB._compat import PersistentPickler, PersistentUnpickler, _protocol
    from ZODB.ConflictResolution import (
        PersistentReferenceFactory,
        find_global,
        persistent_id,
    )

    factory = PersistentReferenceFactory()
    unpickler = PersistentUnpickler(find_global, factory.persistent_load, BytesIO(new))
    meta = unpickler.load()
    if isinstance(meta, tuple):
        klass, newargs = meta[0], meta[1] or ()
        if isinstance(klass, tuple):
            klass = find_global(*klass)
    else:
        klass, newargs = meta, ()
    resolve = klass.__new__(klass, *newargs)._p_resolveConflict
    new_state = unpickler.load()

    def state(data):
        other = PersistentUnpickler(find_global, factory.persistent_load, BytesIO(data))
        other.load()
        return other.load()

    states = state(old), state(committed), new_state
    try:
        resolved = resolve(*states)
    except Exception:
        resolved = apply_items(klass, *states)
    out = BytesIO()
    pickler = PersistentPickler(persistent_id, out, _protocol)
    pickler.dump(meta)
    pickler.dump(resolved)
    return out.getvalue()


def apply_items(klass, old, read, own):
    """Return the state `read` of a bucket, a set or a BTree of one bucket
    of BTrees, with the items that `own` added, changed or deleted against
    `old`. Raise `ValueError` for another class or another shape."""
    name = klass.__name__
    if not klass.__module__.startswith("BTrees.") or not name.endswith(
        ("Bucket", "BTree", "Set")
    ):
        raise ValueError(f"no items to apply in {name}")
    mapping = name.endswith(("Bucket", "BTree"))
    tree = name.endswith(("BTree", "TreeSet"))

    def unwrap(state):
        """Return the flat items and the next bucket of a state."""
        if state is None:
            return (), None
        if tree:
            # A BTree of one bucket holds the state of the bucket inline.
            if len(state) != 1 or len(state[0]) != 1:
                raise ValueError("a BTree of more than one bucket")
            state = state[0][0]
        return state[0], state[1] if len(state) > 1 else None

    def items(flat):
        if mapping:
            return dict(zip(flat[::2], flat[1::2], strict=True))
        return dict.fromkeys(flat)

    old_items, _ = unwrap(old)
    read_flat, read_next = unwrap(read)
    own_items, _ = unwrap(own)
    before, merged, after = items(old_items), items(read_flat), items(own_items)
    for key in before.keys() - after.keys():
        merged.pop(key, None)
    for key, value in after.items():
        if key not in before or before[key] != value:
            merged[key] = value
    flat = []
    for key in sorted(merged):
        flat.append(key)
        if mapping:
            flat.append(merged[key])
    state = (tuple(flat),) if read_next is None else (tuple(flat), read_next)
    return ((state,),) if tree else state


def copy_base(storage, base: list[Transaction]) -> None:
    """Restore the transactions of the base into the new store."""
    for txn in base:
        meta = TransactionMetaData(txn.user, txn.description)
        storage.tpc_begin(meta, txn.tid, " ")
        for record in txn.records:
            storage.restore(record.oid, txn.tid, record.data, "", None, meta)
        storage.tpc_vote(meta)
        storage.tpc_finish(meta)


class _ReadLoad:
    """Loads the oids of the log on a client of its own while a run plays."""

    def __init__(self, storage, oids: list[bytes]):
        self.storage = storage
        self.oids = oids
        self.stop = threading.Event()
        self.loads = 0
        self.thread = threading.Thread(target=self._run, daemon=True, name="read load")

    def _run(self):
        rng = random.Random(0)
        while self.oids and not self.stop.is_set():
            try:
                self.storage.load(rng.choice(self.oids))
                self.loads += 1
            except (POSKeyError, KeyError):
                pass
            self.stop.wait(READ_PAUSE)

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *exc_info):
        self.stop.set()
        self.thread.join()


def _schedule(log: Log, speed: float) -> list[tuple[float, int, int]]:
    """Return the events of the log at `speed`: the time after the first
    start, the kind (0 for the start of a transaction, 1 for its commit) and
    the index of the transaction, in time order."""
    if not log.transactions:
        return []
    first = min(txn.start for txn in log.transactions)
    events = []
    for index, txn in enumerate(log.transactions):
        events.append(((txn.start - first) / speed, 0, index))
        events.append(((txn.commit - first) / speed, 1, index))
    events.sort()
    return events


def _load(storage, oid: bytes) -> tuple[bytes, bytes | None]:
    """Return the current serial and record of `oid`, or the zero serial
    and None for an oid that the store does not hold."""
    try:
        data, serial = storage.load(oid)
    except (POSKeyError, KeyError):
        return z64, None
    return serial, data


def _play(storage, log: Log, speed: float, run: dict) -> list:
    """Play the timed transactions of `log` at `speed` on `storage`. Fill
    the counts of `run`. Return the acknowledged transactions."""
    calls: list[bytes] = []
    resolve = storage.tryToResolveConflict

    def counting(oid, *args, **kwargs):
        calls.append(oid)
        return resolve(oid, *args, **kwargs)

    # The clients look the method up at each vote.
    storage.tryToResolveConflict = counting
    server_path = "python-server" if run["stack"] == "zeo" else "native"
    reads: dict[int, dict[bytes, tuple[bytes, bytes | None]]] = {}
    acknowledged = []
    conflicts: dict[str, dict[str, int]] = {}

    def count(record_class, path):
        paths = conflicts.setdefault(record_class, {})
        paths[path] = paths.get(path, 0) + 1

    events = _schedule(log, speed)
    started = time.monotonic()
    ended = started
    for at, kind, index in events:
        delay = started + at - time.monotonic()
        if delay > 0:
            time.sleep(delay)
        txn = log.transactions[index]
        if kind == 0:
            reads[index] = {oid: _load(storage, oid) for oid in txn.oids}
            continue
        read = reads.pop(index)
        meta = TransactionMetaData(txn.user, txn.description)
        classes = {}
        storage.tpc_begin(meta)
        for record in txn.records:
            serial, state = read[record.oid]
            if record.data is None:
                storage.deleteObject(record.oid, serial, meta)
                continue
            data = rebase(record, state)
            classes[record.oid] = class_of(data)
            storage.store(record.oid, serial, data, "", meta)
        del calls[:]
        try:
            resolved = storage.tpc_vote(meta) or []
            tid = storage.tpc_finish(meta)
        except ConflictError as error:
            storage.tpc_abort(meta)
            oid = getattr(error, "oid", None)
            count(classes.get(oid, "<unknown>"), "refused")
            run["conflict_errors"] += 1
        else:
            for oid in resolved:
                path = "python-client" if oid in calls else server_path
                count(classes.get(oid, "<unknown>"), path)
            acknowledged.append((tid, txn.oids))
        run["order"].append(txn.tid.hex())
        ended = time.monotonic()
    storage.tryToResolveConflict = resolve
    run["seconds"] = ended - started
    run["conflicts"] = conflicts
    return acknowledged


def run_one(
    log: Log, stack: str, speed: float, repetition: int, directory: Path, check
):
    """One run of `stack` at `speed`. Return its item of the result."""
    directory.mkdir(parents=True, exist_ok=True)
    item = {
        "stack": stack,
        "speed": speed,
        "repetition": repetition,
        "transactions": len(log.transactions),
        "order": [],
        "conflicts": {},
        "conflict_errors": 0,
    }
    oids = sorted({oid for txn in log.transactions for oid in txn.oids})
    with servers.server(stack, directory) as address:
        storage = servers.client(stack, address)
        try:
            copy_base(storage, log.base)
            reader = servers.client(stack, address)
            try:
                with _ReadLoad(reader, oids):
                    acknowledged = _play(storage, log, speed, item)
            finally:
                reader.close()
        finally:
            storage.close()
        checker = servers.client(stack, address)
        try:
            missing = check(checker, acknowledged)
        finally:
            checker.close()
    item["check"] = {
        "acknowledged": len(acknowledged),
        "missing": [tid.hex() for tid in missing],
    }
    item["counts"] = not missing
    return item


def run(log: Log, *, workdir, stacks=STACKS, speeds=SPEEDS, repetitions=1, check=check):
    """Replay `log` on each stack, at each speed, `repetitions` times.

    The runs alternate: for each speed, for each repetition, one run of each
    stack, in the order of `stacks`. Each run has a new server on a new
    store under `workdir`. Return the result.
    """
    workdir = Path(workdir)
    runs = []
    for speed in speeds:
        for repetition in range(1, repetitions + 1):
            for stack in stacks:
                directory = workdir / f"{stack}-{speed}x-{repetition}"
                runs.append(run_one(log, stack, speed, repetition, directory, check))
    return {"schema_version": 1, "speeds": list(speeds), "runs": runs}
