"""Sample the processes of a run and the machine from /proc, once a second.

For each process: user and system CPU, resident memory, the bytes read and
written by its I/O, its context switches, and the time its threads waited
for a CPU (the run delay of schedstat). For the machine: the CPU times of
/proc/stat (steal included) and the counters of the disk under the store
from /proc/diskstats (flushes included). The samples go to a JSON lines
file; `summarize` turns a window of them into deltas.
"""

from __future__ import annotations

import json
import os
import threading
import time
from pathlib import Path

TICKS = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")


def read_process(pid: int) -> dict | None:
    try:
        with open(f"/proc/{pid}/stat") as file:
            fields = file.read().rsplit(")", 1)[1].split()
        utime, stime = int(fields[11]), int(fields[12])
        threads, rss = int(fields[17]), int(fields[21])
        io = {}
        try:
            with open(f"/proc/{pid}/io") as file:
                for line in file:
                    key, value = line.split(":")
                    io[key] = int(value)
        except OSError:
            pass
        switches = {}
        with open(f"/proc/{pid}/status") as file:
            for line in file:
                if line.startswith(
                    ("voluntary_ctxt_switches", "nonvoluntary_ctxt_switches")
                ):
                    key, value = line.split(":")
                    switches[key] = int(value)
        delay = 0
        for task in os.listdir(f"/proc/{pid}/task"):
            try:
                with open(f"/proc/{pid}/task/{task}/schedstat") as file:
                    delay += int(file.read().split()[1])
            except OSError:
                pass
    except (OSError, IndexError, ValueError):
        return None
    return {
        "cpu": (utime + stime) / TICKS,
        "user": utime / TICKS,
        "system": stime / TICKS,
        "threads": threads,
        "rss": rss * PAGE,
        "read_bytes": io.get("read_bytes"),
        "write_bytes": io.get("write_bytes"),
        "voluntary": switches.get("voluntary_ctxt_switches"),
        "involuntary": switches.get("nonvoluntary_ctxt_switches"),
        "run_delay": delay / 1e9,
    }


def read_cpu() -> dict:
    with open("/proc/stat") as file:
        values = [int(x) for x in file.readline().split()[1:]]
    names = ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal"]
    return {name: values[i] / TICKS for i, name in enumerate(names) if i < len(values)}


def disk_of(path: Path) -> str | None:
    """Return the name of the block device that holds `path`."""
    try:
        device = os.stat(path).st_dev
        major, minor = os.major(device), os.minor(device)
        link = os.path.realpath(f"/sys/dev/block/{major}:{minor}")
        name = os.path.basename(link)
        # A partition: use the whole disk.
        parent = os.path.basename(os.path.dirname(link))
        if os.path.exists(f"/sys/block/{parent}"):
            return parent
        return name
    except OSError:
        return None


def read_disk(name: str | None) -> dict | None:
    if not name:
        return None
    with open("/proc/diskstats") as file:
        for line in file:
            fields = line.split()
            if fields[2] == name:
                values = [int(x) for x in fields[3:]]
                result = {
                    "reads": values[0],
                    "read_sectors": values[2],
                    "writes": values[4],
                    "write_sectors": values[6],
                    "write_ms": values[7],
                    "io_ms": values[9],
                }
                if len(values) >= 17:
                    result["flushes"] = values[15]
                    result["flush_ms"] = values[16]
                return result
    return None


class Sampler:
    def __init__(self, out: Path, disk: str | None, interval: float = 1.0):
        self.out = out
        self.disk = disk
        self.interval = interval
        self.pids: dict[str, int] = {}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True, name="sampler")

    def watch(self, name: str, pid: int) -> None:
        self.pids[name] = pid

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join()

    def sample(self) -> dict:
        return {
            "t": time.time(),
            "cpu": read_cpu(),
            "disk": read_disk(self.disk),
            "processes": {
                name: read_process(pid) for name, pid in list(self.pids.items())
            },
        }

    def _loop(self) -> None:
        with open(self.out, "a") as file:
            while not self._stop.is_set():
                file.write(json.dumps(self.sample()) + "\n")
                file.flush()
                self._stop.wait(self.interval)
            file.write(json.dumps(self.sample()) + "\n")


def load(path: Path) -> list[dict]:
    if not path.exists():
        return []
    with open(path) as file:
        return [json.loads(line) for line in file if line.strip()]


def summarize(samples: list[dict], start: float, end: float) -> dict:
    """Return the deltas of the counters between the first sample at or
    after `start` and the last sample at or before `end`, and the peaks of
    the gauges in the window."""
    window = [s for s in samples if start <= s["t"] <= end]
    if len(window) < 2:
        return {"seconds": 0.0, "samples": len(window)}
    first, last = window[0], window[-1]
    seconds = last["t"] - first["t"]
    result: dict = {"seconds": seconds, "samples": len(window), "processes": {}}
    cpu = {k: last["cpu"][k] - first["cpu"].get(k, 0) for k in last["cpu"]}
    busy = sum(v for k, v in cpu.items() if k not in ("idle", "iowait"))
    result["machine"] = {
        "cpu_busy_seconds": busy,
        "steal_seconds": cpu.get("steal", 0.0),
        "iowait_seconds": cpu.get("iowait", 0.0),
    }
    if first.get("disk") and last.get("disk"):
        result["disk"] = {
            k: last["disk"][k] - first["disk"].get(k, 0) for k in last["disk"]
        }
    for name in last["processes"]:
        a, b = first["processes"].get(name), last["processes"].get(name)
        if not a or not b:
            continue
        rss = [s["processes"][name]["rss"] for s in window if s["processes"].get(name)]
        entry = {
            "cpu_seconds": b["cpu"] - a["cpu"],
            "user_seconds": b["user"] - a["user"],
            "system_seconds": b["system"] - a["system"],
            "cpu_share": (b["cpu"] - a["cpu"]) / seconds if seconds else None,
            "rss_max": max(rss),
            "rss_end": b["rss"],
            "run_delay_seconds": b["run_delay"] - a["run_delay"],
            "threads": b["threads"],
        }
        for key in ("read_bytes", "write_bytes", "voluntary", "involuntary"):
            if a.get(key) is not None and b.get(key) is not None:
                entry[key] = b[key] - a[key]
        result["processes"][name] = entry
    return result
