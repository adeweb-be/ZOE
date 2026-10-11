"""Acceptance test of sequences in-process.

``storage.sequence(name).next()`` on a ``zoe.Storage`` returns values that are
unique and strictly increasing per store and per name. A value is never given
twice, also after a reopen. The allocation is not part of a transaction, so an
aborted transaction does not give its values back, and threads that commit
take values without a conflict.
"""

import threading

import pytest
import transaction
import ZODB
from persistent.mapping import PersistentMapping

import zoe

# The largest number of values that a reopen or a crash can skip.
MAX_GAP = 100


@pytest.fixture
def path(tmp_path):
    return str(tmp_path / "store")


def test_values_start_at_one_and_increase_by_one(path):
    storage = zoe.Storage(path)
    try:
        sequence = storage.sequence("intids")
        # 250 values cross two block boundaries.
        values = [sequence.next() for _ in range(250)]
    finally:
        storage.close()
    assert values == list(range(1, 251))
    assert all(type(value) is int for value in values)


def test_each_name_has_its_own_sequence(path):
    storage = zoe.Storage(path)
    try:
        intids = storage.sequence("intids")
        lexicon = storage.sequence("lexicon")
        assert [intids.next(), intids.next()] == [1, 2]
        assert lexicon.next() == 1
        assert intids.next() == 3
        assert storage.sequence("lexicon").next() == 2
    finally:
        storage.close()


def test_values_keep_increasing_after_a_reopen(path):
    storage = zoe.Storage(path)
    try:
        last = [storage.sequence("intids").next() for _ in range(5)][-1]
    finally:
        storage.close()

    storage = zoe.Storage(path)
    try:
        first = storage.sequence("intids").next()
    finally:
        storage.close()
    assert last < first <= last + MAX_GAP + 1


def test_values_are_unique_across_threads(path):
    storage = zoe.Storage(path)
    results = [None] * 8

    def allocate(index):
        sequence = storage.sequence("intids")
        results[index] = [sequence.next() for _ in range(200)]

    threads = [threading.Thread(target=allocate, args=(i,)) for i in range(8)]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    finally:
        storage.close()

    for values in results:
        assert values == sorted(set(values))
    every = sorted(value for values in results for value in values)
    # No value came twice, and without a crash no value was skipped.
    assert every == list(range(1, 1601))


def test_an_aborted_transaction_does_not_give_its_values_back(path):
    db = ZODB.DB(zoe.Storage(path))
    try:
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        manager.begin()
        connection.root()["intid"] = db.storage.sequence("intids").next()
        manager.abort()
        connection.close()
        assert db.storage.sequence("intids").next() == 2
    finally:
        db.close()


def test_committing_threads_take_ids_without_a_conflict(path):
    db = ZODB.DB(zoe.Storage(path))
    try:
        with db.transaction() as connection:
            for index in range(4):
                connection.root()[f"folder-{index}"] = PersistentMapping()
        errors = []

        def create(index):
            manager = transaction.TransactionManager()
            connection = db.open(transaction_manager=manager)
            try:
                for _ in range(25):
                    with manager:
                        folder = connection.root()[f"folder-{index}"]
                        intid = db.storage.sequence("intids").next()
                        folder[intid] = PersistentMapping(intid=intid)
            except Exception as error:
                errors.append(error)
            finally:
                connection.close()

        threads = [threading.Thread(target=create, args=(i,)) for i in range(4)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        assert errors == []

        with db.transaction() as connection:
            root = connection.root()
            ids = sorted(
                intid for index in range(4) for intid in root[f"folder-{index}"]
            )
        assert ids == list(range(1, 101))
    finally:
        db.close()
