"""Start the Locust file of a scenario and run its users with no server.

    python locust_probe.py SCENARIO USERS CALLS OUT_JSON

The environment gives the variables that the runner gives to Locust:
PLONEBENCH_MANIFEST and PLONEBENCH_CREDENTIALS at least. The script sets
PLONEBENCH_SCENARIO, and PLONEBENCH_STEPS to one step of USERS users.

The script imports the Locust file as Locust does, then makes the users
of the scenario (the background users too) and calls `on_start` of each
one. Then it calls the task of each user CALLS times. A recorder takes
the place of `PloneUser.operation`, so no request goes out: it records
the kind of each operation and its arguments. An argument that is an
item (a dict with a `path`, or an object with a `path` attribute) is
recorded as its path. For a body that is a closure, the recorder also
records the values that the closure holds, such as the Document and the
folder of `warm-forms`. `gevent.sleep` returns at once.

OUT_JSON gets the times, the peak memory of the process and a sha256 of
the operations. The time of the start runs from the start of the script
to the end of the import of the Locust file.
"""

import time

STARTED = time.perf_counter()

import hashlib  # noqa: E402
import json  # noqa: E402
import os  # noqa: E402
import resource  # noqa: E402
import sys  # noqa: E402

scenario, users, calls, out = (
    sys.argv[1],
    int(sys.argv[2]),
    int(sys.argv[3]),
    sys.argv[4],
)
os.environ["PLONEBENCH_SCENARIO"] = scenario
os.environ["PLONEBENCH_STEPS"] = f"{users}:60"
os.environ.setdefault(
    "PLONEBENCH_HOSTS", ",".join(f"http://127.0.0.1:{9 + n}" for n in range(4))
)

from locust.env import Environment  # noqa: E402

from plonebench import corpus, locustfile  # noqa: E402
from plonebench.ledger import OK  # noqa: E402

IMPORTED = time.perf_counter()


def describe(value):
    """Return a value that JSON can hold, with each item as its path."""
    if isinstance(value, dict):
        if "path" in value:
            return value["path"]
        return {str(k): describe(v) for k, v in sorted(value.items())}
    if isinstance(value, str | int | float | bool) or value is None:
        return value
    if hasattr(value, "path"):
        return value.path
    if isinstance(value, list | tuple):
        return [describe(v) for v in value]
    return type(value).__name__


def held(body):
    """The values that a closure holds, by name, without the user."""
    cells = getattr(body, "__closure__", None) or ()
    names = getattr(getattr(body, "__code__", None), "co_freevars", ())
    return {
        name: describe(cell.cell_contents)
        for name, cell in zip(names, cells, strict=True)
        if name != "self"
    }


records = []


def operation(self, kind, body, *args, retry=False):
    records.append([self.index, kind, [describe(a) for a in args], held(body)])
    target = describe(args[0]) if args else None
    return locustfile.OpResult(OK, target=target, values={"state": corpus.PUBLISHED})


locustfile.PloneUser.operation = operation
locustfile.gevent.sleep = lambda *args, **kwargs: None
environment = Environment(user_classes=[locustfile.PloneUser])
count = users + len(locustfile.SCENARIO.background)

started = time.perf_counter()
made = []
for _ in range(count):
    user = locustfile.PloneUser(environment)
    user.on_start()
    made.append(user)
setup = time.perf_counter() - started
accounts = [
    [u.index, u.role, u.rank, u.account, getattr(u, "section", None)] for u in made
]

started = time.perf_counter()
for user in made:
    for _ in range(calls):
        user.work()
work = time.perf_counter() - started

roles = {}
for user in made:
    roles[user.role] = roles.get(user.role, 0) + 1
result = {
    "scenario": scenario,
    "users": count,
    "roles": roles,
    "start_seconds": IMPORTED - STARTED,
    "setup_seconds": setup,
    "work_seconds": work,
    "work_calls": count * calls,
    "operations": len(records),
    # ru_maxrss is in KB on Linux and in bytes on macOS.
    "maxrss_bytes": resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    * (1 if sys.platform == "darwin" else 1024),
    "digest": hashlib.sha256(json.dumps([accounts, records]).encode()).hexdigest(),
}
with open(out, "w") as file:
    json.dump(result, file)
