"""The reduced run of the ZOE-150 spike. Not for merge.

Run it in devenv shell, after setup.sh, on a machine that runs nothing
else. It appends one JSON line per run to OUT and skips the runs that OUT
already holds, so a stopped run starts again where it stopped.

One run is one cell (mode, concurrency, object size) of one configuration
on one interpreter: a fresh store, and the five scenarios of zodbshootout
in one process (driver.py, shoot.py). Phases, in this order:

- `machine`: the CPU, the governor, the kernel, the disk of the work dir,
  and the load of the machine with no run.
- `fsync`: the cost of a sync on the disk of the work dir (fsyncx.py).
- `gil`: on each interpreter, whether the stacks of the runs turn the GIL
  back on.
- `rtt`: the round trip of one call through each client, and of loopback TCP.
- `pyspy`: py-spy samples of `ZEO.ClientStorage` in `read_hot` (3.13 only).
- `floor`: each cell once, in the process, with no server.
- `main`: each cell in interleaved rounds: zeo, poc-proto, then again. A
  run that other load disturbed runs again, and both stay in OUT.
- `spans`: each cell once on CPython 3.13 for poc-proto, with the OTLP
  spans of the server and the counters of the prototypes.
- `ref`: each cell in --main-rounds rounds of poc-main, `zoe serve` as on
  main.

`spans` and `ref` are optional: a run of them starts only while the time
since the start is under --budget seconds.
"""

import argparse
import json
import os
import shutil
import subprocess
import sys
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
KB = 1024
CELLS = [
    *(("threads", c, s) for s in (KB, 100 * KB) for c in (1, 4, 16, 64)),
    *(("processes", c, s) for s in (KB, 100 * KB) for c in (4, 16, 64)),
]
PHASES = ["machine", "fsync", "gil", "rtt", "pyspy", "floor", "main", "spans", "ref"]
# These phases run only while the run is inside its budget.
OPTIONAL = {"ref", "spans"}

sys.path.insert(0, str(HERE))
from driver import busy_seconds, cpu_state, default_loops, kernel_seconds  # noqa: E402


def venv_python(py):
    return str(REPO / "target" / "bench" / f"venv-{py}" / "bin" / "python")


def parse_cell(text):
    mode, c, size = text.split(":")
    return mode, int(c), int(size) * KB


def key(tag, python, config, mode, c, size, rnd):
    return (tag, python, config, mode, c, size, rnd)


def done_keys(out):
    keys = set()
    if out.exists():
        for line in out.read_text().splitlines():
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("ok"):
                keys.add(key(r["tag"], r["python"], r["config"], r["mode"], r["c"], r["size"], r["round"]))
    return keys


def disk_needed(c, size):
    """A bound of the bytes of one run, as shootout.disk_needed counts them."""
    n, loops = 100, default_loops(c)
    # The populate, the root and the mappings, add and update, read_warm.
    records = (c * n + n) + (c + 3) + 2 * c * loops * (n + 1) + c * n
    return records * (size + 8 * KB) + 64 * KB * KB


def python_label(py):
    out = subprocess.run(
        [venv_python(py), "-c", "import sys, sysconfig; print(sys.version.split()[0] + ('t' if sysconfig.get_config_var('Py_GIL_DISABLED') else ''))"],
        capture_output=True, text=True, check=True,
    )
    return out.stdout.strip()


def log(msg):
    print(f"[{time.strftime('%H:%M:%S')}] {msg}", flush=True)


class Runner:
    def __init__(self, args):
        self.args = args
        self.done = done_keys(args.out)
        self.labels = {py: python_label(py) for py in args.pythons}
        self.started = time.monotonic()
        self.optional = False

    def driver(self, py, config, mode, c, size, rnd, tag="main", extra=()):
        a = self.args
        k = key(tag, self.labels[py], config, mode, c, size, rnd)
        if k in self.done:
            return None
        if self.optional and time.monotonic() - self.started > a.budget:
            log(f"SKIPPED, over the budget of {a.budget} s: {k}")
            return None
        needed, free = disk_needed(c, size), shutil.disk_usage(a.work).free
        if free < needed:
            # The run goes on with the other cells. A later start of run.py
            # runs this one when the disk has room.
            log(f"SKIPPED, not enough free disk: {k}: {needed} bytes needed, {free} free")
            return None
        attempt = 0
        while True:
            attempt += 1
            cmd = [
                venv_python(py), str(HERE / "driver.py"), "--config", config, "--mode", mode,
                "-c", str(c), "-s", str(size), "--round", str(rnd),
                "--attempt", str(attempt), "--tag", tag, "--pool", str(a.pool),
                "--server-cpus", a.server_cpus, "--client-cpus", a.client_cpus,
                # The ZEO server runs on the interpreter of the client, as in
                # the harness.
                "--zeo-python", venv_python(py), "--work", str(a.work), "--results", str(a.out),
                *extra,
            ]
            done = subprocess.run(cmd, capture_output=True, text=True)
            line = done.stdout.strip().splitlines()[-1] if done.stdout.strip() else done.stderr[-500:]
            log(line)
            last = json.loads(a.out.read_text().splitlines()[-1])
            disturbed = last.get("foreign_cores", 0) > a.foreign_limit
            if last.get("ok") and not disturbed:
                self.done.add(k)
                return last
            if attempt >= a.attempts:
                log(f"giving up after {attempt} attempts: {k}")
                return last
            log(f"again: {'disturbed' if last.get('ok') else 'failed'} {k}")

    def cells(self):
        return [parse_cell(x) for x in self.args.cells] if self.args.cells else CELLS


def phase_machine(r):
    a = r.args
    target = a.out.parent / "machine.json"
    if target.exists():
        return
    info = {
        "lscpu": subprocess.run(["lscpu"], capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"}).stdout,
        "kernel": os.uname().release,
        "mem_gib": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30, 1),
        "findmnt": subprocess.run(["findmnt", "-T", str(a.work)], capture_output=True, text=True).stdout,
        "lsblk": subprocess.run(["lsblk", "-o", "NAME,ROTA,MODEL,SIZE,TYPE,MOUNTPOINT"], capture_output=True, text=True).stdout,
        "disk_free": shutil.disk_usage(a.work).free,
        "server_cpus": a.server_cpus, "client_cpus": a.client_cpus, "pool": a.pool,
        "pythons": r.labels, "binaries": (REPO / "target" / "zoe150" / "binaries.txt").read_text(),
        "commit": subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
        "cpu": cpu_state(),
    }
    # The load of the machine with no run: the base of foreign_cores.
    b0, k0 = busy_seconds(), kernel_seconds()
    time.sleep(5)
    info["idle_cores"] = round(((busy_seconds() - b0) - (kernel_seconds() - k0)) / 5, 2)
    target.write_text(json.dumps(info, indent=1) + "\n")
    log(f"machine.json written; idle load {info['idle_cores']} cores")


def phase_fsync(r):
    a = r.args
    target = a.out.parent / "fsync.txt"
    if target.exists():
        return
    done = subprocess.run(
        [venv_python(a.pythons[0]), str(HERE / "fsyncx.py"), str(a.work / "fsyncx.bin")],
        capture_output=True, text=True, check=True,
    )
    target.write_text(done.stdout)
    log("fsync:\n" + done.stdout)


def phase_gil(r):
    a = r.args
    for py in a.pythons:
        target = a.out.parent / f"gil-{r.labels[py]}.json"
        if target.exists():
            continue
        done = subprocess.run(
            [venv_python(py), "-W", "always", str(HERE / "gil.py")],
            capture_output=True, text=True, env={**os.environ, "PYTHONPATH": str(HERE.parent / "ZOE-077" / "rustclient" / "python")},
        )
        target.write_text(done.stdout)
        log(f"gil {r.labels[py]}: {done.stdout.strip()} {done.stderr.strip()[-300:]}")


def phase_rtt(r):
    a = r.args
    for py in a.pythons:
        for config in ("zeo", "poc-proto"):
            if key("rtt", r.labels[py], config, "-", 0, 0, 0) in r.done:
                continue
            done = subprocess.run(
                [venv_python(py), str(HERE / "rtt.py"), "--config", config, "--server-cpus", a.server_cpus,
                 "--client-cpus", a.client_cpus, "--zeo-python", venv_python(py), "--work", str(a.work),
                 "--results", str(a.out)],
                capture_output=True, text=True,
            )
            log(f"rtt {r.labels[py]} {config}: {done.stdout.strip()} {done.stderr.strip()[-500:]}")


def phase_pyspy(r):
    # py-spy does not read the frames of a free-threaded 3.14 interpreter,
    # so the profile runs on 3.13 only.
    a = r.args
    if "3.13" not in a.pythons:
        return
    spy = REPO / "target" / "zoe150" / "tools" / "bin" / "py-spy"
    for c, loops in ((1, 300), (16, 30)):
        out = a.out.parent / f"pyspy-{r.labels['3.13']}-zeo-read_hot-threads-c{c}.txt"
        wrap = f"{HERE / 'wrap-pyspy.sh'} {spy} {out}"
        r.driver("3.13", "zeo", "threads", c, KB, 1, tag="pyspy",
                 extra=("--client-wrap", wrap, "--scenarios", "add", "hot", "--inner-loops", str(loops)))


def phase_floor(r):
    for py in r.args.pythons:
        for mode, c, size in r.cells():
            r.driver(py, "floor", mode, c, size, 1, tag="floor")


def phase_main(r):
    a = r.args
    for py in a.pythons:
        for mode, c, size in r.cells():
            for rnd in range(1, a.rounds + 1):
                for config in ("zeo", "poc-proto"):
                    r.driver(py, config, mode, c, size, rnd)


def phase_ref(r):
    a = r.args
    for rnd in range(1, a.main_rounds + 1):
        for py in a.pythons:
            for mode, c, size in r.cells():
                r.driver(py, "poc-main", mode, c, size, rnd)


def phase_spans(r):
    if "3.13" not in r.args.pythons:
        return
    for mode, c, size in r.cells():
        r.driver("3.13", "poc-proto", mode, c, size, 1, tag="spans", extra=("--spans", "--profile"))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", type=Path, default=HERE / "results" / "run.jsonl")
    p.add_argument("--work", type=Path, default=REPO / "target" / "zoe150" / "work")
    p.add_argument("--pythons", nargs="+", default=["3.13", "3.14t"])
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--main-rounds", type=int, default=1, help="the rounds of poc-main")
    p.add_argument("--cells", nargs="*", help="cells as MODE:CONCURRENCY:KB, for example threads:4:1 (default: the full set)")
    p.add_argument("--phases", nargs="+", default=PHASES, choices=PHASES)
    p.add_argument("--pool", type=int, default=16)
    p.add_argument("--server-cpus", default=os.environ.get("ZOE150_SERVER_CPUS", ""))
    p.add_argument("--client-cpus", default=os.environ.get("ZOE150_CLIENT_CPUS", ""))
    p.add_argument("--foreign-limit", type=float, default=1.5, help="the cores of other load that make a run disturbed")
    p.add_argument("--attempts", type=int, default=2)
    p.add_argument("--budget", type=float, default=840, help="the seconds after which the optional phases stop")
    args = p.parse_args()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.work.mkdir(parents=True, exist_ok=True)
    r = Runner(args)
    started = r.started
    log(f"pythons {r.labels}; {len(r.done)} runs already in {args.out}")
    for name in args.phases:
        t = time.monotonic()
        log(f"== {name}")
        r.optional = name in OPTIONAL
        globals()[f"phase_{name}"](r)
        log(f"== {name} took {time.monotonic() - t:.0f} s")
    log(f"DONE in {time.monotonic() - started:.0f} s")


if __name__ == "__main__":
    main()
