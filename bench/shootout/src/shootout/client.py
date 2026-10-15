"""The client benchmark: `ZEO.ClientStorage` against the native client.

`run` measures both clients against each server and returns one result
document. The document validates against `bench/schema/client.schema.json`.
`report` prints one row per shape, workload, record size and server, and
gives the verdict.

Both clients run through the storage API, without ZODB: `loadBefore` for
the loads, and `new_oid`, `tpc_begin`, `store`, `tpc_vote` and `tpc_finish`
for the commits. So the numbers are the numbers of the client, not of ZODB.

Each round of a row runs in its own Python process, one per client. The
rounds of both clients interleave, and the first client changes from one
round to the next. Both clients run the same calls in the same order on
the same server state:

- The loads run on one store per server and record size. The harness
  fills it before the first round and changes it no more. The cold round
  of each client starts with no cache file and writes one. The warm round
  of the same client then reopens that file.
- Each commit round runs on a fresh store.
"""

import datetime
import json
import shutil
import statistics
import subprocess
import sys
import tempfile
from collections import defaultdict
from collections.abc import Iterator
from contextlib import contextmanager
from pathlib import Path

from shootout import (
    KB,
    RECORD_OVERHEAD,
    STORE_OVERHEAD,
    _percentile,
    environment,
    gil,
)
from shootout.clientworker import (
    NATIVE_CLIENT,
    RECORDS_PER_COMMIT,
    ZEO_CLIENT,
    commits_per_worker,
    records_of,
)
from shootout.storages import (
    STORAGES,
    _zeo_server,
    _zoe_server,
    admin_address,
    server_loads,
    server_versions,
)

SCHEMA_VERSION = 1
SCHEMA = Path(__file__).resolve().parents[3] / "schema" / "client.schema.json"
CLIENTS = (ZEO_CLIENT, NATIVE_CLIENT)
SERVERS = ("zoe-serve", "zeo")
SHAPES = (("threads", 1), ("threads", 16), ("processes", 16))
WORKLOADS = ("cold", "warm", "commit")
SIZES = (KB, 100 * KB)
# The server that the verdict counts.
VERDICT_SERVER = "zoe-serve"
ROUNDS = 3
OBJECTS = 1000
# The most workers of a shape: the load store holds the oids of each.
MOST = max(concurrency for _, concurrency in SHAPES)
# The bytes that a cache entry takes in addition to its record.
CACHE_ENTRY_OVERHEAD = 512
# The extension of the cache file of each client.
CACHE_EXTENSIONS = {ZEO_CLIENT: ".zec", NATIVE_CLIENT: ".zoec"}
# The short name of each client in the work directory.
TAGS = {ZEO_CLIENT: "zeo", NATIVE_CLIENT: "native"}


def cache_size(objects: int, size: int) -> int:
    """Return a cache size that holds `objects` records of `size` bytes.

    Both clients get the same size. A warm round then finds every record
    of its cold round in the cache.
    """
    return objects * (size + CACHE_ENTRY_OVERHEAD) * 5 // 4 + 1024 * KB


def disk_needed(objects: int, size: int) -> int:
    """Return an upper bound of the bytes that one server and size take.

    The load store holds the records of the most workers. The cache files
    of both clients exist at the same time, for one shape. The native
    client adds an index of at most an eighth of the cache size, and a
    header. A commit round adds a fresh store.
    """
    record = size + RECORD_OVERHEAD
    load_store = MOST * objects * record + STORE_OVERHEAD

    def cache_file(n):
        return cache_size(n, size) * 9 // 8 + 64 * KB

    caches = 2 * max(cache_file(MOST * objects), MOST * cache_file(objects))
    commits = MOST * commits_per_worker(objects) * RECORDS_PER_COMMIT * record
    return load_store + caches + commits + STORE_OVERHEAD


def run(
    objects: int = OBJECTS,
    servers: tuple[str, ...] = SERVERS,
    workdir: Path | None = None,
    machine: str | None = None,
    rounds: int = ROUNDS,
) -> dict:
    """Run every row against each server and return the result document."""
    if rounds < ROUNDS:
        raise ValueError(f"the benchmark runs at least {ROUNDS} rounds")
    workdir = Path(workdir or tempfile.gettempdir())
    workdir.mkdir(parents=True, exist_ok=True)
    started = datetime.datetime.now(datetime.UTC)
    root = Path(tempfile.mkdtemp(prefix="client-", dir=workdir))
    rounds_of = defaultdict(list)
    workers_of = defaultdict(list)
    for server in servers:
        for size in SIZES:
            needed = disk_needed(objects, size)
            free = shutil.disk_usage(root).free
            if free < needed:
                shutil.rmtree(root)
                raise RuntimeError(
                    f"not enough free disk for the client benchmark against "
                    f"{server} with records of {size} bytes: it needs {needed} "
                    f"bytes, and the disk of {workdir} has {free} bytes free"
                )
            print(f"== {server}, records of {size} bytes", flush=True)
            base = root / f"{server}-{size}"
            _load_rounds(server, size, objects, rounds, base, rounds_of, workers_of)
            _commit_rounds(server, size, objects, rounds, base, rounds_of, workers_of)
            shutil.rmtree(base)
    shutil.rmtree(root)

    rows = [
        _row(key, rounds_of[key], workers_of[key])
        for key in _keys(servers)
        if key in rounds_of
    ]
    software = environment.software(("ZEO", "ZODB", "zoedb"))
    if "zoe-serve" in servers:
        software |= server_versions(STORAGES["zoe-server"], None)
    return {
        "schema_version": SCHEMA_VERSION,
        "machine": environment.machine(workdir, machine),
        "python": environment.python(),
        "gil_enabled_by": sorted(
            {
                name
                for row in rows
                for w in row["workers"]
                for name in w["gil_enabled_by"]
            }
        ),
        "software": software,
        "run": {
            "started": started.isoformat(timespec="seconds").replace("+00:00", "Z"),
            "commit": environment.commit(),
            "objects": objects,
        },
        "rows": rows,
    }


def _keys(servers) -> list[tuple]:
    """Return the key of each row, in the order of the report."""
    return [
        (client, server, mode, concurrency, workload, size)
        for server in servers
        for mode, concurrency in SHAPES
        for workload in WORKLOADS
        for size in SIZES
        for client in CLIENTS
    ]


def _order(round_number: int) -> tuple[str, ...]:
    """Return the clients in the order of a round. The first one changes."""
    return CLIENTS if round_number % 2 == 0 else CLIENTS[::-1]


@contextmanager
def _server(server: str, directory: Path) -> Iterator[tuple[tuple, str | None]]:
    """Start a server on a fresh store in directory.

    Yield its address as (host, port), and the admin address of `zoe
    serve`, which counts the loads, or None.
    """
    directory.mkdir(parents=True)
    if server == "zoe-serve":
        with _zoe_server(directory, admin=True) as address:
            host, port = address.rsplit(":", 1)
            yield (host, int(port)), admin_address(directory)
    elif server == "zeo":
        # The native client speaks only the msgpack protocol M5, as zoe
        # serve does. So both clients speak M5 to both servers.
        with _zeo_server(directory, msgpack=True) as port:
            yield ("127.0.0.1", port), None
    else:
        raise ValueError(f"unknown server {server!r}")


def _populate(address: tuple, size: int, count: int, path: Path) -> None:
    """Commit `count` records of `size` bytes, and write their oids to path.

    A client of ZEO writes them, in commits of 100 records. No round
    measures it.
    """
    from ZEO.ClientStorage import ClientStorage
    from ZODB.Connection import TransactionMetaData
    from ZODB.utils import z64

    records = records_of(size, 0)
    storage = ClientStorage(address, wait=True, wait_timeout=120)
    oids = []
    try:
        while len(oids) < count:
            batch = [
                storage.new_oid() for _ in range(min(len(records), count - len(oids)))
            ]
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            for oid, data in zip(batch, records, strict=False):
                storage.store(oid, z64, data, "", txn)
            storage.tpc_vote(txn)
            storage.tpc_finish(txn)
            oids += batch
    finally:
        storage.close()
    path.write_bytes(b"".join(oids))


def _load_rounds(server, size, objects, rounds, base, rounds_of, workers_of):
    """Run the cold and warm rounds of each shape against one load store."""
    with _server(server, base / "load") as (address, admin):
        oids = base / "oids"
        _populate(address, size, MOST * objects, oids)
        for number in range(rounds):
            for mode, concurrency in SHAPES:
                caches = base / f"cache-{number}-{mode}-{concurrency}"
                for workload in ("cold", "warm"):
                    for client in _order(number):
                        var = caches / TAGS[client]
                        var.mkdir(parents=True, exist_ok=True)
                        if workload == "warm":
                            _check_cache_files(client, mode, concurrency, var)
                        spec = {
                            "client": client,
                            "address": list(address),
                            "mode": mode,
                            "concurrency": concurrency,
                            "workload": workload,
                            "record_size": size,
                            "objects": objects,
                            "oids": str(oids),
                            "var": str(var),
                            "cache_size": cache_size(
                                objects * (concurrency if mode == "threads" else 1),
                                size,
                            ),
                        }
                        before = server_loads(admin) if admin else None
                        result, workers = _round(
                            spec, caches / f"{workload}-{TAGS[client]}"
                        )
                        if admin:
                            check_loads(
                                spec, result["calls"], server_loads(admin) - before
                            )
                        key = (client, server, mode, concurrency, workload, size)
                        rounds_of[key].append(result)
                        workers_of[key] += workers
                # The cache files of a shape take room. The next round
                # writes them again.
                shutil.rmtree(caches)


def _commit_rounds(server, size, objects, rounds, base, rounds_of, workers_of):
    """Run each commit round on a fresh store."""
    for number in range(rounds):
        for mode, concurrency in SHAPES:
            for client in _order(number):
                directory = (
                    base / f"commit-{number}-{mode}-{concurrency}-{TAGS[client]}"
                )
                with _server(server, directory / "server") as (address, _):
                    spec = {
                        "client": client,
                        "address": list(address),
                        "mode": mode,
                        "concurrency": concurrency,
                        "workload": "commit",
                        "record_size": size,
                        "objects": objects,
                        "cache_size": cache_size(RECORDS_PER_COMMIT, size),
                    }
                    result, workers = _round(spec, directory)
                key = (client, server, mode, concurrency, "commit", size)
                rounds_of[key].append(result)
                workers_of[key] += workers
                shutil.rmtree(directory)


def _check_cache_files(client: str, mode: str, concurrency: int, var: Path) -> None:
    """Fail a warm round whose client wrote no cache file in its cold round."""
    clients = concurrency if mode == "processes" else 1
    for worker in range(clients):
        path = var / f"w{worker}-1{CACHE_EXTENSIONS[client]}"
        if not path.exists():
            raise RuntimeError(
                f"the cold round of {client} wrote no cache file {path.name}, "
                "so its warm round cannot start from it"
            )


def check_loads(spec: dict, calls: int, server_loads: int) -> None:
    """Check the loads that `zoe serve` counted in a cold or warm round.

    Each load of a cold round reaches the server. No load of a warm round
    reaches it: the cache file of the cold round holds every record.
    """
    workload, client = spec["workload"], spec["client"]
    if workload == "cold" and server_loads < calls:
        raise RuntimeError(
            f"the cold round of {client} made {calls} loads, and the server "
            f"counted {server_loads}: a cache served some of them"
        )
    if workload == "warm" and server_loads > 0:
        raise RuntimeError(
            f"the warm round of {client} made {calls} loads, and the server "
            f"counted {server_loads}: the cache file did not serve them all"
        )


def _round(spec: dict, directory: Path) -> tuple[dict, list[dict]]:
    """Run one round in its own process. Return its result and GIL records."""
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "spec.json"
    path.write_text(json.dumps(spec))
    records = directory / "gil"
    records.mkdir()
    done = subprocess.run(
        [sys.executable, "-m", "shootout.clientworker", str(path)],
        capture_output=True,
        text=True,
        env=gil.environ(records),
        check=False,
    )
    shape = f"{spec['mode']} {spec['concurrency']}"
    name = f"{spec['workload']} round of {spec['client']} ({shape})"
    if done.returncode != 0:
        tail = "\n".join(done.stderr.splitlines()[-40:])
        raise RuntimeError(f"the {name} failed:\n{tail}")
    result = json.loads(done.stdout.splitlines()[-1])
    workers = gil.read(records)
    if not workers:
        raise RuntimeError(f"no process of the {name} wrote its GIL record")
    shutil.rmtree(records)
    return result, workers


def _row(key: tuple, results: list[dict], workers: list[dict]) -> dict:
    """Turn the rounds of one client and row into a row of the result."""
    client, server, mode, concurrency, workload, size = key
    rates = [r["calls"] / max(r["end"] - r["start"], 1e-9) for r in results]
    cpus = [r["cpu_seconds"] / r["objects"] for r in results]
    times = [t for r in results for t in r["times"]]
    rate = statistics.median(rates)
    return {
        "client": client,
        "server": server,
        "mode": mode,
        "concurrency": concurrency,
        "workload": workload,
        "record_size": size,
        "calls_per_second": rate,
        "p50_seconds": _percentile(times, 50),
        "p99_seconds": _percentile(times, 99),
        "client_cpu_per_object_seconds": statistics.median(cpus),
        "rounds": len(results),
        "spread": (max(rates) - min(rates)) / rate,
        "workers": workers,
    }


ROW = (
    "{:<9} {:>2} {:<6} {:>6} {:<9} {:>10} {:>10} {:>6} {:>8} {:>8}"
    " {:>8} {:>8} {:>6}  {}"
)


def report(result: dict, out) -> int:
    """Print one row per shape, workload, record size and server.

    A row against `zoe serve` passes when the native client has a higher
    rate and a lower client CPU per object than `ZEO.ClientStorage`, and
    fails otherwise. A row against the ZEO server shows the same
    comparison in lower case, and does not count. A row with the numbers
    of only one client is missing. Return 0 when every row against `zoe
    serve` passes and no row is missing, else 1.
    """
    rows = {}
    for row in result["rows"]:
        key = (
            row["mode"],
            row["concurrency"],
            row["workload"],
            row["record_size"],
            row["server"],
        )
        if (row["client"], key) in rows:
            raise ValueError(
                f"two rows of {row['client']} for {' '.join(map(str, key))}"
            )
        rows[(row["client"], key)] = row
    seen = {server for _, (*_, server) in rows}
    servers = [s for s in SERVERS if s in seen or s == VERDICT_SERVER]
    servers += sorted(seen - set(servers))
    keys = [
        (mode, concurrency, workload, size, server)
        for server in servers
        for mode, concurrency in SHAPES
        for workload in WORKLOADS
        for size in SIZES
    ]
    keys += sorted({key for _, key in rows} - set(keys))

    print(
        ROW.format(
            "mode",
            "c",
            "load",
            "size",
            "server",
            "ZEO/s",
            "native/s",
            "ratio",
            "ZEO cpu",
            "nat cpu",
            "ZEO p99",
            "nat p99",
            "spread",
            "verdict",
        ),
        file=out,
    )
    print(" " * 64 + "(µs per object)    (ms per call)", file=out)
    lose, missing = [], []
    for key in keys:
        zeo, native = rows.get((ZEO_CLIENT, key)), rows.get((NATIVE_CLIENT, key))
        counts = key[4] == VERDICT_SERVER
        if zeo is None or native is None:
            missing.append(key)
            numbers = ["-"] * 8
            verdict = "missing"
        else:
            beats = (
                native["calls_per_second"] > zeo["calls_per_second"]
                and native["client_cpu_per_object_seconds"]
                < zeo["client_cpu_per_object_seconds"]
            )
            verdict = "pass" if beats else ("FAIL" if counts else "fail")
            if counts and not beats:
                lose.append(key)
            numbers = [
                f"{zeo['calls_per_second']:.0f}",
                f"{native['calls_per_second']:.0f}",
                f"{native['calls_per_second'] / zeo['calls_per_second']:.2f}",
                f"{zeo['client_cpu_per_object_seconds'] * 1e6:.1f}",
                f"{native['client_cpu_per_object_seconds'] * 1e6:.1f}",
                f"{zeo['p99_seconds'] * 1e3:.3f}",
                f"{native['p99_seconds'] * 1e3:.3f}",
                f"{max(zeo['spread'], native['spread']):.0%}",
            ]
        if not counts and verdict != "missing":
            verdict += " (does not count)"
        print(ROW.format(*key, *numbers, verdict), file=out)

    if not lose and not missing:
        print(
            f"verdict: the native client beats {ZEO_CLIENT} in every row "
            "against zoe serve",
            file=out,
        )
        return 0
    if lose:
        print(
            f"verdict: the native client does not beat {ZEO_CLIENT} in "
            f"{len(lose)} rows against zoe serve",
            file=out,
        )
    if missing:
        print(
            f"verdict: {len(missing)} rows have the numbers of one client only, "
            "or of none",
            file=out,
        )
    return 1
