"""The numbers of a run: work, latency, attempts and conflicts.

The unit of work is a logical operation of the ledger. The probe records
of the Zope processes join it through the op id that Locust sends in
`X-Bench-Op`. Every function here is pure.
"""

from __future__ import annotations

import json
import math
from collections import Counter, defaultdict
from pathlib import Path

from plonebench.ledger import AMBIGUOUS, FAILED, OK, REJECTED, Record

# Below this many operations, a p99 says little: the report marks it.
P99_MIN = 200
WRITE_KINDS = (
    "edit",
    "create",
    "workflow",
    "patch-title",
    "patch-description",
    "import",
    "reindex",
)


def percentile(values: list[float], q: float) -> float | None:
    """The q-quantile (0..1) with linear interpolation, or None."""
    if not values:
        return None
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    position = q * (len(ordered) - 1)
    low = math.floor(position)
    high = math.ceil(position)
    if low == high:
        return ordered[low]
    return ordered[low] + (ordered[high] - ordered[low]) * (position - low)


def latency(values: list[float]) -> dict:
    return {
        "count": len(values),
        "mean": sum(values) / len(values) if values else None,
        "p50": percentile(values, 0.50),
        "p95": percentile(values, 0.95),
        "p99": percentile(values, 0.99),
        "max": max(values) if values else None,
        "p99_reliable": len(values) >= P99_MIN,
    }


def load_probe(directory: Path) -> tuple[list[dict], list[dict], list[dict]]:
    """Return the request records, the snapshots and the ZEO server
    records of the probe files of a run."""
    requests, snapshots, server = [], [], []
    for path in sorted(directory.glob("*.jsonl")):
        with open(path) as file:
            for line in file:
                if not line.strip():
                    continue
                record = json.loads(line)
                record["file"] = path.stem
                kind = record.get("kind")
                if kind == "request":
                    requests.append(record)
                elif kind == "snapshot":
                    snapshots.append(record)
                elif kind == "zeoserver":
                    server.append(record)
    return requests, snapshots, server


def probe_by_op(requests: list[dict]) -> dict[str, list[dict]]:
    by_op = defaultdict(list)
    for record in requests:
        if record.get("op"):
            by_op[record["op"]].append(record)
    return by_op


def server_totals(records: list[dict]) -> dict:
    """Add up the probe records of a set of requests."""
    totals = Counter()
    conflicts = Counter()
    reasons = Counter()
    for r in records:
        totals["requests"] += 1
        totals["attempts"] += r["attempts"]
        totals["retries"] += r["retries"]
        for key in (
            "loads",
            "load_bytes",
            "stores",
            "store_bytes",
            "votes",
            "finishes",
            "aborts",
            "resolved",
            "resolve_failed",
        ):
            totals[key] += r.get(key, 0)
        for key in (
            "seconds",
            "cpu",
            "commit_seconds",
            "load_seconds",
            "vote_seconds",
            "finish_seconds",
            "resolve_seconds",
        ):
            totals[key] += r.get(key, 0.0)
        reasons.update(r.get("resolve_reasons", ()))
        for error in r["errors"]:
            totals["resolved_in_failed_attempts"] += error.get("resolved", 0)
            if "conflict" in error:
                totals["conflicts_" + error["conflict"]] += 1
                conflicts[error.get("class") or "<read conflict>"] += 1
            else:
                totals["errors_other"] += 1
            if not error["retry"]:
                totals["exhausted" if "conflict" in error else "final_errors"] += 1
        if r["attempts"] > 1:
            totals["retried_requests"] += 1
            # The time of a retried request beyond its last attempt is not
            # known; its whole time counts as time spent with retries.
            totals["retried_seconds"] += r["seconds"]
    result = dict(totals)
    result["conflict_classes"] = dict(conflicts.most_common())
    result["resolve_reasons"] = dict(reasons.most_common())
    return result


def op_summary(
    records: list[Record], probes: dict[str, list[dict]], seconds: float
) -> dict:
    """Return the numbers of a set of operations of one kind."""
    outcomes = Counter(r.outcome for r in records)
    ok = [r for r in records if r.outcome == OK]
    first = [
        r
        for r in ok
        if r.attempts == 1 and all(p["attempts"] == 1 for p in probes.get(r.op, []))
    ]
    server = server_totals([p for r in records for p in probes.get(r.op, [])])
    count = len(records)
    return {
        "operations": count,
        "outcomes": {k: outcomes.get(k, 0) for k in (OK, FAILED, AMBIGUOUS, REJECTED)},
        "ok_per_second": len(ok) / seconds if seconds else None,
        "failure_rate": (count - len(ok)) / count if count else None,
        "first_attempt_ok": len(first),
        "eventual_ok": len(ok),
        "client_attempts": sum(r.attempts for r in records),
        "requests_per_op": sum(r.requests for r in records) / count if count else None,
        "server_attempts_per_op": server.get("attempts", 0) / count if count else None,
        "latency_ok": latency([r.seconds for r in ok]),
        "latency_all": latency([r.seconds for r in records]),
        "server": server,
    }


def step_windows(
    started: float, steps: list[tuple[int, float]]
) -> list[tuple[float, float]]:
    windows = []
    t = started
    for _, seconds in steps:
        windows.append((t, t + seconds))
        t += seconds
    return windows


def summarize_steps(
    records: list[Record],
    probes: dict[str, list[dict]],
    steps: list[tuple[int, float]],
    windows: list[tuple[float, float]],
    trim: float = 0.0,
) -> list[dict]:
    """Return the numbers of each step. An operation belongs to the step it
    started in. With `trim`, the operations of the first `trim` seconds of
    each step stay out, so the ramp of the users does not count."""
    result = []
    for index, ((users, seconds), (start, end)) in enumerate(
        zip(steps, windows, strict=True)
    ):
        chosen = [
            r
            for r in records
            if r.step == index and r.start >= start + trim and r.kind != "login"
        ]
        span = seconds - trim
        kinds = defaultdict(list)
        for r in chosen:
            kinds[r.kind].append(r)
        writes = [r for r in chosen if r.kind in WRITE_KINDS]
        result.append(
            {
                "step": index,
                "users": users,
                "seconds": span,
                "window": [start + trim, end],
                "all": op_summary(chosen, probes, span),
                "writes": op_summary(writes, probes, span),
                "kinds": {
                    k: op_summary(v, probes, span) for k, v in sorted(kinds.items())
                },
                "zope_busy": zope_busy(
                    [p for r in chosen for p in probes.get(r.op, [])], span
                ),
            }
        )
    return result


def zope_busy(requests: list[dict], seconds: float) -> dict:
    """The mean number of requests in Zope at once, by Little's law: the
    time of the requests over the window."""
    per_file = defaultdict(float)
    for r in requests:
        per_file[r["file"]] += r["seconds"]
    return (
        {name: total / seconds for name, total in per_file.items()} if seconds else {}
    )
