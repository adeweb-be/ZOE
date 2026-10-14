"""Quick shootout driver for the ZOE-077 experiments.

Start one server (zeo, or a zoe binary with POC env toggles), run zodbshootout
on one cell, and print one JSON line: ops/s and mean per scenario, server CPU
seconds, and the deltas of the zoe metrics.
"""

import argparse
import json
import os
import re
import shutil
import signal
import socket
import subprocess
import sys
import sysconfig
import time
import urllib.request
from pathlib import Path

import pyperf

ROOT = Path(__file__).resolve().parent
WORK = Path(os.environ.get("POC_WORK", ROOT / "work"))
NAMES = {
    "add": "add",
    "update": "update",
    "cold": "read_cold",
    "warm": "read_warm",
    "hot": "read_hot",
}


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def cpu_seconds(pid):
    # utime + stime of the process and its threads, in clock ticks.
    fields = Path(f"/proc/{pid}/stat").read_text().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / os.sysconf("SC_CLK_TCK")


def io(pid):
    out = {}
    for line in Path(f"/proc/{pid}/io").read_text().splitlines():
        k, v = line.split(":")
        out[k] = int(v)
    return out


def timerslack(ns):
    """Return a preexec_fn that sets the timer slack of the child, or None."""
    if ns is None:
        return None

    def set_slack():
        import ctypes

        # PR_SET_TIMERSLACK. The value stays after exec.
        ctypes.CDLL(None).prctl(29, ns, 0, 0, 0)

    return set_slack


def tree_cpu(pid):
    """Return the CPU seconds of a process, of its live children and of the
    children that it waited for. PostgreSQL runs a process per connection."""
    tick = os.sysconf("SC_CLK_TCK")

    def stat(p):
        return Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()

    own = stat(pid)
    total = sum(int(own[i]) for i in (11, 12, 13, 14))
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = stat(entry.name)
        except OSError:
            continue
        if int(fields[1]) == pid:
            total += sum(int(fields[i]) for i in (11, 12, 13, 14))
    return total / tick


def server_cpu(proc):
    return tree_cpu(proc.pid) if proc else 0.0


def server_io(proc):
    try:
        return io(proc.pid) if proc else {"write_bytes": 0, "syscw": 0}
    except OSError:
        # The processes of a container belong to another user.
        return {"write_bytes": 0, "syscw": 0}


PG_IMAGE = "postgres:17.10-alpine"


class Postgres:
    """PostgreSQL in the container of the CI baseline, on the host network,
    with its data on tmpfs. It acts as the Popen of a server."""

    def __init__(self, port):
        self.port = port
        self.name = f"zoe-poc-077-pg-{port}"
        subprocess.run(
            ["docker", "run", "-d", "--rm", "--name", self.name, "--network", "host",
             "--tmpfs", "/var/lib/postgresql/data:rw",
             "-e", "POSTGRES_HOST_AUTH_METHOD=trust", "-e", "POSTGRES_DB=zoe_bench",
             PG_IMAGE, "-c", f"port={port}", "-c", "listen_addresses=127.0.0.1"],
            check=True, capture_output=True)
        self.pid = int(subprocess.run(
            ["docker", "inspect", "-f", "{{.State.Pid}}", self.name],
            check=True, capture_output=True, text=True).stdout)

    def send_signal(self, sig):
        subprocess.run(["docker", "stop", "-t", "10", self.name], capture_output=True)

    def wait(self, timeout=None):
        return 0

    def kill(self):
        subprocess.run(["docker", "kill", self.name], capture_output=True)

    def poll(self):
        return None


def metrics(admin):
    if not admin:
        return {}
    text = urllib.request.urlopen(f"http://{admin}/metrics", timeout=5).read().decode()
    out = {}
    for line in text.splitlines():
        m = re.match(r'^(zoe_\w+?)(?:\{[^}]*\})?\s+([0-9.eE+-]+)$', line)
        if m and (m.group(1).endswith(("_sum", "_count", "_total"))):
            out[m.group(1)] = out.get(m.group(1), 0.0) + float(m.group(2))
    return out


def start(args, wd):
    if args.server == "mapping":
        return None, None, None
    if args.server == "relstorage":
        import psycopg2

        pg = Postgres(free_port())
        dsn = f"host=127.0.0.1 port={pg.port} dbname=zoe_bench user=postgres"
        # The entrypoint first runs a server without TCP for its set-up. A
        # TCP connection reaches the final server.
        for _ in range(600):
            try:
                psycopg2.connect(dsn).close()
                return pg, dsn, None
            except psycopg2.OperationalError:
                time.sleep(0.05)
        pg.kill()
        raise RuntimeError("postgres did not listen")
    env = {k: v for k, v in os.environ.items() if not k.startswith(("ZOE_", "ZEO_"))}
    for kv in args.env:
        k, v = kv.split("=", 1)
        env[k] = v
    log = open(wd / "server.err", "wb")
    if args.server.startswith("zeo"):
        port = free_port()
        if args.server == "zeo-msgpack":
            env["ZEO_MSGPACK"] = "1"
        proc = subprocess.Popen(
            [os.environ.get("POC_ZEO_PYTHON", sys.executable), "-m", "ZEO.runzeo", "-a", f"127.0.0.1:{port}", "-f", str(wd / "Data.fs")],
            stdout=log, stderr=subprocess.STDOUT, env=env,
        )
        for _ in range(200):
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.1).close()
                return proc, f"127.0.0.1:{port}", None
            except OSError:
                time.sleep(0.05)
        raise RuntimeError("zeo did not listen")
    out = wd / "server.out"
    cmd = [*args.wrap.split(), args.binary, "serve", "--path", str(wd / "store"), "--listen", "127.0.0.1:0",
           "--admin-listen", "127.0.0.1:0", *args.serve_arg]
    proc = subprocess.Popen(cmd, stdout=open(out, "wb"), stderr=log, env=env)
    for _ in range(200):
        text = out.read_text() if out.exists() else ""
        listen = re.search(r"^listening (\S+)", text, re.M)
        admin = re.search(r"^admin (\S+)", text, re.M)
        if listen and admin:
            return proc, listen.group(1), admin.group(1)
        if proc.poll() is not None:
            raise RuntimeError((wd / "server.err").read_text())
        time.sleep(0.05)
    raise RuntimeError("zoe did not listen")


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--server", choices=["zeo", "zeo-msgpack", "zoe", "mapping", "relstorage"], required=True)
    p.add_argument("--history-free", action="store_true", help="RelStorage with keep-history false")
    p.add_argument("--binary")
    p.add_argument("--label", required=True)
    p.add_argument("--env", action="append", default=[])
    p.add_argument("--serve-arg", action="append", default=[])
    p.add_argument("--mode", choices=["threads", "processes"], default="threads")
    p.add_argument("-c", "--concurrency", type=int, default=16)
    p.add_argument("-s", "--size", type=int, default=1024)
    p.add_argument("--values", type=int, default=3)
    p.add_argument("--processes", type=int, default=1)
    p.add_argument("--scenarios", nargs="+", default=["add", "update"])
    p.add_argument("--strace", help="strace -c output file; attaches to the server")
    p.add_argument("--wrap", default="", help="a prefix for the zoe serve command")
    p.add_argument("--client-wrap", default="", help="a prefix for the zodbshootout command")
    p.add_argument("--client-timerslack-ns", type=int,
                   help="the timer slack of zodbshootout; ZEO.ClientStorage sleeps 1 µs after each reply")
    p.add_argument("--client-pool", type=int, default=1,
                   help="the connections of one zoeclientx storage; each transaction takes one")
    p.add_argument("--client-rule", action="store_true",
                   help="zoeclientx: deliver the invalidations and the commits of the pool in tid order")
    p.add_argument("--client", choices=["zeo", "zoeclientx"], default="zeo",
                   help="ZEO.ClientStorage, or the zoe-client storage of rustclient/")
    p.add_argument("--results", default=str(ROOT / "results.jsonl"))
    args = p.parse_args()

    wd = WORK / f"{args.label}-{args.mode}-c{args.concurrency}-s{args.size}"
    shutil.rmtree(wd, ignore_errors=True)
    wd.mkdir(parents=True)
    proc, addr, admin = start(args, wd)
    try:
        conf = wd / "zodb.conf"
        if args.server == "mapping":
            # zodbshootout runs its own MappingStorage first, then this
            # FileStorage in the process. Both have no server.
            conf.write_text(f"<zodb bench>\n  <filestorage>\n    path {wd / 'Data.fs'}\n  </filestorage>\n</zodb>\n")
        elif args.server == "relstorage":
            # The section of the harness, bench/shootout. It keeps history by default.
            keep = "    keep-history false\n" if args.history_free else ""
            conf.write_text(f"%import relstorage\n<zodb bench>\n  <relstorage>\n{keep}"
                            f"    <postgresql>\n      dsn {addr}\n    </postgresql>\n"
                            "  </relstorage>\n</zodb>\n")
        elif args.client == "zoeclientx":
            # The storage of rustclient/: zoe-client through PyO3.
            conf.write_text(f"%import zoeclientx\n<zodb bench>\n  <zoeclientx>\n    server {addr}\n"
                            f"    pool {args.client_pool}\n    rule {'on' if args.client_rule else 'off'}\n"
                            "  </zoeclientx>\n</zodb>\n")
        else:
            conf.write_text(f"<zodb bench>\n  <zeoclient>\n    server {addr}\n  </zeoclient>\n</zodb>\n")
        cmd = [*args.client_wrap.split(), sys.executable, "-m", "zodbshootout", "--processes", str(args.processes),
               "--values", str(args.values), "--warmups", "1", "--loops", "1",
               "--object-counts", "100", "--object-size", str(args.size),
               "--concurrency", str(args.concurrency),
               *(["--zap", "force"] if "add" in args.scenarios else []),
               "--include-mapping", "yes" if args.server == "mapping" else "no", "--quiet", "--output", str(wd / "pyperf.json")]
        if args.mode == "threads":
            cmd += ["--threads", "shared"]
        if "LD_LIBRARY_PATH" in os.environ:
            # pyperf starts its workers with a reduced environment, and the
            # last --inherit-environ wins over the one of zodbshootout.
            cmd += ["--inherit-environ", "LD_LIBRARY_PATH,PYTHONPATH"]
        cmd += [str(conf), *args.scenarios]
        m0, c0, t0, io0 = metrics(admin), server_cpu(proc), time.monotonic(), server_io(proc)
        tracer = None
        if args.strace:
            tracer = subprocess.Popen(
                ["strace", "-f", "-c", "-o", args.strace, "-p", str(proc.pid)],
                stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            time.sleep(0.5)
        import resource
        ru0 = resource.getrusage(resource.RUSAGE_CHILDREN)
        tc0 = time.monotonic()
        with open(wd / "zodbshootout.log", "wb") as log:
            env = dict(os.environ)
            if args.client == "zoeclientx":
                # zodbshootout gives the PYTHON* variables to its workers.
                env["PYTHONPATH"] = str(ROOT / "rustclient" / "python")
            done = subprocess.run(cmd, stdout=log, stderr=subprocess.STDOUT, env=env,
                                  preexec_fn=timerslack(args.client_timerslack_ns))
        ru1 = resource.getrusage(resource.RUSAGE_CHILDREN)
        client_cpu = (ru1.ru_utime + ru1.ru_stime) - (ru0.ru_utime + ru0.ru_stime)
        client_wall = time.monotonic() - tc0
        if tracer:
            tracer.send_signal(signal.SIGINT)
            tracer.wait()
        m1, c1, t1, io1 = metrics(admin), server_cpu(proc), time.monotonic(), server_io(proc)
        if done.returncode:
            print((wd / "zodbshootout.log").read_text()[-3000:], file=sys.stderr)
            raise SystemExit("zodbshootout failed")
    finally:
        if proc:
            proc.send_signal(signal.SIGTERM)
            try:
                proc.wait(timeout=30)
            except subprocess.TimeoutExpired:
                proc.kill()
    suite = pyperf.BenchmarkSuite.load(str(wd / "pyperf.json"))
    result = {"label": args.label, "server": args.server, "client": args.client,
              "history_free": args.history_free,
              "timerslack_ns": args.client_timerslack_ns, "pool": args.client_pool, "rule": args.client_rule, "mode": args.mode,
              "c": args.concurrency, "size": args.size, "env": args.env,
              "server_cpu_s": round(c1 - c0, 2), "wall_s": round(t1 - t0, 1)}
    for b in suite.get_benchmarks():
        name = b.get_name()
        if args.server == "mapping":
            # The suite holds the benchmarks of MappingStorage and of FileStorage.
            prefix = "inproc-mapping-" if "MappingStorage" in name else "inproc-filestorage-"
        else:
            prefix = ""
        key = next((v for k, v in [("write/read", "read_warm"), ("cold", "read_cold"), ("hot", "read_hot"), ("update", "update"), ("add", "add")] if k in name), name)
        mean = b.mean()
        result[prefix + key] = {"ops": round(args.concurrency * 100 / mean), "mean_ms": round(mean * 1e3, 1)}
    delta = {k: m1.get(k, 0) - m0.get(k, 0) for k in m1}
    if delta:
        commits = delta.get("zoe_commits_total", 0)
        result["commits"] = commits
        for h in ("zoe_vote_latency_seconds", "zoe_finish_latency_seconds"):
            n = delta.get(h + "_count", 0)
            if n:
                result[h.replace("zoe_", "").replace("_latency_seconds", "_ms")] = round(delta[h + "_sum"] / n * 1e3, 3)
        n = delta.get("zoe_fsync_batch_size_count", 0)
        if n:
            result["batch"] = round(delta["zoe_fsync_batch_size_sum"] / n, 2)
            result["syncs"] = n
        if commits:
            result["cpu_ms_per_commit"] = round((c1 - c0) / commits * 1e3, 2)
            result["disk_kb_per_commit"] = round((io1["write_bytes"] - io0["write_bytes"]) / commits / 1024, 1)
            result["syscw_per_commit"] = round((io1["syscw"] - io0["syscw"]) / commits, 1)
    # On a free-threaded interpreter, the modules that turned the GIL back on.
    gil = sorted(set(re.findall(r"GIL\) has been enabled to load module '([^']+)'",
                                (wd / "zodbshootout.log").read_text(errors="replace"))))
    result["python"] = sys.version.split()[0] + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else "")
    result["gil_enabled_by"] = gil
    err = (wd / "server.err").read_text() if (wd / "server.err").exists() else ""
    poc = [line for line in err.splitlines() if line.startswith("POC ")]
    if poc:
        result["poc"] = poc[-1]
    result["work"] = str(WORK)
    result["client_cpu_s"] = round(client_cpu, 2)
    result["client_cores"] = round(client_cpu / client_wall, 2)
    if os.environ.get("POC_CLEANUP"):
        shutil.rmtree(wd, ignore_errors=True)
    line = json.dumps(result)
    print(line)
    with open(args.results, "a") as f:
        f.write(line + "\n")


if __name__ == "__main__":
    main()
