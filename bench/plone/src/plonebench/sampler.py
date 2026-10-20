"""Sample the processes of a run and the machine, once a second.

For each process: user and system CPU, resident memory, the bytes read and
written by its I/O, its context switches, and the time its threads waited
for a CPU. For the machine: the CPU times and the counters of the disk
under the store. The samples go to a JSON lines file; `summarize` turns a
window of them into deltas.

For the CPUs of the Zope instances, on Linux, the sampler also reads the
clock that the hardware measured (`cpuinfo_avg_freq`, else
`scaling_cur_freq`) and the time that the core of each CPU spent
throttled (`core_throttle_total_time_ms`). `summarize` turns them into
`zope_clock_mhz` and `zope_throttle_seconds`.

For the server, on Linux, the sampler also reads the counters of the bytes
that it writes to its store (`store_counters`). `summarize` turns them into
`store_write_bytes`. The kernel counter `write_bytes` charges whole
page-cache folios, so it can be many times the bytes that reach a store.

On Linux the values come from /proc: the run delay of schedstat, steal
time, and the flushes of /proc/diskstats. On macOS they come from
`proc_pid_rusage` (the runnable time stands for the run delay) and psutil.
macOS has no steal time, no flush counter and no split of the context
switches: these keys stay absent, never 0.
"""

from __future__ import annotations

import json
import os
import re
import stat
import subprocess
import sys
import threading
import time
from pathlib import Path

DARWIN = sys.platform == "darwin"
TICKS = os.sysconf("SC_CLK_TCK")
PAGE = os.sysconf("SC_PAGE_SIZE")
SYS_CPU = Path("/sys/devices/system/cpu")


def read_process(pid: int) -> dict | None:
    if DARWIN:
        return _darwin.read_process(pid)
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
        "wchar": io.get("wchar"),
        "voluntary": switches.get("voluntary_ctxt_switches"),
        "involuntary": switches.get("nonvoluntary_ctxt_switches"),
        "run_delay": delay / 1e9,
    }


def read_sockets(pid: int) -> dict[int, int] | None:
    """Return the bytes that the process `pid` gave to each of its TCP
    sockets, by the inode of the socket: `bytes_sent` of `ss -i` without
    the retransmitted bytes (`bytes_retrans`), plus the bytes still in the
    send queue (`notsent`). `ss` omits a counter that is 0."""
    inodes = set()
    try:
        for fd in os.listdir(f"/proc/{pid}/fd"):
            try:
                link = os.readlink(f"/proc/{pid}/fd/{fd}")
            except OSError:
                continue
            if link.startswith("socket:["):
                inodes.add(int(link[len("socket:[") : -1]))
        out = subprocess.run(
            ["ss", "-H", "-t", "-n", "-i", "-e", "-O"],
            capture_output=True,
            text=True,
            check=True,
            timeout=10,
        ).stdout
    except (OSError, ValueError, subprocess.SubprocessError):
        return None
    sent = {}
    for line in out.splitlines():
        inode = re.search(r"\bino:(\d+)", line)
        if inode is None or int(inode.group(1)) not in inodes:
            continue
        sent[int(inode.group(1))] = (
            _counter(line, "bytes_sent")
            - _counter(line, "bytes_retrans")
            + _counter(line, "notsent")
        )
    return sent


def _counter(line: str, name: str) -> int:
    found = re.search(rf"\b{name}:(\d+)", line)
    return int(found.group(1)) if found else 0


def read_other_files(pid: int, store_dir: Path) -> int | None:
    """Return the summed size of the regular files that process `pid` holds
    open for writing outside `store_dir`: its logs and its resolution log."""
    # The links in /proc name the real paths.
    store_dir = store_dir.resolve()
    seen = set()
    total = 0
    try:
        fds = os.listdir(f"/proc/{pid}/fd")
    except OSError:
        return None
    for fd in fds:
        try:
            path = os.readlink(f"/proc/{pid}/fd/{fd}")
            with open(f"/proc/{pid}/fdinfo/{fd}") as file:
                flags = next(
                    int(line.split()[1], 8)
                    for line in file
                    if line.startswith("flags:")
                )
            info = os.stat(f"/proc/{pid}/fd/{fd}")
        except (OSError, StopIteration, ValueError, IndexError):
            continue
        if flags & os.O_ACCMODE == os.O_RDONLY or not stat.S_ISREG(info.st_mode):
            continue
        if Path(path).is_relative_to(store_dir):
            continue
        if (info.st_dev, info.st_ino) in seen:
            continue
        seen.add((info.st_dev, info.st_ino))
        total += info.st_size
    return total


class SentBytes:
    """The bytes that the TCP sockets of one process sent, as a counter
    that only grows.

    `ss` gives the bytes of the open sockets only. When a socket closes,
    this counter keeps the bytes that the last sample saw. The bytes that
    the socket sent after that sample are lost: at most one interval of one
    connection. The connections of the Zope instances stay open for the
    whole run."""

    def __init__(self):
        self.last: dict[int, int] = {}
        self.closed = 0

    def update(self, sockets: dict[int, int]) -> int:
        for inode, sent in self.last.items():
            now = sockets.get(inode)
            # A smaller count is a new socket on a reused inode.
            if now is None or now < sent:
                self.closed += sent
        self.last = dict(sockets)
        return self.closed + sum(sockets.values())


def store_counters(
    pid: int, store: Path, appends: bool, sent: SentBytes
) -> dict[str, int]:
    """Return the counters of the bytes that the server `pid` writes to its
    store, on Linux: the bytes that its TCP sockets sent, the size of its
    files outside the directory of the store file `store`, and, for a store
    that the server only appends to (`appends`), the size of `store`."""
    result = {}
    sockets = read_sockets(pid)
    if sockets is not None:
        result["socket_bytes_sent"] = sent.update(sockets)
    other = read_other_files(pid, store.parent)
    if other is not None:
        result["other_files_bytes"] = other
    if appends:
        # None marks a store that the server appends to, without its size.
        try:
            result["store_file_bytes"] = os.stat(store).st_size
        except OSError:
            result["store_file_bytes"] = None
    return result


def cpu_numbers(lists) -> tuple[int, ...]:
    """Return the CPUs of taskset lists such as `1`, `1,2` or `1-3`. A
    part of another form gives no CPU."""
    found = set()
    for text in lists:
        for part in str(text).split(","):
            first, _, last = part.strip().partition("-")
            try:
                found.update(range(int(first), int(last or first) + 1))
            except ValueError:
                continue
    return tuple(sorted(found))


def _read_int(path: Path) -> int | None:
    try:
        return int(path.read_text())
    except (OSError, ValueError):
        return None


def read_cpus(cpus: tuple[int, ...]) -> dict:
    """Return the clock of each CPU of `cpus` in MHz, and the time that
    its core spent throttled since the boot in ms. A value that the machine
    does not give is absent."""
    clock, throttle = {}, {}
    for cpu in cpus:
        base = SYS_CPU / f"cpu{cpu}"
        for name in ("cpuinfo_avg_freq", "scaling_cur_freq"):
            khz = _read_int(base / "cpufreq" / name)
            if khz is not None:
                clock[str(cpu)] = khz / 1000
                break
        ms = _read_int(base / "thermal_throttle" / "core_throttle_total_time_ms")
        if ms is not None:
            throttle[str(cpu)] = ms
    return {"clock_mhz": clock, "throttle_ms": throttle}


def read_cpu() -> dict:
    if DARWIN:
        import psutil

        return psutil.cpu_times()._asdict()
    with open("/proc/stat") as file:
        values = [int(x) for x in file.readline().split()[1:]]
    names = ["user", "nice", "system", "idle", "iowait", "irq", "softirq", "steal"]
    return {name: values[i] / TICKS for i, name in enumerate(names) if i < len(values)}


def disk_of(path: Path) -> str | None:
    """Return the name of the block device that holds `path`."""
    if DARWIN:
        return _darwin.disk_of(path)
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
    if DARWIN:
        return _darwin.read_disk(name)
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
    def __init__(
        self,
        out: Path,
        disk: str | None,
        interval: float = 1.0,
        zope_cpus: tuple[int, ...] = (),
    ):
        self.out = out
        self.disk = disk
        self.interval = interval
        self.zope_cpus = zope_cpus
        self.pids: dict[str, int] = {}
        self.stores: dict[str, tuple[Path, bool, SentBytes]] = {}
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._loop, daemon=True, name="sampler")

    def watch(
        self, name: str, pid: int, store: Path | None = None, appends: bool = False
    ) -> None:
        """Sample process `pid` under `name`. For a server, `store` is its
        store file, and `appends` says that the server only appends to it."""
        if store is not None and not DARWIN:
            self.stores[name] = (store, appends, SentBytes())
        self.pids[name] = pid

    def read(self, name: str, pid: int) -> dict | None:
        entry = read_process(pid)
        if entry is not None and name in self.stores:
            entry.update(store_counters(pid, *self.stores[name]))
        return entry

    def start(self) -> None:
        self._thread.start()

    def stop(self) -> None:
        self._stop.set()
        self._thread.join()

    def sample(self) -> dict:
        sample = {
            "t": time.time(),
            "cpu": read_cpu(),
            "disk": read_disk(self.disk),
            "processes": {
                name: self.read(name, pid) for name, pid in list(self.pids.items())
            },
        }
        if self.zope_cpus and not DARWIN:
            sample["zope_cpus"] = read_cpus(self.zope_cpus)
        return sample

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
    the gauges in the window.

    `store_write_bytes` of a process is the bytes that it wrote to its
    store: the growth of `store_file_bytes` for a store that the server
    only appends to, else the growth of `wchar` minus the growth of
    `socket_bytes_sent` and of `other_files_bytes`. Without these counters
    it is absent, never 0."""
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
    clocks = _clocks(window)
    if clocks:
        result["zope_clock_mhz"] = sum(clocks) / len(clocks)
    throttle = _throttle_seconds(first, last)
    if throttle is not None:
        result["zope_throttle_seconds"] = throttle
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
        for key in (
            "read_bytes",
            "write_bytes",
            "wchar",
            "socket_bytes_sent",
            "other_files_bytes",
            "voluntary",
            "involuntary",
            "switches",
        ):
            if a.get(key) is not None and b.get(key) is not None:
                entry[key] = b[key] - a[key]
        store = _store_bytes(a, b)
        if store is not None:
            entry["store_write_bytes"] = store
        result["processes"][name] = entry
    return result


def _clocks(window: list[dict]) -> list[float]:
    """Return each clock of the Zope CPUs in the samples of `window`."""
    return [
        mhz
        for sample in window
        for mhz in sample.get("zope_cpus", {}).get("clock_mhz", {}).values()
    ]


def _throttle_seconds(first: dict, last: dict) -> float | None:
    """Return the throttle time of the Zope CPUs from `first` to `last`,
    summed over the CPUs, in seconds. None when no CPU gives it in both."""
    a = first.get("zope_cpus", {}).get("throttle_ms", {})
    b = last.get("zope_cpus", {}).get("throttle_ms", {})
    cpus = [cpu for cpu in b if cpu in a]
    if not cpus:
        return None
    return sum(max(0, b[cpu] - a[cpu]) for cpu in cpus) / 1000


def zope_cpus(
    samples: list[dict], windows: list[tuple[float, float]]
) -> tuple[float | None, float | None]:
    """Return the mean clock of the Zope CPUs over the samples of
    `windows`, in MHz, and their throttle time in the windows, summed, in
    seconds. Each is None when the samples do not give it."""
    clocks: list[float] = []
    throttle = None
    for start, end in windows:
        window = [s for s in samples if start <= s["t"] <= end]
        clocks += _clocks(window)
        if len(window) >= 2:
            seconds = _throttle_seconds(window[0], window[-1])
            if seconds is not None:
                throttle = (throttle or 0.0) + seconds
    clock = round(sum(clocks) / len(clocks), 1) if clocks else None
    return clock, None if throttle is None else round(throttle, 3)


def _store_bytes(a: dict, b: dict) -> int | None:
    def growth(key):
        if a.get(key) is None or b.get(key) is None:
            return None
        return b[key] - a[key]

    # A store that the server only appends to counts by its size alone:
    # its wchar holds the writes to Data.fs.tmp too.
    if "store_file_bytes" in a or "store_file_bytes" in b:
        return growth("store_file_bytes")
    written, sent, other = (
        growth(key) for key in ("wchar", "socket_bytes_sent", "other_files_bytes")
    )
    if written is None or sent is None or other is None:
        return None
    return written - sent - other


class _Darwin:
    """The macOS counters, through libproc and psutil."""

    # struct rusage_info_v4 of <sys/resource.h>: a 16-byte UUID, then 35
    # uint64 fields.
    RUSAGE_INFO_V4 = 4
    USER, SYSTEM, RESIDENT = 0, 1, 6
    DISK_READ, DISK_WRITTEN = 16, 17
    RUNNABLE = 34
    # struct proc_taskinfo of <sys/proc_info.h>: 6 uint64, then 12 int32.
    PROC_PIDTASKINFO = 4
    CSW, THREADS = 8, 9

    def __init__(self):
        self._lib = None

    def _load(self):
        if self._lib is None:
            import ctypes
            import ctypes.util

            lib = ctypes.CDLL(ctypes.util.find_library("System"), use_errno=True)

            class Timebase(ctypes.Structure):
                _fields_ = [("numer", ctypes.c_uint32), ("denom", ctypes.c_uint32)]

            timebase = Timebase()
            lib.mach_timebase_info(ctypes.byref(timebase))
            # The times of rusage_info are in Mach absolute time units.
            self._seconds = timebase.numer / timebase.denom / 1e9
            self._ctypes = ctypes
            self._lib = lib
        return self._lib

    def read_process(self, pid: int) -> dict | None:
        lib = self._load()
        ctypes = self._ctypes
        usage = ctypes.create_string_buffer(16 + 8 * 35)
        if lib.proc_pid_rusage(pid, self.RUSAGE_INFO_V4, usage) != 0:
            return None
        fields = (ctypes.c_uint64 * 35).from_buffer(usage, 16)
        task = ctypes.create_string_buffer(6 * 8 + 12 * 4)
        size = lib.proc_pidinfo(pid, self.PROC_PIDTASKINFO, 0, task, len(task))
        ints = None
        if size == len(task):
            ints = (ctypes.c_int32 * 12).from_buffer(task, 6 * 8)
        user = fields[self.USER] * self._seconds
        system = fields[self.SYSTEM] * self._seconds
        # The runnable time counts the time on a CPU too.
        runnable = fields[self.RUNNABLE] * self._seconds
        return {
            "cpu": user + system,
            "user": user,
            "system": system,
            "threads": ints[self.THREADS] if ints else None,
            "rss": fields[self.RESIDENT],
            "read_bytes": fields[self.DISK_READ],
            "write_bytes": fields[self.DISK_WRITTEN],
            "switches": ints[self.CSW] if ints else None,
            "run_delay": max(0.0, runnable - user - system),
        }

    def disk_of(self, path: Path) -> str | None:
        """Return the physical disk under `path`: an APFS volume lives in a
        container on a physical store, and psutil counts the physical
        disks only."""
        import plistlib

        try:
            device = (
                subprocess.run(
                    ["df", str(path)], capture_output=True, text=True, check=True
                )
                .stdout.splitlines()[1]
                .split()[0]
            )
            info = plistlib.loads(
                subprocess.run(
                    ["diskutil", "info", "-plist", device],
                    capture_output=True,
                    check=True,
                ).stdout
            )
        except (OSError, subprocess.CalledProcessError, IndexError, ValueError):
            return None
        stores = info.get("APFSPhysicalStores") or []
        name = (
            stores[0].get("APFSPhysicalStore")
            if stores
            else info.get("ParentWholeDisk")
        )
        if not name:
            return None
        # "disk0s2" -> "disk0".
        match = re.match(r"disk\d+", name)
        return match.group(0) if match else None

    def read_disk(self, name: str) -> dict | None:
        import psutil

        counters = psutil.disk_io_counters(perdisk=True).get(name)
        if counters is None:
            return None
        return {
            "reads": counters.read_count,
            "read_bytes": counters.read_bytes,
            "writes": counters.write_count,
            "write_bytes": counters.write_bytes,
            "read_ms": counters.read_time,
            "write_ms": counters.write_time,
        }


_darwin = _Darwin()
