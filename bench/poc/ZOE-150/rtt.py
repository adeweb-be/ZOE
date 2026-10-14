"""The round trip of one call through each client. Not for merge.

For `zeo`, `ZEO.ClientStorage` calls `new_oids(1)` and `ping` on the ZEO
server. For `poc-main` and `poc-proto`, the POC client calls `new_oids(1)`
on `zoe serve`. Each line also gives a loopback TCP round trip between two
Python processes, the floor of any call. The times are wall time per call
and client CPU per call, median and 90th percentile of 20 batches.
"""

import argparse
import json
import os
import shutil
import socket
import statistics
import subprocess
import sys
import sysconfig
import time
from pathlib import Path
from types import SimpleNamespace

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
sys.path.insert(0, str(HERE.parent / "ZOE-077" / "rustclient" / "python"))

from driver import REPO, cpu_state, pinned, start_server, stop_server  # noqa: E402

BATCHES, CALLS = 20, 250

ECHO = """
import socket, sys
s = socket.socket(); s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
s.bind(("127.0.0.1", 0)); s.listen(1)
print(s.getsockname()[1], flush=True)
c, _ = s.accept(); c.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
while True:
    b = c.recv(64)
    if not b:
        break
    c.sendall(b)
"""


def measure(call):
    for _ in range(CALLS):
        call()
    walls, cpus = [], []
    for _ in range(BATCHES):
        w0, c0 = time.perf_counter(), time.process_time()
        for _ in range(CALLS):
            call()
        walls.append((time.perf_counter() - w0) / CALLS * 1e6)
        cpus.append((time.process_time() - c0) / CALLS * 1e6)
    walls.sort()
    cpus.sort()
    return {
        "wall_us_median": round(statistics.median(walls), 1),
        "wall_us_p90": round(walls[int(len(walls) * 0.9)], 1),
        "cpu_us_median": round(statistics.median(cpus), 1),
    }


def loopback(server_cpus):
    echo = subprocess.Popen(pinned(server_cpus, [sys.executable, "-c", ECHO]), stdout=subprocess.PIPE, text=True)
    port = int(echo.stdout.readline())
    s = socket.create_connection(("127.0.0.1", port))
    s.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)

    def call():
        s.sendall(b"x" * 16)
        s.recv(64)

    try:
        return measure(call)
    finally:
        s.close()
        echo.wait(timeout=10)


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--config", choices=["zeo", "poc-main", "poc-proto"], required=True)
    p.add_argument("--server-cpus", default="")
    p.add_argument("--client-cpus", default="")
    p.add_argument("--zeo-python", default=sys.executable)
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--results", type=Path, required=True)
    args = p.parse_args()
    if args.client_cpus:
        os.sched_setaffinity(0, {int(x) for part in args.client_cpus.split(",") for x in _span(part)})
    python = sys.version.split()[0] + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else "")
    wd = args.work / f"rtt-{python}-{args.config}"
    wd.mkdir(parents=True, exist_ok=True)
    ns = SimpleNamespace(
        config=args.config, zeo_python=args.zeo_python, server_cpus=args.server_cpus, profile=False,
        zoe_main=str(REPO / "target" / "zoe150" / "zoe-main"), zoe_proto=str(REPO / "target" / "zoe150" / "zoe-proto"),
    )
    result = {"tag": "rtt", "config": args.config, "python": python, "mode": "-", "c": 0, "size": 0,
              "scenario": "-", "round": 0, "server_cpus": args.server_cpus, "client_cpus": args.client_cpus,
              "cpu_before": cpu_state()}
    result["loopback_tcp"] = loopback(args.server_cpus)
    proc, addr, _ = start_server(ns, wd, None)
    try:
        if args.config == "zeo":
            from ZEO.ClientStorage import ClientStorage

            host, port = addr.rsplit(":", 1)
            cs = ClientStorage((host, int(port)), wait=True)
            try:
                result["new_oids"] = measure(lambda: cs._call("new_oids", 1))
                result["ping"] = measure(lambda: cs._call("ping"))
            finally:
                cs.close()
        else:
            from zoeclientx._zoeclientx import ZClient

            client = ZClient(addr)
            result["new_oids"] = measure(lambda: client.new_oids(1))
        result["ok"] = True
    finally:
        stop_server(proc)
        shutil.rmtree(wd, ignore_errors=True)
    with open(args.results, "a") as f:
        f.write(json.dumps(result) + "\n")
    print(json.dumps({k: result[k] for k in result if k in ("config", "python", "loopback_tcp", "new_oids", "ping")}))


def _span(part):
    if "-" in part:
        a, b = part.split("-")
        return range(int(a), int(b) + 1)
    return [int(part)]


if __name__ == "__main__":
    main()
