"""The process kill loop against `zoe serve` (harness C2 and C4, N1).

Each round starts the server on the same store, runs several writers that
commit objects and blobs through unmodified `ZEO.ClientStorage` clients,
and kills the server with SIGKILL at a random point: inside `tpc_finish`
too, since the writers spend most of their time there. Then it restarts
the server, stops it with SIGTERM, runs `zoe verify` on the store, and
checks that every commit that a writer saw acknowledged loads with its
data, and that every acknowledged blob has its bytes. A blob file without
its record, or a record without its file, fails `zoe verify`.

`just crash` runs this module with `ZOE_KILL_LOOP_RUNS` rounds, 10,000 in
the nightly job, and the seed `ZOE_KILL_LOOP_SEED`. `just test` does not
collect it. The phase 2 gate asks for 0 losses.
"""

import os
import pickle
import random
import subprocess
import sysconfig
import threading
import time
from pathlib import Path

from zoe.testing import ZoeServer

ZERO = bytes(8)
TIMEOUT = 30
RUNS = int(os.environ.get("ZOE_KILL_LOOP_RUNS", "20"))
SEED = int(os.environ.get("ZOE_KILL_LOOP_SEED", "1"))
WRITERS = 4


def record(value):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"value": value}}, 3)


BLOB_RECORD = pickle.dumps((("ZODB.blob", "Blob"), None), 3) + pickle.dumps(None, 3)


def client(server, cache, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", blob_dir=str(cache), **kw)


def writer(server, cache, rng_seed, done, out):
    """Commit until done is set or the server dies. Append each
    acknowledged commit to out as (oid, tid, data, blob bytes or None)."""
    from ZEO.Exceptions import ClientDisconnected
    from ZODB.Connection import TransactionMetaData

    rng = random.Random(rng_seed)
    storage = client(server, cache, wait_timeout=1)
    try:
        n = 0
        while not done.is_set():
            n += 1
            oid = storage.new_oid()
            txn = TransactionMetaData()
            storage.tpc_begin(txn)
            if n % 3 == 0:
                blob = rng.randbytes(rng.randrange(1, 300_000))
                path = Path(cache) / f"upload-{rng_seed}-{n}"
                path.write_bytes(blob)
                storage.storeBlob(oid, ZERO, BLOB_RECORD, str(path), "", txn)
                data = BLOB_RECORD
            else:
                blob = None
                data = record(f"{rng_seed}-{n}")
                storage.store(oid, ZERO, data, "", txn)
            storage.tpc_vote(txn)
            tid = storage.tpc_finish(txn)
            out.append((oid, tid, data, blob))
    except (ClientDisconnected, OSError):
        pass
    except Exception as error:
        if not done.is_set():
            out.append(error)
    finally:
        try:
            storage.close()
        except Exception:
            pass


def verify(path):
    script = Path(sysconfig.get_path("scripts")) / "zoe"
    return subprocess.run(
        [str(script), "verify", "--path", str(path)],
        capture_output=True,
        text=True,
        check=False,
    )


def test_no_acknowledged_commit_is_lost_through_kills(tmp_path):
    rng = random.Random(SEED)
    path = tmp_path / "store"
    acknowledged = []
    with ZoeServer(str(path)) as server:
        for run in range(RUNS):
            done = threading.Event()
            outs = [[] for _ in range(WRITERS)]
            threads = [
                threading.Thread(
                    target=writer,
                    args=(server, tmp_path / f"cache-{w}", f"{run}-{w}", done, outs[w]),
                    daemon=True,
                )
                for w in range(WRITERS)
            ]
            for thread in threads:
                thread.start()
            time.sleep(rng.uniform(0.05, 0.5))
            server.kill()
            done.set()
            for thread in threads:
                thread.join(TIMEOUT)
            for out in outs:
                errors = [item for item in out if isinstance(item, Exception)]
                assert not errors, errors
                acknowledged.extend(out)

            # Restart, stop cleanly, and verify the store offline.
            server.start()
            assert server.stop() == 0
            result = verify(path)
            assert result.returncode == 0, (
                f"round {run}:\n{result.stdout}{result.stderr}"
            )
            server.start()

            storage = client(server, tmp_path / "cache-check")
            try:
                lost = []
                for oid, tid, data, blob in acknowledged:
                    if storage.load(oid) != (data, tid):
                        lost.append(oid)
                    elif blob is not None:
                        with open(storage.loadBlob(oid, tid), "rb") as f:
                            if f.read() != blob:
                                lost.append(oid)
            finally:
                storage.close()
            assert lost == [], f"round {run} lost {len(lost)} acknowledged commits"
    assert acknowledged, "no commit was acknowledged"
