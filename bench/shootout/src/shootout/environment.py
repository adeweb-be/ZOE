"""The description of the machine, the Python build and the software.

The description holds no host name, user name or path.
"""

import datetime
import importlib.metadata
import os
import platform
import sys
import sysconfig
from pathlib import Path

from benchcommon import provenance

# The distributions whose versions every result records.
SOFTWARE = (
    "zodbshootout",
    "pyperf",
    "ZODB",
    "ZEO",
    "persistent",
    "BTrees",
    "zodbpickle",
    "transaction",
)
RELSTORAGE_SOFTWARE = ("RelStorage", "psycopg2-binary")
ZOE_SOFTWARE = ("zoedb",)


def machine(workdir: Path, name: str | None = None) -> dict:
    """Describe the machine. The disk is the one that holds workdir."""
    cpu = provenance.cpu_model()
    return {
        "name": name or provenance.machine_name(cpu),
        "cpu": cpu,
        "logical_cpus": os.cpu_count() or 1,
        "memory_bytes": os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES"),
        "os": platform.system(),
        "kernel": platform.release(),
        "arch": platform.machine(),
        "disk": _disk(workdir),
    }


def python() -> dict:
    """Describe the Python build that runs the benchmarks."""
    return {
        "implementation": sys.implementation.name,
        "version": platform.python_version(),
        "free_threaded": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
        "compiler": platform.python_compiler(),
    }


def python_tag(python: dict) -> str:
    """Return a short name for a Python build, such as cp313 or cp314t."""
    major, minor = python["version"].split(".")[:2]
    prefix = "cp" if python["implementation"] == "cpython" else python["implementation"]
    return f"{prefix}{major}{minor}{'t' if python['free_threaded'] else ''}"


def software(names: tuple[str, ...]) -> dict[str, str]:
    """Return the installed version of each distribution."""
    return {name: importlib.metadata.version(name) for name in names}


def commit() -> str | None:
    """Return the commit of the repository that the harness runs from."""
    return provenance.commit()


def dirty() -> bool | None:
    """Return whether the tree holds changes that its commit does not, as
    `benchcommon.provenance.dirty` gives it."""
    return provenance.dirty()


def header(
    schema_version: int,
    workdir: Path,
    machine_name: str | None,
    software_versions: dict[str, str],
    started: datetime.datetime,
    **run,
) -> dict:
    """Return the head of a result document: its version, the machine, the
    Python build, the software, and the run with its start, its commit,
    whether the tree held changes and the fields of `run`."""
    return {
        "schema_version": schema_version,
        "machine": machine(workdir, machine_name),
        "python": python(),
        "software": software_versions,
        "run": {
            "started": started.isoformat(timespec="seconds").replace("+00:00", "Z"),
            "commit": commit(),
            "dirty": dirty(),
            **run,
        },
    }


def _disk(path: Path) -> dict | None:
    """Describe the physical block device that holds path, if Linux shows it."""
    dev = os.stat(path).st_dev
    block = Path(f"/sys/dev/block/{os.major(dev)}:{os.minor(dev)}")
    if not block.exists():
        return None
    device = block.resolve()
    # Go from a device mapper or md device to the first device under it.
    while slaves := sorted((device / "slaves").glob("*")):
        device = slaves[0].resolve()
    if (device / "partition").exists():
        device = device.parent
    model = _read(device / "device" / "model")
    rotational = _read(device / "queue" / "rotational")
    if model is None and rotational is None:
        return None
    return {
        "model": model,
        "rotational": None if rotational is None else rotational == "1",
    }


def _read(path: Path) -> str | None:
    try:
        return path.read_text().strip() or None
    except OSError:
        return None
