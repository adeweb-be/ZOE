"""Acceptance test of the loads of large records from a stock client (N4).

`zoe serve` sends the reply of a load from the buffer of the record, in
parts. An unmodified `ZEO.ClientStorage` still gets the same bytes: the
pickle that it stored, for records around the limits of the bin types of
msgpack, of 100 KB and of 1 MiB, of bytes that compress and of bytes that
do not, through `load`, `loadBefore` and `prefetch`.
"""

import functools
import pickle
import random

import support
from support import ZERO, client

# The transactions of the test have this user and this description.
commit = functools.partial(support.commit, user=b"user", description=b"large")

# Around 255 and 65,535 bytes, the limits of bin 8 and bin 16, then 100 KB
# and 1 MiB.
SIZES = [200, 300, 65_000, 66_000, 100_000, 1 << 20]


def record(size, compressible, seed):
    if compressible:
        words = [b"word%d" % n for n in range(500)]
        rng = random.Random(seed)
        body = b" ".join(rng.choice(words) for _ in range(size // 5))[:size]
    else:
        body = random.Random(seed).randbytes(size)
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": body}, 3)


def test_a_stock_client_gets_the_bytes_of_each_large_record(server):
    from ZODB.utils import p64, u64

    writer = client(server, client_label="large-writer")
    try:
        stores = []
        for n, size in enumerate(SIZES):
            for compressible in (True, False):
                data = record(size, compressible, seed=2 * n + compressible)
                stores.append((writer.new_oid(), ZERO, data))
        tid = commit(writer, stores)
        changed = record(100_000, True, seed=99)
        later = commit(writer, [(stores[0][0], tid, changed)])
    finally:
        writer.close()

    # A new client has an empty cache, so each load goes to the server.
    reader = client(server, client_label="large-reader")
    try:
        for oid, _, data in stores[1:]:
            assert reader.load(oid) == (data, tid)
        before = p64(u64(tid) + 1)
        # The first record has a later version: its load before it gives
        # the next serial.
        assert reader.loadBefore(stores[0][0], before) == (stores[0][2], tid, later)
        assert reader.load(stores[0][0]) == (changed, later)
    finally:
        reader.close()

    reader = client(server, client_label="large-prefetch")
    try:
        oids = [oid for oid, _, _ in stores[1:]]
        reader.prefetch(oids, p64(u64(later) + 1))
        for oid, _, data in stores[1:]:
            assert reader.loadBefore(oid, p64(u64(later) + 1)) == (data, tid, None)
    finally:
        reader.close()
