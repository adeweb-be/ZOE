"""POC ZOE-187 (not for merge): focused zodbshootout runs of read_hot.

Each value runs as `_run_value` of the harness runs it: a fresh store and
server, one zodbshootout run with the options of the harness. The driver
only adds, per run:

- `--cache-size`: a `cache-size` key in the client section (both clients);
- `--trace`: the trace of `startup/sitecustomize.py`, for profiling runs;
- `--diag same|copy`: the diagnostic in-memory loadBefore;
- `--env K=V`: a variable for the worker processes (passed through pyperf).

The values of the storages alternate, in the order A B B A ..., so a drift
of the machine falls on both. For zoe-native, the driver counts the loads of
`zoe serve` (`zoe_loads_total`) over the run. It prints one JSON line per
value and appends it to `--out`.

    target/bench/venv-3.14t/bin/python bench/poc/ZOE-187/drive.py \
        --cells processes:16:102400 --storages zeo zoe-native --values 5
"""

import argparse
import contextlib
import json
import os
import shutil
import sys
import tempfile
import time
from pathlib import Path

import shootout
from shootout import gil, storages

HERE = Path(__file__).resolve().parent
STARTUP = HERE / "startup"
POC_VARS = (
    "ZOE187_TRACE", "ZOE187_DIAG", "ZOE187_BARRIER", "ZOE187_PIN", "ZOE187_TERMS",
)


def cell_of(text):
    mode, c, size = text.split(":")
    return shootout.Cell(mode, int(c), int(size))


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--cells", nargs="+", required=True, type=cell_of)
    p.add_argument("--storages", nargs="+", default=["zeo", "zoe-native"])
    p.add_argument("--values", type=int, default=3)
    p.add_argument("--bench", default="hot")
    p.add_argument("--cache-size")
    p.add_argument("--trace", type=Path)
    p.add_argument("--diag", choices=["same", "copy"])
    p.add_argument("--env", action="append", default=[])
    p.add_argument("--barrier", action="store_true", help="workers start their loops together")
    p.add_argument("--pin", help="the CPUs of the workers, such as 0-11")
    p.add_argument("--terms", action="store_true", help="time the terms of a hit")
    p.add_argument("--label", default="")
    p.add_argument("--python", help="the interpreter of zodbshootout and of the ZEO server")
    p.add_argument("--out", type=Path)
    p.add_argument("--workdir", type=Path, default=Path("target/bench/work/zoe187"))
    a = p.parse_args()

    extra = dict(kv.split("=", 1) for kv in a.env)
    if a.python:
        # _run_value and the ZEO server start sys.executable.
        sys.executable = a.python
    if a.trace:
        a.trace.mkdir(parents=True, exist_ok=True)
    if a.diag:
        extra["ZOE187_DIAG"] = a.diag
    if a.pin:
        extra["ZOE187_PIN"] = a.pin
    if a.terms:
        extra["ZOE187_TERMS"] = "1"

    # The workers of pyperf get only the variables of gil.INHERIT.
    gil.INHERIT = tuple(gil.INHERIT) + POC_VARS + tuple(extra)
    environ = gil.environ

    def poc_environ(records):
        env = environ(records)
        env["PYTHONPATH"] = env["PYTHONPATH"] + os.pathsep + str(STARTUP)
        env.update(extra)
        if a.trace:
            env["ZOE187_TRACE"] = str(a.trace_dir)
        if a.barrier:
            env["ZOE187_BARRIER"] = f"{a.barrier_dir}:{a.barrier_count}"
        return env

    gil.environ = poc_environ

    real_zconfig = storages.zconfig
    loads = {}

    @contextlib.contextmanager
    def poc_zconfig(store, workdir, dsn):
        with real_zconfig(store, workdir, dsn) as text:
            if a.cache_size:
                for end in ("</zoeclient>", "</zeoclient>"):
                    text = text.replace(end, f"cache-size {a.cache_size}\n  {end}")
            admin = None
            if store.name == "zoe-native":
                admin = storages.admin_address(workdir)
                loads["before"] = storages.server_loads(admin)
            yield text
            if admin:
                loads["after"] = storages.server_loads(admin)

    shootout.zconfig = poc_zconfig

    a.workdir.mkdir(parents=True, exist_ok=True)
    root = Path(tempfile.mkdtemp(prefix="drive-", dir=a.workdir))
    try:
        for value in range(a.values):
            order = a.storages if value % 2 == 0 else list(reversed(a.storages))
            for cell in a.cells:
                for name in order:
                    store = storages.STORAGES[name]
                    valuedir = root / f"{name}-{cell.mode}-{cell.concurrency}-{cell.object_size}-{value}"
                    valuedir.mkdir()
                    loads.clear()
                    a.trace_dir = None
                    if a.trace:
                        a.trace_dir = a.trace / valuedir.name
                        a.trace_dir.mkdir()
                    a.barrier_dir = valuedir / "barrier"
                    # The warmup and the value each pass the barriers once.
                    a.barrier_count = cell.concurrency
                    started = time.time()
                    times, workers = shootout._run_value(store, cell, a.bench, valuedir, None)
                    log = (valuedir / "zodbshootout.log").read_text(errors="replace")
                    if " ERROR " in log:
                        raise RuntimeError(f"zodbshootout logged an error in {valuedir}")
                    mean = sum(times) / len(times)
                    row = {
                        "label": a.label, "storage": name, "python": a.python or "3.14t", "mode": cell.mode,
                        "c": cell.concurrency, "size": cell.object_size,
                        "value": value, "seconds": mean,
                        "ops": cell.concurrency * 100 / mean,
                        "gil": sorted({w["gil_enabled"] for w in workers}),
                        "server_loads": (loads["after"] - loads["before"]) if loads else None,
                        "cache_size": a.cache_size, "diag": a.diag, "env": extra,
                        "wall": round(time.time() - started, 1),
                        "loadavg": open("/proc/loadavg").read().split()[:3],
                    }
                    print(json.dumps(row), flush=True)
                    if a.out:
                        with a.out.open("a") as out:
                            out.write(json.dumps(row) + "\n")
                    if not os.environ.get("ZOE187_KEEP"):
                        shutil.rmtree(valuedir)
    finally:
        if not os.environ.get("ZOE187_KEEP"):
            shutil.rmtree(root, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
