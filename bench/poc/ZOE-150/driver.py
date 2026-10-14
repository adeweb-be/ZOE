"""One run of the ZOE-150 spike. Not for merge.

Start one server on a fresh store, run zodbshootout for one scenario of one
cell as the harness runs one value (one pyperf worker, one warmup, the
values, one loop per sample), and append one JSON line to the results.

Configurations:

- `zeo`: the ZEO 6 server on a FileStorage, and `ZEO.ClientStorage`.
- `poc-main`: `zoe serve` as on main, and the POC client of
  `ZOE-077/rustclient` with a pool of connections and no cache.
- `poc-proto`: the same client, and `zoe serve` of this branch with the
  prototypes of ZOE-077 (compression at each `storea`, a vote that reads
  only tids).
- `floor`: no server. zodbshootout in the process on a DemoStorage: its
  changes go to a MappingStorage, its base is a read-only FileStorage that
  the driver fills first. This is the time of ZODB and zodbshootout alone.

The line holds the machine state too: the CPU governor and frequency, the
cores of the server and of the client, the load of other processes during
the run, and on a free-threaded interpreter the modules that turned the GIL
back on.
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
import threading
import time
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPO = HERE.parents[2]
CLIENT = HERE.parent / "ZOE-077" / "rustclient" / "python"
# The text that zodbshootout puts in the name of each benchmark.
SCENARIOS = {
    "add": "add {n} objects",
    "update": "update {n} objects",
    "cold": "read {n} cold objects",
    "warm": "write/read {n} objects",
    "hot": "read {n} hot objects",
}
NAMES = {"add": "add", "update": "update", "cold": "read_cold", "warm": "read_warm", "hot": "read_hot"}
PROTO = ["ZOE_POC_STORECOMPRESS=1", "ZOE_POC_TIDONLY=1"]
TICK = os.sysconf("SC_CLK_TCK")


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def read(path, default=""):
    try:
        return Path(path).read_text().strip()
    except OSError:
        return default


def cpu_state():
    cpus = sorted(Path("/sys/devices/system/cpu").glob("cpu[0-9]*"), key=lambda p: int(p.name[3:]))
    freqs = [int(read(c / "cpufreq/scaling_cur_freq", "0")) for c in cpus]
    return {
        "governor": sorted({read(c / "cpufreq/scaling_governor") for c in cpus}),
        "epp": sorted({read(c / "cpufreq/energy_performance_preference") for c in cpus}),
        "driver": read("/sys/devices/system/cpu/intel_pstate/status") or read(cpus[0] / "cpufreq/scaling_driver"),
        "no_turbo": read("/sys/devices/system/cpu/intel_pstate/no_turbo"),
        "mhz_mean": round(sum(freqs) / len(freqs) / 1000) if freqs else None,
        "mhz_by_cpu": [round(f / 1000) for f in freqs],
        "loadavg": read("/proc/loadavg").split()[:3],
    }


def busy_seconds():
    """The user and system time of all CPUs, from /proc/stat, in seconds.

    The time of interrupts does not count: the kernel does not charge it to
    a process, and the loopback traffic of the run makes most of it.
    """
    fields = [int(x) for x in read("/proc/stat").splitlines()[0].split()[1:]]
    # user nice system idle iowait irq softirq steal guest guest_nice
    return (fields[0] + fields[1] + fields[2] + fields[7]) / TICK


def kernel_seconds():
    """The CPU time of the kernel threads: the children of kthreadd."""
    total = 0
    for entry in Path("/proc").iterdir():
        if not entry.name.isdigit():
            continue
        try:
            fields = (entry / "stat").read_text().rsplit(")", 1)[1].split()
        except OSError:
            continue
        if fields[1] == "2":
            total += int(fields[11]) + int(fields[12])
    return total / TICK


def tree_cpu(pid):
    """The CPU seconds of a process and of its children, live or reaped."""

    def stat(p):
        return Path(f"/proc/{p}/stat").read_text().rsplit(")", 1)[1].split()

    try:
        own = stat(pid)
    except OSError:
        return 0.0
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
    return total / TICK


def du(path):
    total = 0
    for root, _, files in os.walk(path):
        for name in files:
            try:
                total += os.lstat(os.path.join(root, name)).st_blocks * 512
            except OSError:
                pass
    return total


def metrics(admin):
    if not admin:
        return {}
    text = urllib.request.urlopen(f"http://{admin}/metrics", timeout=5).read().decode()
    out = {}
    for line in text.splitlines():
        m = re.match(r"^(zoe_\w+?)(?:\{[^}]*\})?\s+([0-9.eE+-]+)$", line)
        if m and m.group(1).endswith(("_sum", "_count", "_total")):
            out[m.group(1)] = out.get(m.group(1), 0.0) + float(m.group(2))
    return out


class Spans:
    """An OTLP/HTTP JSON receiver that sums the spans of the server by name."""

    def __init__(self):
        self.lock = threading.Lock()
        self.by_name = {}
        spans = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                body = self.rfile.read(int(self.headers.get("Content-Length", 0)))
                try:
                    spans.add(json.loads(body))
                except ValueError:
                    pass
                self.send_response(200)
                self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"{}")

            def log_message(self, *args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.endpoint = f"http://127.0.0.1:{self.server.server_address[1]}"
        threading.Thread(target=self.server.serve_forever, daemon=True).start()

    def add(self, doc):
        with self.lock:
            for rs in doc.get("resourceSpans", []):
                for ss in rs.get("scopeSpans", []):
                    for span in ss.get("spans", []):
                        name = span.get("name", "?")
                        took = (int(span["endTimeUnixNano"]) - int(span["startTimeUnixNano"])) / 1e3
                        entry = self.by_name.setdefault(name, {"n": 0, "us": 0.0, "max_us": 0.0, "commits": 0})
                        entry["n"] += 1
                        entry["us"] += took
                        entry["max_us"] = max(entry["max_us"], took)
                        for attr in span.get("attributes", []):
                            if attr.get("key") == "commits":
                                entry["commits"] += int(attr["value"].get("intValue", 0))

    def summary(self):
        with self.lock:
            return {
                name: {
                    "n": e["n"],
                    "mean_us": round(e["us"] / e["n"], 1),
                    "max_us": round(e["max_us"], 1),
                    **({"commits": e["commits"]} if e["commits"] else {}),
                }
                for name, e in sorted(self.by_name.items())
            }

    def close(self):
        self.server.shutdown()


def pinned(cpus, cmd):
    return ["taskset", "-c", cpus, *cmd] if cpus else cmd


def start_server(args, wd, spans):
    """Start the server of the configuration. Return (proc, address, admin)."""
    log = open(wd / "server.err", "wb")
    if args.config == "zeo":
        port = free_port()
        env = {k: v for k, v in os.environ.items() if not k.startswith(("ZOE_", "ZEO_"))}
        cmd = [args.zeo_python, "-m", "ZEO.runzeo", "-a", f"127.0.0.1:{port}", "-f", str(wd / "Data.fs")]
        proc = subprocess.Popen(pinned(args.server_cpus, cmd), stdout=log, stderr=subprocess.STDOUT, env=env)
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            try:
                socket.create_connection(("127.0.0.1", port), timeout=0.1).close()
                return proc, f"127.0.0.1:{port}", None
            except OSError:
                if proc.poll() is not None:
                    raise RuntimeError((wd / "server.err").read_text()) from None
                time.sleep(0.05)
        raise RuntimeError("zeo did not listen")
    env = {k: v for k, v in os.environ.items() if not k.startswith(("ZOE_", "OTEL_"))}
    binary = args.zoe_main if args.config == "poc-main" else args.zoe_proto
    toggles = PROTO if args.config == "poc-proto" else []
    if args.profile and args.config == "poc-proto":
        toggles = [*toggles, "ZOE_POC_PROFILE=1"]
    for kv in toggles:
        k, v = kv.split("=", 1)
        env[k] = v
    if spans:
        env["ZOE_OTLP_ENDPOINT"] = spans.endpoint
        env["OTEL_EXPORTER_OTLP_PROTOCOL"] = "http/json"
    out = wd / "server.out"
    cmd = [binary, "serve", "--path", str(wd / "store"), "--listen", "127.0.0.1:0", "--admin-listen", "127.0.0.1:0"]
    proc = subprocess.Popen(pinned(args.server_cpus, cmd), stdout=open(out, "wb"), stderr=log, env=env)
    deadline = time.monotonic() + 30
    while time.monotonic() < deadline:
        text = out.read_text() if out.exists() else ""
        listen = re.search(r"^listening (\S+)", text, re.M)
        admin = re.search(r"^admin (\S+)", text, re.M)
        if listen and admin:
            return proc, listen.group(1), admin.group(1)
        if proc.poll() is not None:
            raise RuntimeError((wd / "server.err").read_text())
        time.sleep(0.05)
    raise RuntimeError("zoe did not listen")


def stop_server(proc):
    if proc is None or proc.poll() is not None:
        return
    proc.send_signal(signal.SIGTERM)
    try:
        proc.wait(timeout=60)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.wait()


def populate_floor(args, wd):
    """Fill the FileStorage that the floor reads as its base."""
    code = (
        "import sys\n"
        "from ZODB import DB\n"
        "from ZODB.FileStorage import FileStorage\n"
        "from zodbshootout.speedtest import SpeedTestData\n"
        "c, size, path = int(sys.argv[1]), int(sys.argv[2]), sys.argv[3]\n"
        "data = SpeedTestData(c, 100, size)\n"
        "data.populate(lambda: DB(FileStorage(path)), include_data=True)\n"
    )
    subprocess.run([sys.executable, "-c", code, str(args.concurrency), str(args.size), str(wd / "Data.fs")], check=True)


def zconfig(args, wd, addr):
    if args.config == "floor":
        return (
            "<zodb floor>\n  <demostorage>\n    <filestorage base>\n"
            f"      path {wd / 'Data.fs'}\n      read-only true\n"
            "    </filestorage>\n  </demostorage>\n</zodb>\n"
        )
    if args.config == "zeo":
        return f"<zodb zeo>\n  <zeoclient>\n    server {addr}\n  </zeoclient>\n</zodb>\n"
    return (
        f"%import zoeclientx\n<zodb poc>\n  <zoeclientx>\n    server {addr}\n"
        f"    pool {args.pool}\n  </zoeclientx>\n</zodb>\n"
    )


def objects(args):
    """The objects that one call of the scenario reads or writes."""
    if args.scenario == "warm":
        return args.concurrency * 100
    loops = 100 if args.concurrency <= 10 else 30
    return args.concurrency * loops * 100


def main():
    # A stop of the run must stop the server too: the finally clause runs.
    signal.signal(signal.SIGTERM, lambda *_: sys.exit(143))
    p = argparse.ArgumentParser()
    p.add_argument("--config", choices=["zeo", "poc-main", "poc-proto", "floor"], required=True)
    p.add_argument("--mode", choices=["threads", "processes"], required=True)
    p.add_argument("-c", "--concurrency", type=int, required=True)
    p.add_argument("-s", "--size", type=int, required=True)
    p.add_argument("--scenario", choices=sorted(SCENARIOS), required=True)
    p.add_argument("--round", type=int, default=0)
    p.add_argument("--attempt", type=int, default=1)
    p.add_argument("--values", type=int, default=1)
    p.add_argument("--warmups", type=int, default=1)
    p.add_argument("--pool", type=int, default=16)
    p.add_argument("--tag", default="main", help="main, spans, pyspy or floor")
    p.add_argument("--spans", action="store_true", help="receive the OTLP spans of zoe serve")
    p.add_argument("--profile", action="store_true", help="ZOE_POC_PROFILE=1 on the prototype binary")
    p.add_argument("--client-wrap", default="", help="a prefix of the zodbshootout command")
    p.add_argument("--server-cpus", default=os.environ.get("ZOE150_SERVER_CPUS", ""))
    p.add_argument("--client-cpus", default=os.environ.get("ZOE150_CLIENT_CPUS", ""))
    p.add_argument("--zeo-python", default=os.environ.get("ZOE150_ZEO_PYTHON", sys.executable))
    p.add_argument("--zoe-main", default=str(REPO / "target" / "zoe150" / "zoe-main"))
    p.add_argument("--zoe-proto", default=str(REPO / "target" / "zoe150" / "zoe-proto"))
    p.add_argument("--work", type=Path, required=True)
    p.add_argument("--results", type=Path, required=True)
    p.add_argument("--keep", action="store_true", help="keep the store after the run")
    args = p.parse_args()

    python = sys.version.split()[0] + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else "")
    name = f"{args.tag}-{python}-{args.config}-{args.mode}-c{args.concurrency}-s{args.size}-{args.scenario}-r{args.round}-a{args.attempt}"
    wd = args.work / name
    shutil.rmtree(wd, ignore_errors=True)
    wd.mkdir(parents=True)
    result = {
        "tag": args.tag, "config": args.config, "python": python, "mode": args.mode,
        "c": args.concurrency, "size": args.size, "scenario": NAMES[args.scenario],
        "round": args.round, "attempt": args.attempt, "values": args.values, "warmups": args.warmups,
        "pool": args.pool if args.config.startswith("poc") else None,
        "server_cpus": args.server_cpus, "client_cpus": args.client_cpus,
        "objects_per_call": objects(args), "disk_free_before": shutil.disk_usage(args.work).free,
        "started": time.time(), "cpu_before": cpu_state(),
    }
    spans = Spans() if args.spans else None
    proc = addr = admin = None
    ok = False
    try:
        if args.config == "floor":
            populate_floor(args, wd)
        else:
            proc, addr, admin = start_server(args, wd, spans)
        conf = wd / "zodb.conf"
        conf.write_text(zconfig(args, wd, addr))
        cmd = [
            *args.client_wrap.split(), sys.executable, "-m", "zodbshootout",
            "--processes", "1", "--values", str(args.values), "--warmups", str(args.warmups), "--loops", "1",
            "--object-counts", "100", "--object-size", str(args.size), "--concurrency", str(args.concurrency),
            "--include-mapping", "no", "--quiet", "--output", str(wd / "pyperf.json"),
        ]
        if args.scenario == "add" and args.config != "floor":
            cmd += ["--zap", "force"]
        if args.mode == "threads":
            cmd += ["--threads", "shared"]
        # pyperf starts its workers with a reduced environment.
        cmd += ["--inherit-environ", "LD_LIBRARY_PATH,PYTHONPATH"]
        cmd += [str(conf), args.scenario]
        env = dict(os.environ)
        if args.config.startswith("poc"):
            env["PYTHONPATH"] = str(CLIENT)
        import resource

        m0 = metrics(admin)
        s0 = tree_cpu(proc.pid) if proc else 0.0
        b0, k0 = busy_seconds(), kernel_seconds()
        r0 = resource.getrusage(resource.RUSAGE_CHILDREN)
        t0 = time.monotonic()
        with open(wd / "zodbshootout.log", "wb") as log:
            done = subprocess.run(pinned(args.client_cpus, cmd), stdout=log, stderr=subprocess.STDOUT, env=env)
        wall = time.monotonic() - t0
        r1 = resource.getrusage(resource.RUSAGE_CHILDREN)
        b1, k1 = busy_seconds(), kernel_seconds()
        s1 = tree_cpu(proc.pid) if proc else 0.0
        m1 = metrics(admin)
        result["cpu_after"] = cpu_state()
        result["store_bytes"] = du(wd)
        client_cpu = (r1.ru_utime + r1.ru_stime) - (r0.ru_utime + r0.ru_stime)
        result["wall_s"] = round(wall, 2)
        result["client_cpu_s"] = round(client_cpu, 2)
        result["server_cpu_s"] = round(s1 - s0, 2)
        # The CPU of the other processes of the machine during the run, and
        # of the kernel threads: the sync and the disk encryption of the
        # run are kernel threads.
        result["kernel_cores"] = round((k1 - k0) / wall, 2)
        result["foreign_cores"] = round(max(0.0, (b1 - b0) - (k1 - k0) - client_cpu - (s1 - s0)) / wall, 2)
        log_text = (wd / "zodbshootout.log").read_text(errors="replace")
        result["gil_enabled_by"] = sorted(set(re.findall(r"GIL\) has been enabled to load module '([^']+)'", log_text)))
        if done.returncode:
            result["error"] = log_text[-2000:]
        else:
            import pyperf

            suite = pyperf.BenchmarkSuite.load(str(wd / "pyperf.json"))
            text = SCENARIOS[args.scenario].format(n=100)
            found = [b for b in suite.get_benchmarks() if b.get_name().endswith(f": {text}")]
            values = list(found[0].get_values())
            mean = sum(values) / len(values)
            result["samples_s"] = values
            result["mean_ms"] = round(mean * 1e3, 3)
            result["ops"] = round(args.concurrency * 100 / mean, 1)
            ok = True
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
    except Exception as e:  # noqa: BLE001 - one failed run must not stop the set
        result["error"] = repr(e)
    finally:
        stop_server(proc)
        if spans:
            time.sleep(0.5)
            result["spans"] = spans.summary()
            spans.close()
    err = read(wd / "server.err")
    poc = [line for line in err.splitlines() if line.startswith("POC ")]
    if poc:
        result["poc"] = poc[-1]
    result["ok"] = ok
    if ok and not args.keep:
        shutil.rmtree(wd, ignore_errors=True)
    elif not ok:
        result["work_dir"] = str(wd)
    line = json.dumps(result)
    with open(args.results, "a") as f:
        f.write(line + "\n")
    brief = {k: result.get(k) for k in ("config", "python", "mode", "c", "size", "scenario", "round", "ops", "wall_s", "foreign_cores", "ok")}
    print(json.dumps(brief), flush=True)
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
