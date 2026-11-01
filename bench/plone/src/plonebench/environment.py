"""The record of the machine and the software of a run. No host name, no
user name and no path."""

from __future__ import annotations

import os
import platform
import re
import subprocess
import sys
from pathlib import Path

from benchcommon import provenance

from plonebench import stack

EGGS = (
    "Plone",
    "Products.CMFPlone",
    "Zope",
    "ZODB",
    "ZEO",
    "persistent",
    "BTrees",
    "plone.restapi",
    "zodbpickle",
    "msgpack",
)


# The CPU model, its slug and the sysctl of macOS are those of every
# harness.
sysctl = provenance.sysctl
slug = provenance.slug
cpu_model = provenance.cpu_model


def memory_bytes() -> int | None:
    if sys.platform == "darwin":
        value = sysctl("hw.memsize")
        return int(value) if value else None
    try:
        with open("/proc/meminfo") as file:
            for line in file:
                if line.startswith("MemTotal"):
                    return int(line.split()[1]) * 1024
    except OSError:
        return None
    return None


def core_kinds() -> dict | None:
    """The performance and efficiency cores of an Apple CPU. The scheduler
    of macOS chooses the kind of core for each thread."""
    if sys.platform != "darwin":
        return None
    kinds = {}
    for level in range(int(sysctl("hw.nperflevels") or 0)):
        name = sysctl(f"hw.perflevel{level}.name") or f"level{level}"
        count = sysctl(f"hw.perflevel{level}.physicalcpu")
        kinds[name] = int(count) if count else None
    return kinds or None


def filesystem(path: Path) -> str | None:
    if sys.platform == "darwin":
        import psutil

        path = path.resolve()
        best = None
        for part in psutil.disk_partitions(all=True):
            mount = Path(part.mountpoint)
            if path == mount or mount in path.parents:
                if best is None or len(str(mount)) > len(best.mountpoint):
                    best = part
        return best.fstype if best else None
    try:
        out = subprocess.run(
            ["stat", "-f", "-c", "%T", str(path)], capture_output=True, text=True
        )
    except OSError:
        return None
    return out.stdout.strip() or None


def durability(zeo_fsync: str) -> dict:
    """How each server makes a commit durable. On macOS, fsync(2) leaves
    the data in the cache of the drive; only fcntl F_FULLFSYNC writes it to
    the medium. `zoe serve` calls fsync(2) there (ADR 0040), as Python's
    os.fsync does, unless the harness makes os.fsync use F_FULLFSYNC."""
    if sys.platform == "darwin":
        zeo = (
            "F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it"
            if zeo_fsync == "full"
            else "fsync(2) in FileStorage: the drive may still cache the data"
        )
        return {
            "zeo": zeo,
            "zoe": "fsync(2) (ADR 0040): the drive may still cache the data",
        }
    return {"zeo": "fsync(2) in FileStorage", "zoe": "fdatasync(2)"}


def cgroup_limits() -> dict:
    limits = {}
    for name in ("cpu.max", "memory.max"):
        path = Path("/sys/fs/cgroup") / name
        if path.exists():
            limits[name] = path.read_text().strip()
    return limits


def eggs() -> dict[str, str]:
    """The versions of the eggs that the buildout scenarios share."""
    found: dict[str, set[str]] = {}
    for path in stack.egg_paths("*.egg"):
        match = re.match(r"^(.+?)-([^-]+)-py", path.name)
        if match and match.group(1) in EGGS:
            found.setdefault(match.group(1), set()).add(match.group(2))
    # The eggs directory can hold two versions of one egg. Take the
    # highest, so the record does not depend on the order of the files.
    return {name: max(found[name], key=_version_key) for name in sorted(found)}


def _version_key(version: str) -> tuple:
    """Order versions by their numbers, then by their text."""
    return [int(number) for number in re.findall(r"\d+", version)], version


def zoedb_version() -> str | None:
    for path in (stack.PLONE / "zoedb").glob("zoedb-*.dist-info"):
        return path.name.split("-")[1]
    return None


def zoe_version() -> str | None:
    try:
        out = subprocess.run(
            [str(stack.zoe_binary()), "--version"],
            capture_output=True,
            text=True,
            timeout=10,
        )
        return out.stdout.strip() or None
    except OSError:
        return None


def git(*args: str) -> str | None:
    """Run git in the repository. Return its output without the spaces at
    its ends, or None when git fails or prints nothing."""
    return (provenance.git(*args) or "").strip() or None


# Whether the tree differs from the commit of the run, as every harness
# records it.
dirty = provenance.dirty


def zope_python() -> dict:
    """The interpreter of the Plone instances, and its GIL."""
    python = stack.PLONE / "venv" / "bin" / "python"
    code = (
        "import sys, sysconfig, json; "
        "print(json.dumps({'implementation': sys.implementation.name, "
        "'version': sys.version.split()[0], "
        "'free_threaded': bool(sysconfig.get_config_var('Py_GIL_DISABLED')), "
        "'gil_enabled': getattr(sys, '_is_gil_enabled', lambda: True)()}))"
    )
    try:
        out = subprocess.run(
            [str(python), "-c", code], capture_output=True, text=True, timeout=30
        )
        import json

        return json.loads(out.stdout)
    except (OSError, ValueError):
        return {}


def record(store_dir: Path, zeo_fsync: str = "plain") -> dict:
    from plonebench.sampler import disk_of

    disk = disk_of(store_dir)
    rotational = None
    if disk:
        try:
            rotational = (
                Path(f"/sys/block/{disk}/queue/rotational").read_text().strip() == "1"
            )
        except OSError:
            pass
    try:
        import locust

        locust_version = locust.__version__
    except ImportError:
        locust_version = None
    return {
        "machine": {
            # The name of the machine in the results repository. None when
            # ZOE_BENCH_MACHINE is not set: the summary then gives a name.
            "name": os.environ.get("ZOE_BENCH_MACHINE") or None,
            "os": platform.system(),
            "cpu": cpu_model(),
            "logical_cpus": os.cpu_count(),
            "core_kinds": core_kinds(),
            "memory_bytes": memory_bytes(),
            "kernel": platform.release(),
            "cgroup": cgroup_limits(),
            "cpu_pinning": stack.pinning(),
            "store_disk": {
                "device": disk,
                "device_rotational": rotational,
                "filesystem": filesystem(store_dir),
            },
            "durability": durability(zeo_fsync),
        },
        "software": {
            "zope_python": zope_python(),
            "runner_python": sys.version.split()[0],
            "eggs": eggs(),
            "zoedb": zoedb_version(),
            "zoe": zoe_version(),
            "locust": locust_version,
        },
        "commit": provenance.commit(),
        "dirty": dirty(),
    }
