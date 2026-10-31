"""The storages under test, as ZConfig sections for zodbshootout.

Every storage runs with its default settings, but `relstorage-hf` keeps no
history. `zconfig` starts a server on a new store when the storage needs
one. `reset_database` and `empty_database` prepare the database of
`relstorage` and `relstorage-hf`, which keep their data outside the work
directory.
"""

import contextlib
import subprocess
import urllib.request
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path

from benchcommon import servers


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
    # The functions of the two modes have the same names but other
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
def _zeo_server(workdir: Path, msgpack: bool = False) -> Iterator[int]:
    """Run a ZEO server on a FileStorage in workdir and yield its port.

    With msgpack, the server speaks the msgpack protocol M5 of ZEO, and
    not the pickle protocol Z5.
    """
    env = servers.scrubbed()
    if msgpack:
        env["ZEO_MSGPACK"] = "1"
    with servers.zeo_server(workdir, env=env) as port:
        yield port


def zoe_binary() -> str:
    """Return the `zoe` binary that the harness runs as `zoe serve`:
    `ZOE_BINARY`, else `target/release/zoe` of the repository. Raise
    FileNotFoundError, with the command that builds it, when it is
    missing."""
    return str(servers.zoe_binary())


@contextlib.contextmanager
def _zoe_server(workdir: Path, admin: bool = False) -> Iterator[str]:
    """Run `zoe serve` on a store in workdir and yield its address.

    The server runs with its default settings: no `ZOE_*` variable of the
    environment reaches it. With admin, it also listens on an admin port,
    and `admin_address` gives that address.
    """
    # zoe serve writes `admin <address>` before `listening <address>`.
    args = ("--admin-listen", "127.0.0.1:0") if admin else ()
    with servers.zoe_serve(workdir, args) as address:
        yield address
