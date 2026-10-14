"""Unit tests of `zoe.plone.uidgenerator`, without Plone.

A `PersistentMapping` with the attribute `_uid_counter` stands for
`portal_uidgenerator`: `next_id` reads only that attribute and the range
`_zoe_ids` of the tool.
"""

import threading

import pytest
import transaction
import ZODB
from BTrees.Length import Length
from persistent.mapping import PersistentMapping
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import Unsupported

import zoe
from zoe.plone import uidgenerator
from zoe.plone.uidgenerator import BLOCK, STATE, next_id, sequence_name

# The ids that the stock counter of the site gave.
SEED = 100_000


@pytest.fixture
def db(tmp_path):
    database = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    yield database
    database.close()


def add_tool(db, counter=SEED):
    """Add the tool to the root of `db`, with `counter` as `_uid_counter`."""
    with db.transaction() as connection:
        tool = PersistentMapping()
        tool._uid_counter = counter
        connection.root()["tool"] = tool


def take(db, count, name="ids"):
    """Take `count` ids, each in a transaction that commits a change.

    Return the ids. Check that the transaction never writes the tool.
    """
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    taken = []
    try:
        for _ in range(count):
            with manager:
                root = connection.root()
                uid = next_id(root["tool"])
                assert not root["tool"]._p_changed
                log = root.get(name)
                if log is None:
                    log = root[name] = PersistentMapping()
                log[uid] = True
                taken.append(uid)
    finally:
        connection.close()
    return taken


def tool_state(db):
    """Return `_uid_counter` and the range of the tool, as committed."""
    with db.transaction() as connection:
        tool = connection.root()["tool"]
        return tool._uid_counter, getattr(tool, STATE, None)


def test_a_storage_that_is_not_zoe_keeps_the_stock_counter():
    database = ZODB.DB(MappingStorage())
    try:
        add_tool(database)
        with database.transaction() as connection:
            assert next_id(connection.root()["tool"]) is None
    finally:
        database.close()


def test_a_tool_without_a_database_keeps_the_stock_counter():
    tool = PersistentMapping()
    tool._uid_counter = SEED
    assert next_id(tool) is None


def test_a_tool_that_the_transaction_changes_keeps_the_stock_counter(db):
    add_tool(db)
    with db.transaction() as connection:
        tool = connection.root()["tool"]
        tool._uid_counter += 1
        assert next_id(tool) is None


def test_a_new_tool_keeps_the_stock_counter(db):
    with db.transaction() as connection:
        tool = PersistentMapping()
        tool._uid_counter = SEED
        connection.root()["tool"] = tool
        connection.savepoint()
        assert tool._p_jar is not None
        tool._p_changed = False
        assert next_id(tool) is None


def test_ids_are_above_the_counter_and_unique(db):
    add_tool(db)
    taken = take(db, 3 * BLOCK + 7)
    assert all(type(uid) is int for uid in taken)
    assert min(taken) > SEED
    assert len(set(taken)) == len(taken)
    assert taken == sorted(taken)


def test_the_counter_covers_every_id_given(db):
    add_tool(db)
    for _ in range(5):
        taken = take(db, 37)
        counter, state = tool_state(db)
        assert counter == state[3]
        assert max(taken) <= counter


def test_the_tool_gets_one_write_for_each_block(db):
    add_tool(db)
    take(db, 1)
    _, first = tool_state(db)
    take(db, BLOCK)
    assert tool_state(db)[1] == first
    take(db, 1)
    assert tool_state(db)[1] != first


def test_a_tool_that_is_a_ghost_takes_its_id_from_the_sequence(db):
    add_tool(db)
    take(db, 3)
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        with manager:
            tool = connection.root()["tool"]
            connection.cacheMinimize()
            assert tool._p_changed is None
            uid = next_id(tool)
            assert uid is not None
            assert not tool._p_changed
    finally:
        connection.close()
    counter, _ = tool_state(db)
    assert SEED < uid <= counter


def test_each_tool_has_its_own_sequence(db):
    add_tool(db)
    with db.transaction() as connection:
        other = PersistentMapping()
        other._uid_counter = SEED
        connection.root()["other"] = other
    with db.transaction() as connection:
        root = connection.root()
        assert sequence_name(root["tool"]) != sequence_name(root["other"])
        first = [next_id(root["tool"]) for _ in range(3)]
        second = [next_id(root["other"]) for _ in range(3)]
    assert len(set(first)) == 3
    assert len(set(second)) == 3


def test_an_old_counter_that_is_a_length_counts(db):
    add_tool(db, counter=Length(SEED))
    taken = take(db, 5)
    assert min(taken) > SEED
    counter, _ = tool_state(db)
    assert type(counter) is int
    assert counter >= max(taken)


def test_ids_of_the_stock_counter_meanwhile_are_not_given_again(db):
    add_tool(db)
    wired = take(db, 10)
    # The wiring stops, and the stock counter gives ids from the counter.
    with db.transaction() as connection:
        tool = connection.root()["tool"]
        stock = [tool._uid_counter + 1, tool._uid_counter + 2]
        tool._uid_counter += 2
    assert min(stock) > max(wired)
    # The wiring starts again above the ids of the stock counter.
    again = take(db, 10)
    assert min(again) > max(stock)


def copy_store(db, tmp_path, lost=0):
    """Copy the store of `db` into a new store, and return its database.

    The sequence of the tool in the new store gives `lost` values before
    the copy is used, as reservations that fail after their value do.
    """
    with db.transaction() as connection:
        name = sequence_name(connection.root()["tool"])
    copy = zoe.Storage(str(tmp_path / "copy"))
    copy.copyTransactionsFrom(db.storage)
    for _ in range(lost):
        copy.sequence(name).next()
    return ZODB.DB(copy)


@pytest.mark.parametrize("lost", [0, 1, BLOCK])
def test_a_copy_into_a_new_store_gives_no_id_again(db, tmp_path, lost):
    add_tool(db)
    before = take(db, 25)
    # The sequence of the new store starts again at 1.
    database = copy_store(db, tmp_path, lost)
    try:
        after = take(database, 25)
    finally:
        database.close()
    assert min(after) > max(before)
    assert len(set(after)) == len(after)


def test_threads_after_a_copy_give_no_id_again(db, tmp_path):
    add_tool(db)
    before = take(db, 2 * BLOCK + 5)
    database = copy_store(db, tmp_path)
    with database.transaction() as connection:
        for index in range(8):
            connection.root()[f"ids-{index}"] = PersistentMapping()
    results = {}
    errors = []

    def creator(index):
        try:
            results[index] = take(database, 10, name=f"ids-{index}")
        except Exception as error:
            errors.append(error)

    threads = [threading.Thread(target=creator, args=(i,)) for i in range(8)]
    try:
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
    finally:
        database.close()
    assert errors == []
    after = [uid for taken in results.values() for uid in taken]
    assert len(set(after)) == len(after) == 80
    assert min(after) > max(before)


def test_a_server_without_sequences_keeps_the_stock_counter(db, monkeypatch):
    add_tool(db)

    class Sequence:
        def next(self):
            raise Unsupported("no sequences")

    monkeypatch.setattr(type(db.storage), "sequence", lambda self, name: Sequence())
    with db.transaction() as connection:
        assert next_id(connection.root()["tool"]) is None
    assert tool_state(db) == (SEED, None)


def test_threads_take_unique_ids_without_writing_the_tool(db):
    add_tool(db)
    # Each thread writes its own log, so the threads do not conflict on the
    # root.
    with db.transaction() as connection:
        for index in range(8):
            connection.root()[f"ids-{index}"] = PersistentMapping()
    results = {}
    errors = []

    def creator(index):
        try:
            results[index] = take(db, 40, name=f"ids-{index}")
        except Exception as error:
            errors.append(error)

    threads = [threading.Thread(target=creator, args=(i,)) for i in range(8)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    assert errors == []
    every = [uid for taken in results.values() for uid in taken]
    assert len(every) == 8 * 40
    assert len(set(every)) == len(every)
    assert min(every) > SEED
    counter, _ = tool_state(db)
    assert counter >= max(every)


def test_install_needs_plone():
    # The wheel never imports Plone: only `install` does.
    try:
        import Products.CMFUid  # noqa: F401
    except ImportError:
        with pytest.raises(ImportError):
            uidgenerator.install()
    else:
        pytest.skip("Products.CMFUid is installed")
