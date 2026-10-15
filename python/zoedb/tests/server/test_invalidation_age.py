"""Acceptance test: `zoe serve --invalidation-age SECONDS`, as in ZEO.

`getInvalidations(tid)` gives `None` when more than
`--invalidation-queue-size` transactions that wrote records come after
`tid`, and the client then clears or verifies its cache. With
`--invalidation-age SECONDS` (`ZOE_INVALIDATION_AGE`), the server also
replays the transactions after `tid` from the transaction index of the
store when the time of `tid` is less than SECONDS before the clock of the
server, whatever their number, as ZEO's server does with the key
`invalidation-age` of `<zeo>`. The option takes whole seconds, as
`--idle-timeout` does. The default, 0, keeps the queue size as the only
limit.
"""

import time

import pytest

from zoe.testing import ZoeServer

TIMEOUT = 30


def client(server, **kw):
    from ZEO.ClientStorage import ClientStorage

    kw.setdefault("wait_timeout", TIMEOUT)
    return ClientStorage(server.address, storage="1", **kw)


def commit(storage, oids, serials):
    from ZODB.Connection import TransactionMetaData

    txn = TransactionMetaData()
    storage.tpc_begin(txn)
    for oid in oids:
        storage.store(oid, serials.get(oid, bytes(8)), b"x" * 10, "", txn)
    storage.tpc_vote(txn)
    tid = storage.tpc_finish(txn)
    for oid in oids:
        serials[oid] = tid
    return tid


def invalidations_after_five_commits(server, wait=0):
    """Commit once, wait, commit five times. Return getInvalidations.

    Also return the last tid and the oids of the five commits.
    """
    storage = client(server)
    try:
        serials = {}
        a, b, c = (storage.new_oid() for _ in range(3))
        first = commit(storage, [a], serials)
        time.sleep(wait)
        for oid in [b, c, b, c, b]:
            last = commit(storage, [oid], serials)
        return storage._call("getInvalidations", first), last, {b, c}
    finally:
        storage.close()


@pytest.fixture
def path(tmp_path):
    return str(tmp_path / "store")


def test_a_recent_tid_is_replayed_beyond_the_queue_size(path):
    args = ["--invalidation-queue-size", "2", "--invalidation-age", "3600"]
    with ZoeServer(path, args=args) as server:
        answer, last, oids = invalidations_after_five_commits(server)
    assert answer is not None
    tid, invalidated = answer
    assert tid == last
    assert set(invalidated) == oids
    assert len(invalidated) == len(oids)


def test_zoe_invalidation_age_is_the_twin_of_the_option(path):
    args = ["--invalidation-queue-size", "2"]
    env = {"ZOE_INVALIDATION_AGE": "3600"}
    with ZoeServer(path, args=args, env=env) as server:
        answer, last, oids = invalidations_after_five_commits(server)
    assert answer is not None
    assert answer[0] == last
    assert set(answer[1]) == oids


def test_a_tid_older_than_the_age_is_not_replayed(path):
    args = ["--invalidation-queue-size", "2", "--invalidation-age", "1"]
    with ZoeServer(path, args=args) as server:
        answer, _, _ = invalidations_after_five_commits(server, wait=2.5)
    assert answer is None


def test_without_the_age_the_queue_size_is_the_limit(path):
    args = ["--invalidation-queue-size", "2"]
    with ZoeServer(path, args=args) as server:
        answer, _, _ = invalidations_after_five_commits(server)
    assert answer is None
