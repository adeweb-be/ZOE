"""Run digest.py and sample the object cache of its connection.

    run.py ZOPE_CONF cache_probe.py DIGEST_PY OUT_JSON PROBE_JSON

A thread reads the count of the objects of the cache that are not ghosts,
each millisecond, while digest.py walks the site. PROBE_JSON gets the
target size of the cache, the largest count that the thread read, and the
count of the objects of the database.
"""

import json
import sys
import threading
import time

digest_py, out_json, probe_json = sys.argv[1:4]
connection = app._p_jar  # noqa: F821
cache = connection._cache
target = cache.cache_size
objects = connection.db().objectCount()
peak = 0
done = threading.Event()


def sample():
    global peak
    while not done.is_set():
        peak = max(peak, cache.cache_non_ghost_count)
        time.sleep(0.001)


thread = threading.Thread(target=sample, daemon=True)
thread.start()
sys.argv = [digest_py, out_json]
with open(digest_py) as file:
    code = compile(file.read(), digest_py, "exec")
try:
    exec(code, {"app": app, "__name__": "__main__", "__file__": digest_py})  # noqa: F821
finally:
    done.set()
    thread.join()
with open(probe_json, "w") as file:
    json.dump({"target": target, "peak": peak, "objects": objects}, file)
