"""Unit tests of `zoe.plone.reader`, the subscriber that gives the reader of
ZOE to the main database of Zope, without Plone.

The subscriber gives the reader to a database on a ZOE storage, and to no
other database. A ZOE storage with `native_reader` false gets nothing. The
acceptance test, `zoe/plone/tests/test_reader_install.py`, starts Zope.
"""

import types

import ZODB
from ZODB.MappingStorage import MappingStorage
from ZODB.serialize import ObjectReader

import zoe
import zoe.reader
from zoe.plone import reader


def stock(db):
    """Give each connection of the pool of `db` ZODB's `ObjectReader`,
    whatever the switch of the tests."""
    for conn in db.pool:
        conn._reader = ObjectReader(conn, conn._cache, conn._reader._factory)


def opened(db):
    """Tell for each connection of the pool of `db` if it has the reader."""
    return [isinstance(conn._reader, zoe.reader.Reader) for conn in db.pool]


def test_a_database_on_zoe_storage_gets_the_reader(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store")))
    try:
        stock(db)
        reader.database_opened(types.SimpleNamespace(database=db))
        assert db.klass is zoe.reader.Connection
        assert opened(db) == [True]
    finally:
        db.close()


def test_native_reader_false_gives_nothing(tmp_path):
    db = ZODB.DB(zoe.Storage(str(tmp_path / "store"), native_reader=False))
    try:
        stock(db)
        reader.database_opened(types.SimpleNamespace(database=db))
        assert db.klass is ZODB.Connection.Connection
        assert opened(db) == [False]
    finally:
        db.close()


def test_another_storage_gets_nothing():
    db = ZODB.DB(MappingStorage())
    try:
        stock(db)
        reader.database_opened(types.SimpleNamespace(database=db))
        assert db.klass is ZODB.Connection.Connection
        assert opened(db) == [False]
    finally:
        db.close()


def test_a_zoe_client_is_a_zoe_storage():
    import zoe.zeo

    # Without wait, the native client does not connect.
    storage = zoe.zeo.ClientStorage(("127.0.0.1", 1), wait=False, wait_timeout=1)
    try:
        assert reader.is_zoe_storage(storage)
    finally:
        storage.close()
    assert not reader.is_zoe_storage(MappingStorage())
