"""Run clientx.py (ZEO.ClientStorage) and clientx (zoe-client) on fresh servers. Not for merge.

Each run starts a new server with its store on POC_WORK, runs one client
in one shape, stops the server, and appends the JSON line of the client,
with the server and its toggles, to --results.

    python client-ab.py --samples samples.bin --clientx target/release/clientx --rounds 2
"""

import argparse
import json
import os
import shutil
import signal
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import driver

HERE = Path(__file__).resolve().parent
BEST = ["ZOE_POC_PROFILE=1", "ZOE_POC_STORECOMPRESS=1", "ZOE_POC_TIDONLY=1"]
SERVERS = {
    "zeo-msgpack": ("zeo-msgpack", []),
    "zoe-base": ("zoe", ["ZOE_POC_PROFILE=1"]),
    "zoe-best": ("zoe", BEST),
    "zoe-best-zstd1": ("zoe", [*BEST, "ZOE_POC_COMPRESSION=zstd:1"]),
    "zoe-best-nocomp": ("zoe", [*BEST, "ZOE_POC_COMPRESSION=none"]),
    # Compression at the vote, on several threads, instead of at storea.
    "zoe-par8": ("zoe", ["ZOE_POC_PROFILE=1", "ZOE_POC_TIDONLY=1", "ZOE_POC_PARCOMPRESS=8"]),
    "zoe-par4": ("zoe", ["ZOE_POC_PROFILE=1", "ZOE_POC_TIDONLY=1", "ZOE_POC_PARCOMPRESS=4"]),
}


def run(args, server, client, shared):
    kind, env = SERVERS[server]
    wd = driver.WORK / f"clientx-{server}-{client}-{'shared' if shared else 'separate'}"
    shutil.rmtree(wd, ignore_errors=True)
    wd.mkdir(parents=True)
    ns = SimpleNamespace(server=kind, env=env, binary=args.zoe, wrap="", serve_arg=[])
    proc, addr, _ = driver.start(ns, wd)
    try:
        common = ["--server", addr, "--workers", str(args.workers), "--txns", str(args.txns),
                  "--samples", args.samples, "--server-pid", str(proc.pid), "--label", server]
        if shared:
            common.append("--shared")
        if client == "python":
            cmd = [sys.executable, str(HERE / "clientx.py"), *common]
        else:
            cmd = [args.clientx, *common, "--threads", str(args.threads)]
        out = subprocess.run(cmd, capture_output=True, text=True, check=False)
        if out.returncode:
            raise SystemExit(out.stderr[-3000:])
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
        err = wd / "server.err"
        poc = [x for x in err.read_text().splitlines() if x.startswith("POC ")] if err.exists() else []
        if os.environ.get("POC_CLEANUP"):
            shutil.rmtree(wd, ignore_errors=True)
    line = json.loads(out.stdout.strip().splitlines()[-1])
    line["server"] = server
    line["env"] = env
    if poc:
        # The counters of the whole run: add, update and load.
        line["poc"] = json.loads(poc[-1][4:])
    return line


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--samples", required=True)
    p.add_argument("--clientx", required=True)
    p.add_argument("--zoe", default=os.environ.get("POC_ZOE"))
    p.add_argument("--servers", nargs="+", default=list(SERVERS))
    p.add_argument("--clients", nargs="+", default=["python", "rust"])
    p.add_argument("--shapes", nargs="+", default=["shared", "separate"])
    p.add_argument("--workers", type=int, default=16)
    p.add_argument("--txns", type=int, default=20)
    p.add_argument("--threads", type=int, default=4, help="tokio worker threads of clientx")
    p.add_argument("--rounds", type=int, default=1)
    p.add_argument("--results", default=str(HERE / "results" / "client-ab.jsonl"))
    args = p.parse_args()
    for _ in range(args.rounds):
        for shape in args.shapes:
            for server in args.servers:
                for client in args.clients:
                    line = run(args, server, client, shape == "shared")
                    with open(args.results, "a") as f:
                        f.write(json.dumps(line) + "\n")
                    steps = "  ".join(
                        f"{k} begin {line[k]['begin_ms']:.2f} store {line[k]['store_ms']:.2f} vote {line[k]['vote_ms']:.2f} finish {line[k]['finish_ms']:.2f} ms"
                        for k in ("add", "update") if "begin_ms" in line[k])
                    print(f"{shape:8} {server:15} {client:7} "
                          + "  ".join(f"{k} {line[k]['objects_per_s']:>7} obj/s cli {line[k]['client_us_per_object']:>6} srv {line[k]['server_us_per_object']:>6} µs/obj"
                                      for k in ("add", "update", "load"))
                          + (f"\n{'':33}{steps}" if steps else ""), flush=True)


if __name__ == "__main__":
    main()
