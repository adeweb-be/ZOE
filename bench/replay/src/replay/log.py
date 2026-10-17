"""The log of a replay: the transactions of a store, with their timing.

The commit of a transaction is the time of its tid. Its start comes from
outside the store: the probe records of a run of B5 give it (see
`starts_of_probes`).
"""

import dataclasses
import json
from pathlib import Path

from ZODB.FileStorage import FileStorage
from ZODB.TimeStamp import TimeStamp


@dataclasses.dataclass(frozen=True)
class Record:
    """A record of a transaction of the log."""

    oid: bytes
    data: bytes | None
    # The record of the oid before this one in the log, or None.
    previous: bytes | None


@dataclasses.dataclass(frozen=True)
class Transaction:
    """A transaction of the log."""

    tid: bytes
    # The start, in seconds since the epoch. None in the base.
    start: float | None
    # The commit: the time of the tid, in seconds since the epoch.
    commit: float
    records: tuple[Record, ...]
    user: bytes = b""
    description: bytes = b""

    @property
    def oids(self) -> list[bytes]:
        return [record.oid for record in self.records]


@dataclasses.dataclass(frozen=True)
class Log:
    """The base, which each run copies first, and the timed transactions."""

    base: list[Transaction]
    transactions: list[Transaction]


def read_log(store, starts: dict[bytes, float]) -> Log:
    """Read the FileStorage `store` through its iterator, in tid order.

    `starts` maps a tid to the start of its transaction. The transactions
    before the first one with a start are the base. A transaction without a
    start after it raises `ValueError`.
    """
    storage = FileStorage(str(store), read_only=True)
    try:
        last: dict[bytes, bytes | None] = {}
        base: list[Transaction] = []
        timed: list[Transaction] = []
        for txn in storage.iterator():
            records = []
            for record in txn:
                records.append(Record(record.oid, record.data, last.get(record.oid)))
                last[record.oid] = record.data
            start = starts.get(txn.tid)
            if start is None and timed:
                raise ValueError(
                    f"the transaction {txn.tid.hex()} has no start, after the first "
                    "timed transaction"
                )
            item = Transaction(
                tid=txn.tid,
                start=start,
                commit=TimeStamp(txn.tid).timeTime(),
                records=tuple(records),
                user=txn.user,
                description=txn.description,
            )
            (timed if start is not None else base).append(item)
        return Log(base, timed)
    finally:
        storage.close()


def starts_of_probes(store, probes: list[Path]) -> tuple[dict[bytes, float], dict]:
    """Return the start of each transaction of `store` from the probe
    records of a run of B5, and a report of the match.

    A probe record of a request gives its wall-clock start `t`, its time
    `seconds` and its `finishes`. Each `tpc_finish` of the request ends one
    commit before the request ends. So a tid belongs to the request that
    committed, that started before the time of the tid, and whose end comes
    first after it. A request of several attempts started its last attempt
    after `t`: the start of the transaction is then the end of the request
    minus the time of one attempt. A tid that no request explains starts at
    its commit: it reads its serials when it commits, so it overlaps
    nothing.
    """
    requests = []
    for path in probes:
        for line in Path(path).read_text().splitlines():
            record = json.loads(line)
            if record.get("kind") == "request" and record.get("finishes"):
                end = record["t"] + record["seconds"]
                attempt = record["seconds"] / max(1, record.get("attempts") or 1)
                requests.append([end, end - attempt, record["finishes"]])
    requests.sort()
    storage = FileStorage(str(store), read_only=True)
    try:
        tids = [txn.tid for txn in storage.iterator()]
    finally:
        storage.close()
    first = requests[0][1] if requests else None
    starts: dict[bytes, float] = {}
    matched = unmatched = 0
    index = 0
    for tid in tids:
        commit = TimeStamp(tid).timeTime()
        if first is None or commit < first:
            continue
        while index < len(requests) and requests[index][0] < commit:
            index += 1
        found = None
        for candidate in requests[index:]:
            if candidate[2] > 0 and candidate[1] <= commit:
                found = candidate
                break
        if found is None:
            starts[tid] = commit
            unmatched += 1
            continue
        found[2] -= 1
        starts[tid] = found[1]
        matched += 1
    return starts, {
        "matched": matched,
        "unmatched": unmatched,
        "requests": len(requests),
    }
