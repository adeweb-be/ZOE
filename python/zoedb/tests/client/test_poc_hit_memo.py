"""POC ZOE-187 (not for merge): the memo of hits of the native client.

`ZOE_POC_HIT_MEMO=BOUND[:MIN_LEN]` gives a client, at its open, a memo of
at most BOUND bytes of the `bytes` of its hits of at least MIN_LEN bytes.
A hit of an entry whose copy the memo holds gives the same `bytes` object
and copies nothing. The client cache still decides each hit.

- Without the memo, two hits of an entry give two `bytes` objects.
- With the memo, two hits give the same object, and nothing reaches the
  server.
- A commit of another client: the next load gets the new data; the old
  snapshot still gets the old data.
- The memo holds at most its bound, also when the hits cover more.
- `_cache.clear()` empties the memo, and the next load reaches the server.
- A ring that wraps: each load gives the committed data of its object.
- Hits below MIN_LEN are not kept.
"""

import pytest
from test_native_cache import KB, ZERO, AdminServer, commit, counted, native, seen


@pytest.fixture
def server(tmp_path):
    with AdminServer(tmp_path / "store") as server:
        yield server


def open_with_memo(server, monkeypatch, memo, **kw):
    if memo is None:
        monkeypatch.delenv("ZOE_POC_HIT_MEMO", raising=False)
    else:
        monkeypatch.setenv("ZOE_POC_HIT_MEMO", memo)
    return native(server.address, **kw)


def stats(storage):
    return storage._native._poc_cache_stats()


def objects(writer, count, size):
    """Commit count objects of size bytes. Return their oids, data and tid."""
    oids = [writer.new_oid() for _ in range(count)]
    datas = [bytes([i % 251]) * size for i in range(count)]
    tid = commit(writer, [(oid, ZERO, d) for oid, d in zip(oids, datas, strict=True)])
    return oids, datas, tid


@pytest.mark.parametrize("memo", [None, "1000000"])
def test_two_hits_give_the_same_bytes_only_with_the_memo(server, monkeypatch, memo):
    writer = native(server.address)
    storage = open_with_memo(server, monkeypatch, memo)
    try:
        (oid,), (data,), tid = objects(writer, 1, 100 * KB)
        seen(storage, tid)
        first, loads = counted(server, lambda: storage.load(oid))
        assert loads == 1 and first == (data, tid)
        second, loads = counted(server, lambda: storage.load(oid))
        third, _ = counted(server, lambda: storage.load(oid))
        assert loads == 0 and second == (data, tid)
        assert (second[0] is third[0]) == (memo is not None)
        if memo:
            assert stats(storage)["memo_hits"] >= 1
    finally:
        storage.close()
        writer.close()


def test_a_commit_of_another_client_gives_the_new_data(server, monkeypatch):
    writer = native(server.address)
    storage = open_with_memo(server, monkeypatch, "1000000")
    try:
        (oid,), (data,), first = objects(writer, 1, 50 * KB)
        seen(storage, first)
        for _ in range(3):
            assert storage.load(oid) == (data, first)
        second = commit(writer, [(oid, first, b"new" * 1000)])
        seen(storage, second)
        assert storage.load(oid) == (b"new" * 1000, second)
        assert tuple(storage.loadBefore(oid, second)) == (data, first, second)
        assert tuple(storage.loadBefore(oid, second)) == (data, first, second)
    finally:
        storage.close()
        writer.close()


def test_the_memo_holds_at_most_its_bound(server, monkeypatch):
    writer = native(server.address)
    bound = 300 * KB
    storage = open_with_memo(server, monkeypatch, str(bound), cache_size=20_000 * KB)
    try:
        oids, datas, tid = objects(writer, 40, 30 * KB)
        seen(storage, tid)
        for _ in range(3):
            for oid, data in zip(oids, datas, strict=True):
                assert storage.load(oid) == (data, tid)
        s = stats(storage)
        assert s["memo_bytes"] <= bound, s
        assert s["memo_dropped"] + s["memo_refused"] > 0, s
    finally:
        storage.close()
        writer.close()


def test_clear_empties_the_memo(server, monkeypatch):
    writer = native(server.address)
    storage = open_with_memo(server, monkeypatch, "1000000")
    try:
        (oid,), (data,), tid = objects(writer, 1, 10 * KB)
        seen(storage, tid)
        storage.load(oid)
        storage.load(oid)
        assert stats(storage)["memo_copies"] == 1
        storage._cache.clear()
        assert stats(storage)["memo_copies"] == 0
        assert counted(server, lambda: storage.load(oid)) == ((data, tid), 1)
    finally:
        storage.close()
        writer.close()


def test_a_ring_that_wraps_gives_the_committed_data(server, monkeypatch):
    writer = native(server.address)
    storage = open_with_memo(server, monkeypatch, "10000000", cache_size=400 * KB)
    try:
        oids, datas, tid = objects(writer, 30, 50 * KB)
        seen(storage, tid)
        for _ in range(3):
            for oid, data in zip(oids, datas, strict=True):
                assert storage.load(oid) == (data, tid)
            for oid, data in reversed(list(zip(oids, datas, strict=True))):
                assert storage.load(oid) == (data, tid)
    finally:
        storage.close()
        writer.close()


def test_hits_below_the_least_length_are_not_kept(server, monkeypatch):
    writer = native(server.address)
    storage = open_with_memo(server, monkeypatch, f"1000000:{16 * KB}")
    try:
        (small,), _, tid = objects(writer, 1, 1 * KB)
        seen(storage, tid)
        for _ in range(3):
            storage.load(small)
        assert stats(storage)["memo_copies"] == 0
    finally:
        storage.close()
        writer.close()
