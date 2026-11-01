"""The loads of a Plone request, in one Zope thread. run.py runs it:

    interpreter run.py ZOPE_CONF loadpath.py OUT ADMIN PATHS REQUESTS

PATHS is a JSON file: a list of [kind, path], such as
["document", "/Plone/section-00/folder-00/document-00"]. The script sends
REQUESTS GET requests, anonymous, through the WSGI publisher of Zope in
this process, in one thread, and takes the paths in turn. Before each
request, it calls `cacheMinimize()` on the database, so each object that
the request reads loads through `Connection.setstate`. For each request it
records the time, the loads of the connections (`_load_count`) and the
loads that `zoe serve` counted on its admin port ADMIN: the misses of the
client cache. The functions `_request_folder` and `_request_document`
run these requests only, so a profile of this process can count the
samples under each.

Then it sends each path once more with `sys.setprofile` on, and counts the
loads, the `persistent_load` callbacks and the Python calls under
`Connection.setstate`, and the classes of the objects loaded, with whether
their `__setstate__` is Python code. It writes the result as JSON to OUT.
"""

import inspect
import io
import json
import statistics
import sys
import urllib.request
from collections import Counter
from time import perf_counter

from ZODB.Connection import Connection
from ZODB.serialize import ObjectReader
from ZPublisher.WSGIPublisher import publish_module

out, admin, paths_file, requests = sys.argv[1:5]
reader = sys.argv[5:6] == ["reader"]
requests = int(requests)
paths = json.loads(open(paths_file).read())
db = app._p_jar.db()  # noqa: F821 - run.py gives app
if reader:
    # The reader of the spike ZOE-274: setGhostState and getState in Rust.
    import zoe.reader

    zoe.reader.install(db)
    zoe.reader.reset_counts()


def server_loads():
    """The sum of `zoe_loads_total` on the admin port of zoe serve."""
    with urllib.request.urlopen(f"http://{admin}/metrics", timeout=30) as reply:
        text = reply.read().decode()
    total = 0.0
    for line in text.splitlines():
        name, _, value = line.rpartition(" ")
        if name.split("{", 1)[0] == "zoe_loads_total":
            total += float(value)
    return int(total)


def load_count():
    return sum(conn._load_count for conn in db.pool.all)


def environ(path):
    return {
        "REQUEST_METHOD": "GET",
        "PATH_INFO": path,
        "QUERY_STRING": "",
        "SERVER_NAME": "127.0.0.1",
        "SERVER_PORT": "8080",
        "SERVER_PROTOCOL": "HTTP/1.1",
        "HTTP_HOST": "127.0.0.1:8080",
        "wsgi.url_scheme": "http",
        "wsgi.input": io.BytesIO(),
        "wsgi.errors": sys.stderr,
        "wsgi.version": (1, 0),
        "wsgi.multithread": False,
        "wsgi.multiprocess": False,
        "wsgi.run_once": False,
    }


def publish(path):
    status = []

    def start_response(code, headers, exc_info=None):
        status.append(code)

    body = publish_module(environ(path), start_response)
    size = sum(len(chunk) for chunk in body)
    if hasattr(body, "close"):
        body.close()
    return status[0], size


# The requests of each kind run in a function of their own, so a profile
# can count the samples of each kind apart.
def _request_folder(path):
    return publish(path)


def _request_document(path):
    return publish(path)


REQUEST = {"folder": _request_folder, "document": _request_document}


rows = []
for index in range(requests):
    kind, path = paths[index % len(paths)]
    db.cacheMinimize()
    loads_before, server_before = load_count(), server_loads()
    start = perf_counter()
    status, size = REQUEST[kind](path)
    seconds = perf_counter() - start
    rows.append(
        {
            "kind": kind,
            "path": path,
            "first": index < len(paths),
            "status": status,
            "bytes": size,
            "seconds": seconds,
            "loads": load_count() - loads_before,
            "server_loads": server_loads() - server_before,
        }
    )

# The counts, with sys.setprofile on.
SETSTATE = Connection.setstate.__code__
PERSISTENT_LOAD = ObjectReader._persistent_load.__code__
counts = Counter()
classes = Counter()
inside = [0]


def profile(frame, event, arg):
    if event == "call":
        code = frame.f_code
        if code is SETSTATE:
            inside[0] += 1
            counts["loads"] += 1
            obj = frame.f_locals.get("obj")
            klass = type(obj)
            classes[f"{klass.__module__}.{klass.__qualname__}"] += 1
        elif inside[0]:
            counts["python calls under setstate"] += 1
            if code is PERSISTENT_LOAD:
                counts["persistent_load"] += 1
        counts["python calls"] += 1
    elif event == "return" and frame.f_code is SETSTATE:
        inside[0] -= 1


for _kind, path in paths:
    db.cacheMinimize()
    sys.setprofile(profile)
    try:
        publish(path)
    finally:
        sys.setprofile(None)


def python_setstate(name):
    module, _, attr = name.rpartition(".")
    klass = sys.modules.get(module)
    for part in attr.split("."):
        klass = getattr(klass, part, None)
    return inspect.isfunction(getattr(klass, "__setstate__", None))


def summary(selected, key):
    values = [row[key] for row in selected]
    return {
        "median": statistics.median(values),
        "mean": statistics.fmean(values),
        "low": min(values),
        "high": max(values),
    }


groups = {}
for kind in sorted({row["kind"] for row in rows}):
    for first in (True, False):
        selected = [r for r in rows if r["kind"] == kind and r["first"] == first]
        if selected:
            groups[f"{kind} {'first' if first else 'again'}"] = {
                "requests": len(selected),
                **{
                    key: summary(selected, key)
                    for key in ("seconds", "loads", "server_loads")
                },
            }
loads = counts["loads"] or 1
reader_counts = None
if reader:
    reader_counts = zoe.reader.counts()
result = {
    "reader": reader,
    "reader_counts": reader_counts,
    "requests": rows,
    "groups": groups,
    "counted_requests": len(paths),
    "counts": dict(counts),
    "per_load": {
        "persistent_load": counts["persistent_load"] / loads,
        "python calls under setstate": counts["python calls under setstate"] / loads,
    },
    "classes": [
        {"class": name, "loads": n, "python_setstate": python_setstate(name)}
        for name, n in classes.most_common()
    ],
}
with open(out, "w") as file:
    json.dump(result, file, indent=1)
print(json.dumps(groups, indent=1))
