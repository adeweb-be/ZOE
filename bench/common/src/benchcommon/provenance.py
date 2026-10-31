"""Where the numbers of a result come from: the CPU model and the name of
the machine, the commit, and whether the tree differs from it (ADR 0054).
No host name, no user name and no path."""

import os
import platform
import re
import subprocess
import sys
from pathlib import Path

from benchcommon.servers import REPO


def sysctl(name: str) -> str | None:
    """Return the value of the sysctl `name` of macOS, or None."""
    try:
        out = subprocess.run(["sysctl", "-n", name], capture_output=True, text=True)
    except (OSError, subprocess.SubprocessError):
        return None
    return out.stdout.strip() or None


def cpu_model() -> str:
    """Return the model of the CPU, such as `Apple M2 Pro` or
    `Intel(R) Core(TM) Ultra 7 155H`."""
    if sys.platform == "darwin":
        if model := sysctl("machdep.cpu.brand_string"):
            return model
    else:
        try:
            cpuinfo = Path("/proc/cpuinfo").read_text()
        except OSError:
            cpuinfo = ""
        if match := re.search(r"^model name\s*:\s*(.+)$", cpuinfo, re.MULTILINE):
            return match.group(1).strip()
    return platform.processor() or platform.machine() or "unknown"


def slug(text: str) -> str:
    """Return a CPU model as a machine name: intel-core-ultra-7-155h."""
    text = re.sub(r"\((r|tm)\)", "", text.lower())
    return re.sub(r"[^a-z0-9]+", "-", text).strip("-") or "unknown"


def machine_name(cpu: str | None = None) -> str:
    """Return the name of the machine in the results: `ZOE_BENCH_MACHINE`,
    else the slug of the CPU model."""
    return os.environ.get("ZOE_BENCH_MACHINE") or slug(cpu or cpu_model())


def git(*args: str, cwd: Path = REPO) -> str | None:
    """Run git in `cwd`, by default the repository. Return its stdout, or
    None when git fails."""
    try:
        done = subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True)
    except (OSError, subprocess.SubprocessError):
        return None
    return done.stdout if done.returncode == 0 else None


def commit() -> str | None:
    """Return the commit of a result: `ZOE_BENCH_COMMIT`, else the HEAD of
    the repository, else None."""
    if named := os.environ.get("ZOE_BENCH_COMMIT"):
        return named
    return (git("rev-parse", "HEAD") or "").strip() or None


def dirty() -> bool | None:
    """Return true when the tracked files of the repository hold changes
    that its commit does not, false when they hold none, and None when git
    fails, as in a tree with no .git. Return None also when
    `ZOE_BENCH_COMMIT` names another commit than the tree: the tree is then
    no record of that commit. Untracked files do not count."""
    head = (git("rev-parse", "HEAD") or "").strip()
    if not head:
        return None
    named = os.environ.get("ZOE_BENCH_COMMIT")
    if named and not head.startswith(named.lower()):
        return None
    status = git("status", "--porcelain", "--untracked-files=no")
    if status is None:
        return None
    return bool(status.strip())
