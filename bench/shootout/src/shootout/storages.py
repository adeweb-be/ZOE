"""The storages under test, as ZConfig sections for zodbshootout.

Every storage runs with its default settings.
"""

import contextlib
import socket
import subprocess
import sys
import time
from collections.abc import Iterator
from dataclasses import dataclass
from pathlib import Path


@dataclass(frozen=True)
class Storage:
    """One storage that the harness can measure."""

    name: str
    # FileStorage and zoe lock their files, so only one process can open them.
    multi_process: bool
    needs_dsn: bool


STORAGES = {
    s.name: s
    for s in (
        Storage("filestorage", multi_process=False, needs_dsn=False),
        Storage("zeo", multi_process=True, needs_dsn=False),
        Storage("relstorage", multi_process=True, needs_dsn=True),
        Storage("zoe", multi_process=False, needs_dsn=False),
    )
}


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
    elif storage.name == "relstorage":
        if not dsn:
            raise ValueError("relstorage needs a PostgreSQL DSN")
        # ZConfig reads "$" as the start of a substitution.
        dsn = dsn.replace("$", "$$")
        yield "%import relstorage\n" + _section(
            storage.name,
            "<relstorage>",
            "<postgresql>",
            f"dsn {dsn}",
            "</postgresql>",
            "</relstorage>",
        )
    elif storage.name == "zoe":
        yield "%import zoe\n" + _section(
            storage.name, "<zoe>", f"path {workdir / 'store'}", "</zoe>"
        )
    else:
        raise ValueError(f"unknown storage {storage.name!r}")


def server_versions(storage: Storage, dsn: str | None) -> dict[str, str]:
    """Return the versions of the servers that the storage talks to."""
    if storage.name != "relstorage":
        return {}
    import psycopg2

    with contextlib.closing(psycopg2.connect(dsn)) as conn:
        major, minor = divmod(conn.server_version, 10000)
    return {"PostgreSQL": f"{major}.{minor}"}


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
