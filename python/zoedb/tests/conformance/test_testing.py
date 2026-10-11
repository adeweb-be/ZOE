"""Unit tests of `zoe.testing`."""

import os

import pytest
import transaction
import ZODB
from ZODB.POSException import ReadOnlyError, StorageError
from ZODB.tests.MinPO import MinPO
from ZODB.tests.StorageTestBase import zodb_unpickle

import zoe
import zoe.testing


def test_a_store_stays_open_until_the_layer_tears_down():
    layer = zoe.testing.StorageLayer("TEST_LAYER")
    layer.setUp()
    directory = layer._directory
    try:
        layer.testSetUp()
        db = ZODB.DB(None)
        conn = db.open()
        layer.testTearDown()

        # A later test commits through the connection that a test left
        # open.
        layer.testSetUp()
        conn.root()["key"] = "value"
        transaction.commit()
        layer.testTearDown()
    finally:
        layer.tearDown()

    assert not os.path.exists(directory)
    with pytest.raises(StorageError):
        db.storage.lastTransaction()
    # The connection to the closed store does not take part in a later
    # commit.
    transaction.commit()


def test_each_database_of_a_test_has_a_new_store():
    layer = zoe.testing.StorageLayer("TEST_LAYER")
    layer.setUp()
    try:
        layer.testSetUp()
        try:
            dbs = [ZODB.DB(None), ZODB.DB(None)]
            names = {db.storage.getName() for db in dbs}
            assert len(names) == 2
            for db in dbs:
                db.close()
        finally:
            layer.testTearDown()
        real = os.path.realpath(layer._directory)
        assert all(name.startswith(real + os.sep) for name in names)
    finally:
        layer.tearDown()


class TestStorageTestCase(zoe.testing.StorageTestCase):
    def test_open_opens_the_same_store_again(self):
        oid = self._storage.new_oid()
        self._dostore(oid, data=MinPO(7))
        self._storage.close()
        self.open(read_only=True)
        assert zodb_unpickle(self._storage.load(oid)[0]).value == 7
        with pytest.raises(ReadOnlyError):
            self._storage.new_oid()
