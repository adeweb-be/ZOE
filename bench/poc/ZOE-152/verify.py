"""ZEO's verification and lock, run by the stock client on the prototype.

Not for merge. A ZEO 6.2 server on a FileStorage; a `ZEO.ClientStorage` with
the prototype as a private cache file. Each step reopens the client and
prints `verify_result` of ZEO's client, the hits and misses of the cache, and
whether every object read its newest value.
"""

import json
import os
import sys
import tempfile
import time

sys.path.insert(0, os.path.dirname(__file__))

import transaction  # noqa: E402
import ZEO  # noqa: E402
import ZODB  # noqa: E402
from hit import Proto  # noqa: E402
from ZEO.ClientStorage import ClientStorage  # noqa: E402
from zodbshootout._pobject import PObject  # noqa: E402

N = 50


def client(addr, path, **kw):
    cs = ClientStorage(addr, cache=Proto(path=path, size=8 << 20, **kw), wait=True)
    return cs, ZODB.DB(cs)


def result(cs):
    return cs._server.client.verify_result


def read_all(db):
    conn = db.open()
    objs = conn.root()["o"]
    vals = [o.data for o in objs]
    conn.close()
    return vals


def write(addr, idx, value, txns=1):
    db = ZEO.DB(addr)
    for t in range(txns):
        conn = db.open()
        objs = conn.root()["o"]
        for i in idx:
            objs[i].data = f"{value}-{t}".encode()
        transaction.commit()
        conn.close()
    db.close()


def step(name, addr, path, expect_vals):
    cs, db = client(addr, path)
    res = result(cs)
    s0 = cs._cache.inner.stats()
    vals = read_all(db)
    s1 = cs._cache.inner.stats()
    out = {
        "step": name,
        "verify_result": res,
        "opened": cs._cache.inner.opened(),
        "hits": s1["hits"] - s0["hits"],
        "misses": s1["misses"] - s0["misses"],
        "newest_values": vals == expect_vals,
    }
    print(json.dumps(out), flush=True)
    db.close()
    return out


def main():
    tmp = tempfile.mkdtemp(prefix="verify-")
    addr, stop = ZEO.server(path=os.path.join(tmp, "Data.fs"), zeo_conf={"invalidation_queue_size": 5})
    cache = os.path.join(tmp, "c.zoec")
    cs, db = client(addr, cache)
    print(json.dumps({"step": "first open", "verify_result": result(cs)}))
    conn = db.open()
    conn.root()["o"] = [PObject(b"v0") for _ in range(N)]
    transaction.commit()
    conn.close()
    read_all(db)
    db.close()
    vals = [b"v0"] * N
    step("reopen, no change", addr, cache, vals)
    write(addr, [1, 2, 3], "v1")
    for i in (1, 2, 3):
        vals[i] = b"v1-0"
    step("reopen after 1 commit by another client", addr, cache, vals)
    write(addr, [4, 5], "v2", txns=20)
    for i in (4, 5):
        vals[i] = b"v2-19"
    step("reopen after 20 commits, queue of 5", addr, cache, vals)
    # The lock: a second private open of the same file fails.
    cs, db = client(addr, cache)
    try:
        Proto(path=cache, size=8 << 20)
        second = "opened"
    except OSError as e:
        second = f"OSError: {e}"
    print(json.dumps({"step": "second private open", "result": second}))
    try:
        Proto(path=cache, size=8 << 20, shared=True)
        shared = "opened"
    except OSError as e:
        shared = f"OSError: {e}"
    print(json.dumps({"step": "shared open of a private file", "result": shared}))
    db.close()
    stop()
    # A cache newer than the server: a fresh server, the same file.
    addr2, stop2 = ZEO.server(path=os.path.join(tmp, "Other.fs"))
    cs = ClientStorage(addr2, cache=Proto(path=cache, size=8 << 20), wait=False)
    deadline = time.time() + 10
    while result(cs) is None and time.time() < deadline:
        time.sleep(0.05)
    print(json.dumps({"step": "cache newer than the server", "verify_result": result(cs)}))
    cs.close()
    stop2()


if __name__ == "__main__":
    main()
