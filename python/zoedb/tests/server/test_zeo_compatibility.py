r"""Acceptance test: `zoe serve` answers as ZEO's server where ZEO's suites look.

- A serial that a client sends as a str of 8 bytes, such as ZEO's tests
  send with `'\0' * 8`, reads as those 8 bytes, as ZEO's server takes it.
  So a store of a new object with that serial commits, and a store of an
  object that has a record with that serial is a conflict. A str serial
  of another length still fails with `TypeError`.
- `history(oid, size)` with an oid that is not 8 bytes raises
  `POSKeyError`, as ZEO's server does, before it checks the size. A size
  that is not an int still raises `TypeError`.
- `iterator_gc` is an async call: the client does not wait for it. It
  takes effect before the later calls of its connection, so an
  `iterator_next` for that iterator raises `KeyError`. A second
  `iterator_gc` of the same iterator does nothing.
"""

import pytest
from support import client, commit

from zoe.testing import ZoeServer

STR_ZERO = "\0" * 8


@pytest.fixture
def storage(tmp_path):
    with ZoeServer(str(tmp_path / "store")) as server:
        storage = client(server)
        try:
            yield storage
        finally:
            storage.close()


def test_a_str_serial_of_eight_bytes_reads_as_those_bytes(storage):
    from ZODB.POSException import ConflictError

    oid = storage.new_oid()
    tid = commit(storage, [(oid, STR_ZERO, b"first")])
    assert storage.load(oid) == (b"first", tid)

    # The object has a record now: the serial of a new object conflicts.
    with pytest.raises(ConflictError):
        commit(storage, [(oid, STR_ZERO, b"second")])
    assert storage.load(oid) == (b"first", tid)

    # The serial as bytes still works.
    tid2 = commit(storage, [(oid, tid, b"second")])
    assert storage.load(oid) == (b"second", tid2)


def test_a_str_serial_of_another_length_still_fails(storage):
    oid = storage.new_oid()
    with pytest.raises(TypeError):
        commit(storage, [(oid, "\0" * 7, b"data")])


def test_history_of_an_oid_that_is_not_bytes_raises_poskeyerror(storage):
    from ZODB.POSException import POSKeyError

    oid = storage.new_oid()
    commit(storage, [(oid, bytes(8), b"data")])

    with pytest.raises(POSKeyError):
        storage.history(None, None)
    with pytest.raises(POSKeyError):
        storage.history(None, 1)
    with pytest.raises(TypeError):
        storage.history(oid, None)
    assert len(storage.history(oid, 1)) == 1


def test_iterator_gc_takes_effect_before_the_next_call(storage):
    oid = storage.new_oid()
    serial = commit(storage, [(oid, bytes(8), b"0")])
    for i in range(30):
        # The iterator object goes at once, so the client forgets it, and
        # tpc_finish then sends iterator_gc without waiting.
        next(storage.iterator())
        [iid] = storage._iterator_ids
        serial = commit(storage, [(oid, serial, str(i + 1).encode())])
        assert not storage._iterator_ids
        with pytest.raises(KeyError):
            storage._call("iterator_next", iid)


def test_a_second_iterator_gc_of_an_iterator_does_nothing(storage):
    oid = storage.new_oid()
    commit(storage, [(oid, bytes(8), b"data")])
    next(storage.iterator())
    [iid] = storage._iterator_ids
    storage._async("iterator_gc", [iid])
    storage._async("iterator_gc", [iid])
    with pytest.raises(KeyError):
        storage._call("iterator_next", iid)
    assert storage.load(oid)[0] == b"data"
