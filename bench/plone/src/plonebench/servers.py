"""The counters of the storage servers.

Both servers answer `server_status` with the 13 keys of ZEO 6's server.
zoe serve also has Prometheus metrics on its admin port. The runner reads
both before and after the measured window and keeps the differences. A
counter that a server does not keep stays absent: the report never shows
it as 0.
"""

from __future__ import annotations

import re
import urllib.request

STATUS_COUNTERS = (
    "loads",
    "stores",
    "commits",
    "aborts",
    "conflicts",
    "conflicts_resolved",
)


def server_status(port: int, timeout: float = 30.0) -> dict | None:
    """Return `server_status` of the server, through a stock read-only
    client. The client loads nothing."""
    try:
        from ZEO.ClientStorage import ClientStorage

        storage = ClientStorage(
            ("127.0.0.1", port), wait=True, read_only=True, wait_timeout=timeout
        )
    except Exception as error:  # the server is gone or refuses
        return {"error": f"{type(error).__name__}: {error}"}
    try:
        status = storage.server_status()
        return {
            k: v
            for k, v in status.items()
            if isinstance(v, (int, float, str)) or v is None
        }
    finally:
        storage.close()


_LINE = re.compile(r"^([a-zA-Z_:][a-zA-Z0-9_:]*)(\{[^}]*\})?\s+(\S+)$")


def prometheus(port: int) -> dict[str, float]:
    with urllib.request.urlopen(
        f"http://127.0.0.1:{port}/metrics", timeout=10
    ) as response:
        text = response.read().decode()
    values = {}
    for line in text.splitlines():
        if line.startswith("#") or not line.strip():
            continue
        match = _LINE.match(line)
        if match:
            name, labels, value = match.groups()
            labels = (
                (labels or "").replace('storage="1",', "").replace(',storage="1"', "")
            )
            labels = labels.replace('{storage="1"}', "")
            values[name + labels] = float(value)
    return values


def delta(before: dict | None, after: dict | None) -> dict:
    if not before or not after:
        return {}
    result = {}
    for key, value in after.items():
        if isinstance(value, (int, float)) and isinstance(
            before.get(key), (int, float)
        ):
            result[key] = value - before[key]
    return result


def histogram(metrics: dict[str, float], name: str) -> dict | None:
    """Return the count, the sum and the buckets of a histogram of the
    metrics (already a delta)."""
    buckets = []
    for key, value in metrics.items():
        if key.startswith(name + "_bucket{"):
            le = key.split('le="', 1)[1].split('"', 1)[0]
            buckets.append((float("inf") if le == "+Inf" else float(le), value))
    if not buckets:
        return None
    buckets.sort()
    count = metrics.get(name + "_count", buckets[-1][1])
    total = metrics.get(name + "_sum")
    return {"count": count, "sum": total, "buckets": buckets}


def histogram_quantile(hist: dict | None, q: float) -> float | None:
    """The upper bound of the bucket that holds quantile `q`, as Prometheus
    reads a histogram. None without observations."""
    if not hist or not hist["count"]:
        return None
    target = q * hist["count"]
    for bound, cumulative in hist["buckets"]:
        if cumulative >= target:
            return bound
    return None
