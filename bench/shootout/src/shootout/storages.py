"""The storages under test, as ZConfig sections for zodbshootout.

Every storage runs with its default settings, but `relstorage-hf` keeps no
history. `zconfig` starts a server on a new store when the storage needs
one. `reset_database` and `empty_database` prepare the database of
`relstorage` and `relstorage-hf`, which keep their data outside the work
directory.
"""

import contextlib
import os
import signal
import socket
import subprocess
import sys
import time
import urllib.request
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Storage:
    """One storage that the harness can measure."""

    name: str
    # FileStorage and zoe lock their files, so only one process can open them.
    # A server takes the connections of several processes.
    multi_process: bool
    needs_dsn: bool


STORAGES = {
    s.name: s
    for s in (
        Storage("filestorage", multi_process=False, needs_dsn=False),
        Storage("zeo", multi_process=True, needs_dsn=False),
        Storage("relstorage", multi_process=True, needs_dsn=True),
        Storage("zoe", multi_process=False, needs_dsn=False),
        Storage("zoe-server", multi_process=True, needs_dsn=False),
        Storage("zoe-native", multi_process=True, needs_dsn=False),
        Storage("relstorage-hf", multi_process=True, needs_dsn=True),
    )
}

# The storages that keep their data in PostgreSQL, each with its value of
# keep-history.
RELSTORAGE = {"relstorage": True, "relstorage-hf": False}

# The storages whose read_cold the harness checks against the loads that
# the server counts. Their `zoe serve` listens on an admin port too.
COUNTS_COLD_LOADS = frozenset({"zoe-native"})


@contextlib.contextmanager
def zconfig(storage: Storage, workdir: Path, dsn: str | None) -> Iterator[str]:
    """Start what the storage needs in workdir and yield its ZConfig text."""
    if storage.name == "filestorage":
        yield _section(
            storage.name,
            "<filestorage>",
            f"path {workdir / 'Data.fs'}",
            "</filestorage>",
        )
    elif storage.name == "zeo":
        with _zeo_server(workdir) as port:
            yield _section(
                storage.name, "<zeoclient>", f"server 127.0.0.1:{port}", "</zeoclient>"
            )
    elif storage.name in RELSTORAGE:
        lines = _relstorage(dsn, RELSTORAGE[storage.name])
        yield "%import relstorage\n" + _section(storage.name, *lines)
    elif storage.name == "zoe":
        yield "%import zoe\n" + _section(
            storage.name, "<zoe>", f"path {workdir / 'store'}", "</zoe>"
        )
    elif storage.name == "zoe-server":
        with _zoe_server(workdir) as address:
            yield _section(
                storage.name, "<zeoclient>", f"server {address}", "</zeoclient>"
            )
    elif storage.name == "zoe-native":
        # The key native selects the native inside of zoe.zeo.ClientStorage.
        with _zoe_server(workdir, admin=True) as address:
            yield "%import zoe\n" + _section(
                storage.name,
                "<zoeclient>",
                f"server {address}",
                "native true",
                "</zoeclient>",
            )
    else:
        raise ValueError(f"unknown storage {storage.name!r}")


def empty_database(storage: Storage, dsn: str | None) -> None:
    """Remove every object and transaction from the database of RelStorage.

    RelStorage's zap_all truncates its tables. The other storages keep their
    data in the work directory, so the function does nothing for them.
    """
    if storage.name not in RELSTORAGE:
        return
    import ZODB.config

    lines = _relstorage(dsn, RELSTORAGE[storage.name])
    text = "%import relstorage\n" + "".join(f"{line}\n" for line in lines)
    relstorage = ZODB.config.storageFromString(text)
    try:
        relstorage.zap_all()
    finally:
        relstorage.close()


def reset_database(storage: Storage, dsn: str | None) -> None:
    """Drop the schema of RelStorage from the database, in both history modes.

    `relstorage` and `relstorage-hf` use the same database, and RelStorage
    refuses to open a database that has the schema of the other mode. So
    the harness calls this function before the first value of each of the
    two storages. The function drops the tables, the sequences and the
    views of both modes, then the functions of both modes. The next open
    creates the schema of the storage. The function does nothing for the
    other storages.
    """
    if storage.name not in RELSTORAGE:
        return
    if not dsn:
        raise ValueError(f"{storage.name} needs a PostgreSQL DSN")
    import psycopg2
    from relstorage.adapters.postgresql import PostgreSQLAdapter
    from relstorage.options import Options

    names = set()
    for keep_history in RELSTORAGE.values():
        adapter = PostgreSQLAdapter(dsn=dsn, options=Options(keep_history=keep_history))
        adapter.schema.drop_all()
        names.update(adapter.schema.procedures)
    # The functions of the two modes have the same names and other
    # arguments, so each mode would keep the functions of the other.
    with contextlib.closing(psycopg2.connect(dsn)) as conn:
        with conn, conn.cursor() as cursor:
            cursor.execute(
                "SELECT p.oid::regprocedure::text FROM pg_proc p"
                " JOIN pg_namespace n ON n.oid = p.pronamespace"
                " WHERE n.nspname = current_schema() AND p.proname = ANY(%s)",
                (sorted(names),),
            )
            for (function,) in cursor.fetchall():
                cursor.execute(f"DROP FUNCTION {function}")


def admin_address(workdir: Path) -> str:
    """Return the admin address of the `zoe serve` that runs in workdir."""
    out = workdir / "zoe-serve.out"
    for line in out.read_text(errors="replace").splitlines():
        if line.startswith("admin "):
            return line.split()[1]
    raise RuntimeError(f"zoe serve wrote no admin address to {out}")


def server_loads(admin: str) -> int:
    """Return the loads that `zoe serve` counted, from its admin port.

    The value is the sum of `zoe_loads_total` over the storage names.
    """
    url = f"http://{admin}/metrics"
    with urllib.request.urlopen(url, timeout=30) as reply:
        text = reply.read().decode()
    total = 0.0
    for line in text.splitlines():
        name, _, value = line.rpartition(" ")
        if name.split("{", 1)[0] == "zoe_loads_total":
            total += float(value)
    return int(total)


def server_versions(storage: Storage, dsn: str | None) -> dict[str, str]:
    """Return the versions of the servers that the storage talks to."""
    if storage.name in ("zoe-server", "zoe-native"):
        done = subprocess.run(
            [zoe_binary(), "--version"], capture_output=True, text=True, check=True
        )
        # clap prints "zoe <version>".
        return {"zoe": done.stdout.split()[-1]}
    if storage.name not in RELSTORAGE:
        return {}
    import psycopg2

    with contextlib.closing(psycopg2.connect(dsn)) as conn:
        major, minor = divmod(conn.server_version, 10000)
    return {"PostgreSQL": f"{major}.{minor}"}


def _relstorage(dsn: str | None, keep_history: bool = True) -> tuple[str, ...]:
    """Return the lines of the RelStorage section on PostgreSQL.

    The section sets only the DSN, and keep-history when it is false.
    """
    if not dsn:
        raise ValueError("relstorage needs a PostgreSQL DSN")
    # ZConfig reads "$" as the start of a substitution.
    dsn = dsn.replace("$", "$$")
    return (
        "<relstorage>",
        *(() if keep_history else ("keep-history false",)),
        "<postgresql>",
        f"dsn {dsn}",
        "</postgresql>",
        "</relstorage>",
    )


def _section(name: str, *lines: str) -> str:
    body = "".join(f"  {line}\n" for line in lines)
    return f"<zodb {name}>\n{body}</zodb>\n"


@contextlib.contextmanager
def _zeo_server(workdir: Path) -> Iterator[int]:
    """Run a ZEO server on a FileStorage in workdir and yield its port."""
    port = _free_port()
    log = workdir / "zeo.log"
    with log.open("wb") as out:
        server = subprocess.Popen(
            [
                sys.executable,
                "-m",
                "ZEO.runzeo",
                "-a",
                f"127.0.0.1:{port}",
                "-f",
                str(workdir / "Data.fs"),
            ],
            stdout=out,
            stderr=subprocess.STDOUT,
        )
    try:
        _wait_for_port(port, server, log)
        yield port
    finally:
        server.terminate()
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


def zoe_binary() -> str:
    """Return the `zoe` binary that the harness runs as `zoe serve`.

    `ZOE_BINARY` gives it. Else it is `target/release/zoe` of the
    repository, under `CARGO_TARGET_DIR` when it is set. When that binary
    is missing, cargo builds it.
    """
    if binary := os.environ.get("ZOE_BINARY"):
        return binary
    root = Path(__file__).resolve().parents[4]
    # cargo runs in the repository, so a relative target directory starts
    # there.
    target = root / (os.environ.get("CARGO_TARGET_DIR") or "target")
    binary = target / "release" / "zoe"
    if not binary.exists():
        print(f"building {binary}", file=sys.stderr, flush=True)
        command = ["cargo", "build", "--release", "--locked"]
        subprocess.run(
            command + ["-p", "zoe-server", "--bin", "zoe"], cwd=root, check=True
        )
    return str(binary)


@contextlib.contextmanager
def _zoe_server(workdir: Path, admin: bool = False) -> Iterator[str]:
    """Run `zoe serve` on a store in workdir and yield its address.

    The server runs with its default settings: no `ZOE_*` variable of the
    environment reaches it. With admin, it also listens on an admin port,
    and `admin_address` gives that address.
    """
    out = workdir / "zoe-serve.out"
    log = workdir / "zoe-serve.log"
    environ = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
    command = [zoe_binary(), "serve", "--path", str(workdir / "store")]
    # The port 0 takes a free port. `zoe serve` writes `listening <address>`
    # to stdout once it accepts connections.
    command += ["--listen", "127.0.0.1:0"]
    if admin:
        # zoe serve writes `admin <address>` before `listening <address>`.
        command += ["--admin-listen", "127.0.0.1:0"]
    with out.open("wb") as stdout, log.open("wb") as stderr:
        server = subprocess.Popen(
            command,
            stdin=subprocess.DEVNULL,
            stdout=stdout,
            stderr=stderr,
            env=environ,
        )
    try:
        yield _wait_for_listening(out, server, log)
    finally:
        # SIGTERM stops the server cleanly.
        server.send_signal(signal.SIGTERM)
        try:
            server.wait(timeout=30)
        except subprocess.TimeoutExpired:
            server.kill()
            server.wait()


def _wait_for_listening(out: Path, server: subprocess.Popen, log: Path) -> str:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        for line in out.read_text(errors="replace").splitlines():
            if line.startswith("listening "):
                return line.split()[1]
        if server.poll() is not None:
            raise RuntimeError(f"zoe serve stopped:\n{log.read_text()}")
        time.sleep(0.1)
    server.kill()
    server.wait()
    raise RuntimeError(f"zoe serve did not listen:\n{log.read_text()}")


def _free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def _wait_for_port(port: int, server: subprocess.Popen, log: Path) -> None:
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        if server.poll() is not None:
            raise RuntimeError(f"the ZEO server stopped:\n{log.read_text()}")
        with contextlib.suppress(OSError):
            socket.create_connection(("127.0.0.1", port), timeout=1).close()
            return
        time.sleep(0.1)
    raise RuntimeError(f"the ZEO server did not listen on port {port}")
