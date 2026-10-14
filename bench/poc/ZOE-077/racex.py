"""Run the race tests of ZODB against zoeclientx on zoe serve. Not for merge.

The tests are those of `ZODB.tests.racetest.RaceTests`, unchanged. This
script only gives them a storage: `_storage` and `_new_storage_client`
open a zoeclientx storage with the pool and the rule of the command line.
ZODB fixes the number of iterations of each test, so the script runs each
test again and again, each run on a new `zoe serve` and a new store.

    python racex.py --binary ZOE --pool 4 --rule on --runs 100 \
        --tests loadopen external --results results/race.jsonl

Each run writes one JSON line: the test, the pool, the rule, the
interpreter, the modules that turned the GIL back on, the version of
persistent, the result and the first lines of a failure.
"""

import argparse
import importlib.metadata
import json
import os
import random
import re
import shutil
import signal
import subprocess
import sys
import sysconfig
import time
import unittest
import warnings
from pathlib import Path

ROOT = Path(__file__).resolve().parent
sys.path.insert(0, str(ROOT / "rustclient" / "python"))

# Record the modules that turn the GIL back on while they load.
with warnings.catch_warnings(record=True) as caught:
    warnings.simplefilter("always")
    import BTrees.OOBTree  # noqa: F401
    import persistent  # noqa: F401
    import transaction  # noqa: F401
    import ZODB  # noqa: F401
    from ZODB.tests.MinPO import MinPO
    from ZODB.tests.racetest import IModelSpec
    from ZODB.tests.racetest import RaceTests
    from zope.interface import implementer

    import zoeclientx
GIL_BY = sorted(
    {m for w in caught for m in re.findall(r"to load module '([^']+)'", str(w.message))}
)

WORK = Path(os.environ.get("POC_WORK", "/dev/shm/zoe-poc-151"))
TESTS = {
    "loadopen": "test_race_loadopen_vs_local_invalidate",
    "external": "test_race_load_vs_external_invalidate",
    "external-touch": "test_poc_load_vs_external_invalidate_touch",
}


@implementer(IModelSpec)
class T2ObjectsIncOrTouch:
    """A model of the POC for the checker of ZODB, not a test of ZODB.

    It is T2ObjectsInc, but half of the steps change only a third object.
    Such a step commits without a conflict even when its client did not
    yet get the invalidations of an earlier step of another client. So it
    shows a client that runs `func(tid)` before it delivered the earlier
    tids: the next transaction of that client starts after the earlier tid
    and still holds the old obj2 in its cache.

    invariant: obj1 == obj2
    """

    def init(_, root):
        root["obj1"] = MinPO(0)
        root["obj2"] = MinPO(0)
        root["obj3"] = MinPO(0)

    def next(_, root):
        if random.randint(0, 1):
            root["obj1"].value += 1
            root["obj2"].value += 1
        else:
            root["obj3"].value += 1

    def assertStateOK(_, root):
        i1 = root["obj1"].value
        i2 = root["obj2"].value
        if not (i1 == i2):
            raise AssertionError("obj1 (%d)  !=  obj2 (%d)" % (i1, i2))


def start(binary, wd):
    out = wd / "server.out"
    env = {k: v for k, v in os.environ.items() if not k.startswith(("ZOE_", "ZEO_"))}
    proc = subprocess.Popen(
        [binary, "serve", "--path", str(wd / "store"), "--listen", "127.0.0.1:0"],
        stdout=open(out, "wb"),
        stderr=open(wd / "server.err", "wb"),
        env=env,
    )
    for _ in range(400):
        text = out.read_text() if out.exists() else ""
        listen = re.search(r"^listening (\S+)", text, re.M)
        if listen:
            return proc, listen.group(1)
        if proc.poll() is not None:
            raise RuntimeError((wd / "server.err").read_text())
        time.sleep(0.025)
    raise RuntimeError("zoe did not listen")


def case(addr, pool, rule):
    class Race(RaceTests, unittest.TestCase):
        def setUp(self):
            self._opened = []
            self._storage = self._new_storage_client()

        def tearDown(self):
            for storage in self._opened:
                storage.close()

        def _new_storage_client(self):
            storage = zoeclientx.ZoeClientStorage(addr, pool=pool, rule=rule)
            self._opened.append(storage)
            return storage

        def test_poc_load_vs_external_invalidate_touch(self):
            return self._check_race_load_vs_external_invalidate(T2ObjectsIncOrTouch())

    return Race


def run_one(args, test, index):
    wd = WORK / f"{test}-{args.pool}-{args.rule}-{index}"
    shutil.rmtree(wd, ignore_errors=True)
    wd.mkdir(parents=True)
    proc, addr = start(args.binary, wd)
    t0 = time.monotonic()
    try:
        suite = unittest.TestSuite([case(addr, args.pool, args.rule == "on")(TESTS[test])])
        result = unittest.TestResult()
        suite.run(result)
    finally:
        proc.send_signal(signal.SIGTERM)
        try:
            proc.wait(timeout=30)
        except subprocess.TimeoutExpired:
            proc.kill()
        shutil.rmtree(wd, ignore_errors=True)
    problems = result.failures + result.errors
    skipped = [reason for _, reason in result.skipped]
    status = "fail" if problems else ("skip" if skipped else "pass")
    detail = ""
    if problems:
        trace = problems[0][1]
        lines = [line for line in trace.splitlines() if line.strip()]
        detail = "\n".join(lines[-6:])
    elif skipped:
        detail = skipped[0]
    return {
        "test": TESTS[test],
        "pool": args.pool,
        "rule": args.rule,
        "python": sys.version.split()[0]
        + ("t" if sysconfig.get_config_var("Py_GIL_DISABLED") else ""),
        "gil_enabled": getattr(sys, "_is_gil_enabled", lambda: True)(),
        "gil_enabled_by": GIL_BY,
        "persistent": importlib.metadata.version("persistent"),
        "run": index,
        "status": status,
        "seconds": round(time.monotonic() - t0, 2),
        "detail": detail,
    }


def main():
    p = argparse.ArgumentParser()
    p.add_argument("--binary", required=True)
    p.add_argument("--pool", type=int, default=4)
    p.add_argument("--rule", choices=["on", "off"], required=True)
    p.add_argument("--runs", type=int, default=10)
    p.add_argument("--tests", nargs="+", choices=sorted(TESTS), default=sorted(TESTS))
    p.add_argument("--stop-after", type=int, default=0, help="stop a test after this many failures")
    p.add_argument("--results", required=True)
    args = p.parse_args()
    with open(args.results, "a") as out:
        for test in args.tests:
            failures = 0
            for index in range(args.runs):
                line = run_one(args, test, index)
                out.write(json.dumps(line) + "\n")
                out.flush()
                print(test, args.pool, args.rule, index, line["status"], line["seconds"], flush=True)
                if line["status"] == "fail":
                    failures += 1
                    print(line["detail"], flush=True)
                    if args.stop_after and failures >= args.stop_after:
                        break


if __name__ == "__main__":
    main()
