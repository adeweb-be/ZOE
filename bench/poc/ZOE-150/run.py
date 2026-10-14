"""The full run of the ZOE-150 spike. Not for merge.

Run it in devenv shell, after setup.sh, on a machine that runs nothing
else. It appends one JSON line per run to OUT and skips the runs that OUT
already holds, so a stopped run starts again where it stopped.

Phases, in this order:

- `machine`: the CPU, the governor, the kernel, the disk of the work dir.
- `fsync`: the cost of a sync on the disk of the work dir (fsyncx.py).
- `gil`: on each interpreter, the modules that turn the GIL back on.
- `rtt`: the round trip of one call through each client, and of loopback TCP.
- `pyspy`: py-spy samples of `ZEO.ClientStorage` in `read_hot`.
- `floor`: each cell and scenario once, in the process, with no server.
- `main`: each cell and scenario in interleaved rounds: zeo, poc-main,
  poc-proto, zeo, poc-main, poc-proto, ... A run that other load disturbed
  runs again, and both stay in OUT.
- `spans`: each cell and scenario once on CPython 3.13 for poc-main and
  poc-proto, with the OTLP spans of the server and the counters of the
  prototypes.
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
SCENARIOS = ["add", "update", "cold", "warm", "hot"]
CONFIGS = ["zeo", "poc-main", "poc-proto"]
PHASES = ["machine", "fsync", "gil", "rtt", "pyspy", "floor", "main", "spans"]


def venv_python(py):
    return str(REPO / "target" / "bench" / f"venv-{py}" / "bin" / "python")


def parse_cell(text):
    mode, c, size = text.split(":")
    return mode, int(c), int(size) * KB


def done_keys(out):
    keys = set()
    if out.exists():
        for line in out.read_text().splitlines():
            try:
                r = json.loads(line)
            except ValueError:
                continue
            if r.get("ok"):
                keys.add(key(r["tag"], r["python"], r["config"], r["mode"], r["c"], r["size"], r["scenario"], r["round"]))
    return keys


def key(tag, python, config, mode, c, size, scenario, rnd):
    names = {"cold": "read_cold", "warm": "read_warm", "hot": "read_hot"}
    return (tag, python, config, mode, c, size, names.get(scenario, scenario), rnd)


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

    def driver(self, py, config, mode, c, size, scenario, rnd, tag="main", extra=()):
        a = self.args
        k = key(tag, self.labels[py], config, mode, c, size, scenario, rnd)
        if k in self.done:
            return None
        attempt = 0
        while True:
            attempt += 1
            cmd = [
                venv_python(py), str(HERE / "driver.py"), "--config", config, "--mode", mode,
                "-c", str(c), "-s", str(size), "--scenario", scenario, "--round", str(rnd),
                "--attempt", str(attempt), "--tag", tag, "--pool", str(a.pool),
                "--server-cpus", a.server_cpus, "--client-cpus", a.client_cpus,
                "--zeo-python", venv_python("3.13"), "--work", str(a.work), "--results", str(a.out),
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
    info = {
        "lscpu": subprocess.run(["lscpu"], capture_output=True, text=True, env={**os.environ, "LC_ALL": "C"}).stdout,
        "kernel": os.uname().release,
        "mem_gib": round(os.sysconf("SC_PAGE_SIZE") * os.sysconf("SC_PHYS_PAGES") / 2**30, 1),
        "findmnt": subprocess.run(["findmnt", "-T", str(a.work)], capture_output=True, text=True).stdout,
        "lsblk": subprocess.run(["lsblk", "-o", "NAME,ROTA,MODEL,SIZE,TYPE"], capture_output=True, text=True).stdout,
        "disk_free": shutil.disk_usage(a.work).free,
        "server_cpus": a.server_cpus, "client_cpus": a.client_cpus, "pool": a.pool,
        "pythons": r.labels, "binaries": (REPO / "target" / "zoe150" / "binaries.txt").read_text(),
        "commit": subprocess.run(["git", "-C", str(REPO), "rev-parse", "HEAD"], capture_output=True, text=True).stdout.strip(),
    }
    sys.path.insert(0, str(HERE))
    from driver import cpu_state

    info["cpu"] = cpu_state()
    (a.out.parent / "machine.json").write_text(json.dumps(info, indent=1) + "\n")
    log("machine.json written")


def phase_fsync(r):
    a = r.args
    target = a.out.parent / "fsync.txt"
    if target.exists():
        return
    done = subprocess.run(
        [venv_python("3.13"), str(REPO / "bench" / "poc" / "ZOE-077" / "fsyncx.py"), str(a.work / "fsyncx.bin")],
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
        for config in CONFIGS:
            if key("rtt", r.labels[py], config, "-", 0, 0, "-", 0) in r.done:
                continue
            done = subprocess.run(
                [venv_python(py), str(HERE / "rtt.py"), "--config", config, "--server-cpus", a.server_cpus,
                 "--client-cpus", a.client_cpus, "--zeo-python", venv_python("3.13"), "--work", str(a.work),
                 "--results", str(a.out)],
                capture_output=True, text=True,
            )
            log(f"rtt {r.labels[py]} {config}: {done.stdout.strip()} {done.stderr.strip()[-500:]}")


def phase_pyspy(r):
    # py-spy does not read the frames of a free-threaded 3.14 interpreter,
    # so the profile runs on 3.13 only. Five values give enough samples.
    a = r.args
    spy = REPO / "target" / "zoe150" / "tools" / "bin" / "py-spy"
    py = "3.13"
    for c in (1, 16):
        out = a.out.parent / f"pyspy-{r.labels[py]}-zeo-read_hot-threads-c{c}.txt"
        wrap = f"{HERE / 'wrap-pyspy.sh'} {spy} {out}"
        r.driver(py, "zeo", "threads", c, KB, "hot", 1, tag="pyspy", extra=("--client-wrap", wrap, "--values", "5"))


def phase_floor(r):
    for py in r.args.pythons:
        for mode, c, size in r.cells():
            for scenario in SCENARIOS:
                if size > KB and c >= 64 and scenario in ("add", "update"):
                    # The DemoStorage keeps every write in memory: about
                    # 40 GB here, more than the memory of the machine.
                    continue
                r.driver(py, "floor", mode, c, size, scenario, 1, tag="floor")


def phase_main(r):
    a = r.args
    for py in a.pythons:
        for mode, c, size in r.cells():
            for scenario in SCENARIOS:
                for rnd in range(1, a.rounds + 1):
                    for config in a.configs:
                        r.driver(py, config, mode, c, size, scenario, rnd)


def phase_spans(r):
    py = "3.13" if "3.13" in r.args.pythons else r.args.pythons[0]
    for mode, c, size in r.cells():
        for scenario in SCENARIOS:
            for config in ("poc-main", "poc-proto"):
                r.driver(py, config, mode, c, size, scenario, 1, tag="spans", extra=("--spans", "--profile"))


def main():
    p = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    p.add_argument("--out", type=Path, default=HERE / "results" / "full.jsonl")
    p.add_argument("--work", type=Path, default=REPO / "target" / "zoe150" / "work")
    p.add_argument("--pythons", nargs="+", default=["3.13", "3.14t"])
    p.add_argument("--configs", nargs="+", default=CONFIGS)
    p.add_argument("--rounds", type=int, default=3)
    p.add_argument("--cells", nargs="*", help="cells as MODE:CONCURRENCY:KB, for example threads:4:1 (default: the full set)")
    p.add_argument("--phases", nargs="+", default=PHASES, choices=PHASES)
    p.add_argument("--pool", type=int, default=16)
    p.add_argument("--server-cpus", default="0-5")
    p.add_argument("--client-cpus", default="6-19")
    p.add_argument("--foreign-limit", type=float, default=1.0, help="the cores of other load that make a run disturbed")
    p.add_argument("--attempts", type=int, default=2)
    args = p.parse_args()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.work.mkdir(parents=True, exist_ok=True)
    r = Runner(args)
    log(f"pythons {r.labels}; {len(r.done)} runs already in {args.out}")
    for name in args.phases:
        log(f"== {name}")
        globals()[f"phase_{name}"](r)
    log("DONE")


if __name__ == "__main__":
    main()
