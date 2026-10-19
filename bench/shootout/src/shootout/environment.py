"""The description of the machine, the Python build and the software.

The description holds no host name, user name or path.
"""

import importlib.metadata
import os
import platform
import re
import subprocess
import sys
import sysconfig
from pathlib import Path

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
    cpu = _cpu_model()
    return {
        "name": name or os.environ.get("ZOE_BENCH_MACHINE") or _slug(cpu),
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
    if value := os.environ.get("ZOE_BENCH_COMMIT"):
        return value
    return _git("rev-parse", "HEAD") or None


def dirty() -> bool | None:
    """Return true when the tree of the harness holds changes that its
    commit does not, false when it holds none, and None when git fails, as
    in a tree with no .git. Return None also when ZOE_BENCH_COMMIT names
    another commit than the tree: the tree is then no record of that
    commit."""
    head = _git("rev-parse", "HEAD")
    if not head:
        return None
    named = os.environ.get("ZOE_BENCH_COMMIT")
    if named and not head.startswith(named.lower()):
        return None
    status = _git("status", "--porcelain", "--untracked-files=no")
    if status is None:
        return None
    return bool(status)


def _git(*args: str) -> str | None:
    """Run git in the tree of the harness. Return its output without the
    spaces at its ends, or None when it fails."""
    try:
        out = subprocess.run(
            ["git", *args],
            capture_output=True,
            text=True,
            check=True,
            cwd=Path(__file__).parent,
        )
    except (OSError, subprocess.CalledProcessError):
        return None
    return out.stdout.strip()


def _cpu_model() -> str:
    try:
        cpuinfo = Path("/proc/cpuinfo").read_text()
    except OSError:
        cpuinfo = ""
    if match := re.search(r"^model name\s*:\s*(.+)$", cpuinfo, re.MULTILINE):
        return match.group(1).strip()
    return platform.processor() or platform.machine() or "unknown"


def _slug(text: str) -> str:
    text = re.sub(r"\((r|tm)\)", "", text.lower())
    return re.sub(r"[^a-z0-9]+", "-", text).strip("-") or "unknown"


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
