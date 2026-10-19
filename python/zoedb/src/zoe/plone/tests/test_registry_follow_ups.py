"""Acceptance tests of the follow-ups of the registry backend of zoe.plone
(ADR 0050): a rollback to a savepoint that staged a field, `reset` on a ZEO
client without kv, and the work of `keys(min, max)` and `values()`.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, as `just plone zoe backends` runs them. They install
the backend themselves. The test of `reset` starts `zoe serve`: the `zoe`
binary of `zoe.testing.ZoeServer.binary()` must exist.
"""

import collections
import shutil
import tempfile
import unittest
from unittest import mock

import transaction
from Acquisition import aq_base
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, PLONE_SITE_ID
from ZODB.DB import DB
from ZODB.POSException import Unsupported

from zoe.plone import registry as backend
from zoe.plone import registry_rows
from zoe.plone.registry_rows import MARKER, decode_marker
from zoe.plone.tests.test_concurrent_creations import copy_records
from zoe.plone.tests.test_registry_backend import TITLE, namespace, opened

TIMEOUT = 30


class Bound(str):
    """A bound of `keys(min, max)` that counts the comparisons with it.

    A comparison of a name with an instance of this subclass of str calls
    a method of this class first, also when the name is on the left.
    """

    compared = 0

    def __lt__(self, other):
        Bound.compared += 1
        return str.__lt__(self, other)

    def __le__(self, other):
        Bound.compared += 1
        return str.__le__(self, other)

    def __gt__(self, other):
        Bound.compared += 1
        return str.__gt__(self, other)

    def __ge__(self, other):
        Bound.compared += 1
        return str.__ge__(self, other)


class TestRegistryFollowUps(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        backend.install()
        self.addCleanup(backend.uninstall)
        self.portal = self.layer["portal"]
        self.db = self.layer["zodbDB"]
        self.registry = self.portal.portal_registry

    def moved(self):
        """Move the registry of the site and commit. Return it."""
        backend.move(self.registry)
        transaction.commit()
        return self.registry

    # A rollback to a savepoint.

    def test_a_rollback_to_a_savepoint_that_staged_a_field_keeps_the_counter(self):
        registry = self.moved()
        before = registry.records[TITLE].field.title
        transaction.commit()
        # The connection joins the transaction before the first read of the
        # registry. So its savepoint comes before the savepoint of the
        # backend, which stages the field that changed in place.
        self.portal.zoe_test_attribute = "Joined first"
        registry.records[TITLE].field.title = "Changed in place"
        savepoint = transaction.savepoint()
        # The savepoint kept the field: the rollback keeps it too.
        savepoint.rollback()
        transaction.commit()
        counter = aq_base(registry)._records.counter
        marker = decode_marker(namespace(registry).get(MARKER))
        self.assertGreater(counter, 1, "the commit wrote no counter")
        self.assertEqual(marker[1], counter, "the marker and the registry differ")
        with opened(self.db) as (app, _):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            self.assertNotEqual(
                field.title, before, "another connection reads stale rows"
            )
            self.assertEqual(field.title, "Changed in place")

    # `reset` on a ZEO client without kv.

    def served_copy(self):
        """Copy the site to a new `zoe.Storage`, move its registry there,
        write a record and commit. Return the path of the store, closed."""
        import zoe

        directory = tempfile.mkdtemp(prefix="zoe-registry-reset-")
        self.addCleanup(shutil.rmtree, directory, ignore_errors=True)
        path = f"{directory}/zoe"
        storage = zoe.Storage(path)
        try:
            copy_records(self.db.storage, storage)
        except BaseException:
            storage.close()
            raise
        db = DB(storage)
        try:
            with opened(db) as (app, manager):
                registry = app[PLONE_SITE_ID].portal_registry
                backend.move(registry)
                manager.commit()
                registry[TITLE] = "Kept on the server"
                manager.commit()
        finally:
            db.close()
        return path

    def test_reset_refuses_a_zeo_client_without_kv_of_a_store_with_its_rows(self):
        from ZEO.ClientStorage import ClientStorage

        import zoe.zeo
        from zoe.testing import ZoeServer

        path = self.served_copy()
        with ZoeServer(path) as server:
            # A stock client of ZEO: the server does not give it kv.
            stock = DB(ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT))
            try:
                with opened(stock) as (app, manager):
                    registry = app[PLONE_SITE_ID].portal_registry
                    with self.assertRaises(Unsupported):
                        registry[TITLE]
                    manager.abort()
                    # The client cannot tell whether the server holds the
                    # rows. So reset refuses, and the registry stays moved.
                    with self.assertRaises((Unsupported, ValueError)):
                        backend.reset(registry)
                    self.assertIsInstance(
                        aq_base(registry)._records, backend.MovedRecords
                    )
            finally:
                stock.close()
            # A client with kv reads the rows of the store.
            client = DB(
                zoe.zeo.ClientStorage(server.address, storage="1", wait_timeout=TIMEOUT)
            )
            try:
                with opened(client) as (app, _):
                    registry = app[PLONE_SITE_ID].portal_registry
                    self.assertEqual(registry[TITLE], "Kept on the server")
            finally:
                client.close()

    # The work of the views.

    def test_keys_of_a_range_compare_a_number_of_names_that_grows_as_a_log(self):
        registry = self.moved()
        names = list(registry.records.keys())
        count = len(names)
        self.assertGreater(count, 100)
        low, high = names[count // 2], names[count // 2 + 3]
        Bound.compared = 0
        found = registry.records.keys(Bound(low), Bound(high))
        compared = Bound.compared
        self.assertEqual(found, [name for name in names if low <= name <= high])
        limit = 4 * (count.bit_length() + 1) + len(found)
        self.assertLessEqual(
            compared,
            limit,
            f"keys(min, max) compared {compared} times for {count} names",
        )

    def test_values_and_a_commit_pickle_each_field_once(self):
        self.moved()
        pickled = []
        dump = registry_rows._Pickler.dump

        def counted(pickler, obj):
            pickled.append(obj)
            return dump(pickler, obj)

        with mock.patch.object(registry_rows._Pickler, "dump", counted):
            records = self.registry.records.values()
            transaction.commit()
        self.assertTrue(records)
        fields = [obj for obj in pickled if not isinstance(obj, str)]
        times = collections.Counter(map(id, fields))
        twice = sum(1 for count in times.values() if count > 1)
        self.assertEqual(
            twice, 0, f"{twice} of {len(times)} fields pickled more than once"
        )
