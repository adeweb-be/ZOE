"""The client side of ZEO commits and loads, with ZEO.ClientStorage. Not for merge.

redbx/src/clientx.rs runs the same workload with zoe-client. Each worker
commits --txns transactions of 100 new objects (add), then --txns
transactions that rewrite its last 100 objects (update), then loads each
object that it added, on a new connection with a cold cache (load). With
--shared, the workers are threads on one ClientStorage, as the threads cells
of zodbshootout are. Without it, each worker is a process with its own
ClientStorage, as the processes cells are. The report is one JSON line.
"""

import argparse
import json
import logging
import multiprocessing
import resource
import struct
import threading
import time

from ZEO.ClientStorage import ClientStorage
from ZODB.Connection import TransactionMetaData
from ZODB.utils import maxtid, z64


def cpu(pid=None):
    if pid is None:
        r = resource.getrusage(resource.RUSAGE_SELF)
        return r.ru_utime + r.ru_stime
    fields = open(f"/proc/{pid}/stat").read().rsplit(")", 1)[1].split()
    return (int(fields[11]) + int(fields[12])) / 100.0


def samples(path):
    raw = open(path, "rb").read()
    out, i = [], 0
    while i < len(raw):
        (n,) = struct.unpack_from("<I", raw, i)
        out.append(raw[i + 4 : i + 4 + n])
        i += 4 + n
    return out


def connect(addr):
    host, port = addr.rsplit(":", 1)
    return ClientStorage((host, int(port)), wait=True)


def add(cs, w, txns, data):
    added, last = [], None
    for t in range(txns):
        oids = [cs.new_oid() for _ in range(100)]
        txn = TransactionMetaData()
        cs.tpc_begin(txn)
        for i, oid in enumerate(oids):
            cs.store(oid, z64, data[(w * 7919 + t * 100 + i) % len(data)], "", txn)
        cs.tpc_vote(txn)
        tid = cs.tpc_finish(txn)
        added.extend(oids)
        last = (oids, tid)
    return added, last


def update(cs, w, txns, data, last):
    oids, serial = last
    for t in range(txns):
        txn = TransactionMetaData()
        cs.tpc_begin(txn)
        for i, oid in enumerate(oids):
            cs.store(oid, serial, data[(w * 104729 + t * 100 + i) % len(data)], "", txn)
        cs.tpc_vote(txn)
        serial = cs.tpc_finish(txn)


def load(cs, oids):
    for oid in oids:
        cs.loadBefore(oid, maxtid)


def process_worker(w, args, barrier, queue):
    data = samples(args.samples)
    cs = connect(args.server)
    marks = []
    barrier.wait()
    marks.append(cpu())
    added, last = add(cs, w, args.txns, data)
    marks.append(cpu())
    barrier.wait()
    update(cs, w, args.txns, data, last)
    marks.append(cpu())
    barrier.wait()
    fresh = connect(args.server)
    barrier.wait()
    m = cpu()
    load(fresh, added)
    marks.append(cpu())
    barrier.wait()
    cs.close()
    fresh.close()
    # The CPU of each phase: add, update, load.
    queue.put((marks[1] - marks[0], marks[2] - marks[1], marks[3] - m))


def run_processes(args):
    ctx = multiprocessing.get_context("fork")
    barrier = ctx.Barrier(args.workers + 1)
    queue = ctx.Queue()
    procs = [ctx.Process(target=process_worker, args=(w, args, barrier, queue)) for w in range(args.workers)]
    for p in procs:
        p.start()
    walls, servers = [], []
    barrier.wait()
    t, s = time.monotonic(), cpu(args.server_pid)
    for _ in ("add", "update"):
        barrier.wait()
        t1, s1 = time.monotonic(), cpu(args.server_pid)
        walls.append(t1 - t)
        servers.append(s1 - s)
        t, s = t1, s1
    barrier.wait()
    t, s = time.monotonic(), cpu(args.server_pid)
    barrier.wait()
    walls.append(time.monotonic() - t)
    servers.append(cpu(args.server_pid) - s)
    per = [queue.get() for _ in procs]
    for p in procs:
        p.join()
    clients = [sum(x[i] for x in per) for i in range(3)]
    return walls, clients, servers


def run_threads(args):
    data = samples(args.samples)
    cs = connect(args.server)
    results = [None] * args.workers

    def phase(target):
        c0, s0, t0 = cpu(), cpu(args.server_pid), time.monotonic()
        threads = [threading.Thread(target=target, args=(w,)) for w in range(args.workers)]
        for th in threads:
            th.start()
        for th in threads:
            th.join()
        return time.monotonic() - t0, cpu() - c0, cpu(args.server_pid) - s0

    def do_add(w):
        results[w] = add(cs, w, args.txns, data)

    def do_update(w):
        update(cs, w, args.txns, data, results[w][1])

    walls, clients, servers = [], [], []
    for target in (do_add, do_update):
        wall, client, server = phase(target)
        walls.append(wall)
        clients.append(client)
        servers.append(server)
    fresh = connect(args.server)
    wall, client, server = phase(lambda w: load(fresh, results[w][0]))
    walls.append(wall)
    clients.append(client)
    servers.append(server)
    cs.close()
    fresh.close()
    return walls, clients, servers


def report(wall, client, server, commits, objects):
    return {
        "wall_s": round(wall, 3),
        "commits_per_s": round(commits / wall),
        "objects_per_s": round(objects / wall),
        "client_cpu_s": round(client, 2),
        "client_us_per_object": round(client * 1e6 / objects, 1),
        "server_cpu_s": round(server, 2),
        "server_us_per_object": round(server * 1e6 / objects, 1),
    }


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--server", required=True)
    p.add_argument("--workers", type=int, default=16)
    p.add_argument("--shared", action="store_true")
    p.add_argument("--txns", type=int, default=20)
    p.add_argument("--samples", required=True)
    p.add_argument("--server-pid", type=int, required=True)
    p.add_argument("--label", default="python")
    args = p.parse_args()
    logging.basicConfig(level=logging.ERROR)
    walls, clients, servers = (run_threads if args.shared else run_processes)(args)
    commits = args.workers * args.txns
    out = {"client": "ZEO.ClientStorage", "label": args.label, "workers": args.workers,
           "shared": args.shared, "txns": args.txns}
    for i, name in enumerate(("add", "update", "load")):
        out[name] = report(walls[i], clients[i], servers[i], 0 if name == "load" else commits, commits * 100)
    print(json.dumps(out))


if __name__ == "__main__":
    main()
