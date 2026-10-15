"""The ledger of the logical operations of a run, as JSON lines.

Locust writes one record for each logical operation when it ends: what the
operation meant to do and what the site acknowledged. The verification
reads the ledger after the run and checks the site against it. The report
reads it for the work done and the latency of each operation.

An outcome is one of:

- `ok`: the site acknowledged the operation, and the response showed that
  it took effect (a status message, a redirect to the new item, a 204);
- `failed`: the site answered with an error. Zope aborted the transaction
  of the request, so the write did not commit;
- `ambiguous`: no complete answer (a timeout, a lost connection). The write
  may have committed;
- `rejected`: the site answered, but the response did not show the effect
  (a login form instead of the page, a missing status message).

A write also leaves a `pending` record before its request goes out, with
what it will write. When the run ends while the request is in flight, no
final record follows; `read` then gives the pending record as `ambiguous`.
"""

from __future__ import annotations

import json
import os
import threading
from collections.abc import Iterable, Iterator
from dataclasses import asdict, dataclass, field

OK = "ok"
FAILED = "failed"
AMBIGUOUS = "ambiguous"
REJECTED = "rejected"
PENDING = "pending"
OUTCOMES = (OK, FAILED, AMBIGUOUS, REJECTED)


@dataclass
class Record:
    op: str
    kind: str
    user: int
    role: str
    account: str | None
    instance: int
    step: int
    start: float
    end: float
    outcome: str
    # The attempts of the client: more than 1 only with explicit client
    # retries.
    attempts: int = 1
    requests: int = 0
    statuses: list[int] = field(default_factory=list)
    target: str | None = None
    # What the operation wrote or expects, for the verification.
    values: dict = field(default_factory=dict)
    detail: str | None = None

    @property
    def seconds(self) -> float:
        return self.end - self.start


class Ledger:
    """An append-only JSON lines file. One writer process; gevent greenlets
    and threads may share it."""

    def __init__(self, path: str):
        os.makedirs(os.path.dirname(path) or ".", exist_ok=True)
        self._file = open(path, "a", buffering=1)
        self._lock = threading.Lock()

    def write(self, record: Record) -> None:
        line = json.dumps(asdict(record), separators=(",", ":")) + "\n"
        with self._lock:
            self._file.write(line)

    def close(self) -> None:
        self._file.close()


def read(paths: Iterable[str]) -> Iterator[Record]:
    """Yield the final record of each operation, by start time. A pending
    record without a final one becomes `ambiguous`."""
    final: dict[str, Record] = {}
    pending: dict[str, Record] = {}
    for path in paths:
        if not os.path.exists(path):
            continue
        with open(path) as file:
            for line in file:
                line = line.strip()
                if not line:
                    continue
                record = Record(**json.loads(line))
                if record.outcome == PENDING:
                    pending[record.op] = record
                else:
                    final[record.op] = record
    for op, record in pending.items():
        if op not in final:
            record.outcome = AMBIGUOUS
            record.detail = "unfinished at the end of the run"
            final[op] = record
    yield from sorted(final.values(), key=lambda r: r.start)
