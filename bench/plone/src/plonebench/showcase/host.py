"""The machine side of the showcase: its log, the bench lock, the load
average, the sleep of the machine, and what a build or a prepare leaves
for the runs after it."""

from __future__ import annotations

import contextlib
import glob
import hashlib
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

from benchcommon import lock
from benchcommon import servers as common
from benchcommon.provenance import commit as commit_of
from benchcommon.provenance import dirty as dirty_of

from plonebench import fixture, stack

# The interval between two reads of the load average, in seconds.
LOAD_POLL = 15.0

__all__ = ["commit_of", "dirty_of"]


class Log:
    """The log of a showcase: each line goes to stdout and to
    DIR/showcase.log."""

    def __init__(self, path: Path):
        self.path = path

    def __call__(self, message: str) -> None:
        line = f"{time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} {message}"
        print(line, flush=True)
        with open(self.path, "a") as file:
            file.write(line + "\n")


@contextlib.contextmanager
def bench_lock(log: Log):
    """Hold an exclusive flock(2) on the bench lock, so that no other
    benchmark runs beside the run. A child command of the run takes the
    lock through the open file of the showcase, so it does not wait."""
    with lock.held(log):
        yield


def wait_for_load(max_load: float, stop: Path, log: Log) -> bool:
    """Wait while the 1-minute load average is `max_load` or more. Return
    False when the file `stop` appears during the wait."""
    told = False
    while True:
        load = os.getloadavg()[0]
        if load < max_load:
            return True
        if stop.exists():
            return False
        if not told:
            log(
                f"wait: the 1-minute load average is {load:.2f}, not below {max_load:g}"
            )
            told = True
        time.sleep(LOAD_POLL)


def keep_awake(log: Log) -> subprocess.Popen | None:
    """Keep the machine awake until `release`, through systemd-inhibit on
    Linux. Warn when it cannot."""
    if not sys.platform.startswith("linux"):
        return None
    inhibit = shutil.which("systemd-inhibit")
    if inhibit is None:
        log("warning: systemd-inhibit is missing: turn off the automatic suspend")
        return None
    command = [inhibit, "--what=sleep:idle", "--who=zoe-showcase", "--mode=block"]
    check = subprocess.run(
        [*command, "--why=check", "true"], capture_output=True, text=True
    )
    if check.returncode != 0:
        log("warning: systemd-inhibit refuses: turn off the automatic suspend")
        return None
    # `cat` reads its stdin until the showcase closes it or ends. Then
    # systemd-inhibit ends and releases the lock of the sleep.
    return subprocess.Popen(
        [*command, "--why=a run of just bench showcase", "cat"],
        stdin=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


def release(process: subprocess.Popen | None) -> None:
    """End the systemd-inhibit of `keep_awake`: `cat` ends when its stdin
    closes, and systemd-inhibit with it."""
    if process is None:
        return
    process.stdin.close()
    common.stop(process, 10)


def record_of(directory: Path, id_: str) -> dict | None:
    """Return the record of a run that ended well in `directory`, or None."""
    path = directory / "state" / f"{id_}.done"
    if not path.exists():
        return None
    try:
        return json.loads(path.read_text())
    except ValueError:
        return {}


# What a build or a prepare leaves for the runs after it.
PROVENANCE = {
    "zoe_sha256": "the zoe binary",
    "zoedb_sha256": "the zoedb extension of the Plone instances",
    "converted_at": "the commit that converted the fixture",
}


def provenance(run: dict) -> dict:
    """Return what a build or a prepare leaves for the runs after it: the
    sha256 of the zoe binary, the sha256 of the zoedb extension of the Plone
    instances, or the commit that converted the fixture."""
    if run["kind"] == "build" and run["target"] == "zoe":
        return {"zoe_sha256": _sha256(stack.zoe_binary())}
    if run["kind"] == "build" and run["target"] == "plone-zoeclient":
        found = sorted(glob.glob(str(stack.PLONE / "zoedb" / "zoe" / "_zoe*.so")))
        return {"zoedb_sha256": _sha256(Path(found[0])) if found else None}
    if run["kind"] == "prepare":
        path = fixture.fixture_dir(run["fixture"]) / "equivalence.json"
        try:
            return {"converted_at": json.loads(path.read_text()).get("converted_at")}
        except (OSError, ValueError):
            return {"converted_at": None}
    return {}


def check_provenance(document: dict, directory: Path, log: Log) -> None:
    """Warn when a build or a fixture changed since its run in `directory`:
    the runs of this start then use another binary or another fixture."""
    for run in document["runs"]:
        record = record_of(directory, run["id"])
        if not record:
            continue
        for key, value in provenance(run).items():
            if key in record and record[key] != value:
                log(
                    f"warning: {PROVENANCE[key]} changed since {run['id']} ran "
                    f"in this directory ({record[key]}, now {value}): the runs "
                    "of this start use the new one"
                )


def _sha256(path: Path) -> str | None:
    try:
        with open(path, "rb") as file:
            return hashlib.file_digest(file, "sha256").hexdigest()
    except OSError:
        return None
