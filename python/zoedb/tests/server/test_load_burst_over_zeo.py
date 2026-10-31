"""Acceptance test of a burst of loads from a stock client (N2, N4).

An unmodified `ZEO.ClientStorage` sends bursts of loads with `prefetch`
while another client commits. `zoe serve` answers the loads of one
connection concurrently, and each reply comes after the invalidation of
each commit that it shows and before the invalidation of each later
commit. So each load gives the data that the writer committed at its
serial, never a serial past the last transaction of the client, and after
the last invalidation each object shows its last commit. A next serial can
be past that transaction: its invalidation came before the reply.
"""

import functools
import pickle
import threading
import time

import support
from support import TIMEOUT, ZERO, client

# The transactions of the test have this user and this description.
commit = functools.partial(support.commit, user=b"user", description=b"burst")

OBJECTS = 20
COMMITS = 40


def record(oid, n):
    meta = (("persistent.mapping", "PersistentMapping"), None)
    return pickle.dumps(meta, 3) + pickle.dumps({"data": {"oid": oid, "n": n}}, 3)


def test_bursts_of_prefetched_loads_keep_the_order_of_the_invalidations(server):
    from ZODB.utils import p64, u64

    writer = client(server, client_label="burst-writer")
    reader = client(server, client_label="burst-reader")
    try:
        oids = [writer.new_oid() for _ in range(OBJECTS)]
        first = commit(writer, [(oid, ZERO, record(oid, 0)) for oid in oids])
        written = {(oid, first): record(oid, 0) for oid in oids}
        serials = {oid: first for oid in oids}
        failures = []

        def commits():
            try:
                for n in range(1, COMMITS + 1):
                    picked = {oids[(n * 7 + k * 5) % OBJECTS] for k in range(3)}
                    stores = [(oid, serials[oid], record(oid, n)) for oid in picked]
                    tid = commit(writer, stores)
                    for oid, _, data in stores:
                        serials[oid] = tid
                        written[(oid, tid)] = data
            except Exception as error:
                failures.append(error)

        committing = threading.Thread(target=commits)
        committing.start()
        loaded = []
        while committing.is_alive():
            last = reader.lastTransaction()
            before = p64(u64(last) + 1)
            # One burst: a load of each object that the cache does not
            # hold, each sent before any reply.
            reader.prefetch(oids, before)
            for oid in oids:
                data, serial, end = reader.loadBefore(oid, before)
                assert serial <= last
                assert end is None or serial < end
                loaded.append((oid, serial, data))
            # The writer and the thread of the client get the GIL.
            time.sleep(0.002)
        committing.join()
        assert not failures, failures
        # The reader sent more than one burst while the writer committed.
        assert len(loaded) > OBJECTS

        # Each load gave the data that the writer committed at its serial.
        for oid, serial, data in loaded:
            assert written[(oid, serial)] == data

        # After the last invalidation, each object shows its last commit.
        last = max(serials.values())
        for _ in range(TIMEOUT * 10):
            if reader.lastTransaction() >= last:
                break
            time.sleep(0.1)
        assert reader.lastTransaction() == last
        before = p64(u64(last) + 1)
        reader.prefetch(oids, before)
        for oid in oids:
            data, serial, end = reader.loadBefore(oid, before)
            assert (serial, end) == (serials[oid], None)
            assert data == written[(oid, serial)]
    finally:
        reader.close()
        writer.close()
