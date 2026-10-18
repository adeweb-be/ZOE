"""Integration tests of the registry backend of zoe.plone on a Plone site
(ADR 0050): the check, the data manager, the guard, the pickler, the move
back, `copy_rows` and `reset`.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, as `just plone zoe backends` runs them. They install
the backend themselves.
"""

import contextlib
import shutil
import tempfile
import unittest

import transaction
from Acquisition import aq_base
from BTrees.OOBTree import OOBTree
from persistent.mapping import PersistentMapping
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, PLONE_SITE_ID
from plone.registry.registry import Registry, _Records
from ZODB.DB import DB
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import ConflictError, ReadConflictError, Unsupported

import zoe.kv
from zoe.plone import registry as backend
from zoe.plone.registry_rows import MARKER, decode_marker, dumps, encode_marker
from zoe.plone.tests.test_concurrent_creations import copy_records

TITLE = "plone.site_title"


@contextlib.contextmanager
def opened(db):
    """Yield the Zope root of a new connection of `db` and its transaction
    manager. Abort and close the connection at the end."""
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        manager.begin()
        yield connection.root()["Application"], manager
    finally:
        manager.abort()
        connection.close()


def namespace(registry):
    """Return the namespace of the moved `registry`."""
    registry = aq_base(registry)
    return zoe.kv.Namespace(registry._p_jar, "durable", registry._records.name)


class TestRegistryBackend(unittest.TestCase):
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

    def set_marker(self, registry, oid, counter):
        """Write the marker of `registry` beside the backend, and drop the
        cache: such a write does not change the counter of the registry."""
        namespace(registry).set(MARKER, encode_marker(oid, counter))
        transaction.commit()
        backend._cache.clear()

    # The option.

    def test_the_move_needs_the_option(self):
        backend.uninstall()
        with self.assertRaises(RuntimeError):
            backend.move(self.registry)
        self.assertNotIsInstance(aq_base(self.registry)._records, backend.MovedRecords)

    def test_a_storage_without_namespaces_refuses_the_move(self):
        db = DB(MappingStorage())
        self.addCleanup(db.close)
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        self.addCleanup(connection.close)
        connection.root()["registry"] = registry = Registry()
        manager.commit()
        with self.assertRaises(Unsupported):
            backend.move(registry)
        self.assertIsInstance(registry._records, _Records)
        self.assertNotIsInstance(registry._records, backend.MovedRecords)
        manager.abort()

    def test_a_move_writes_the_rows_and_a_marker(self):
        registry = self.moved()
        records = aq_base(registry)._records
        self.assertEqual(records.counter, 1)
        self.assertEqual(
            decode_marker(namespace(registry).get(MARKER)),
            (aq_base(registry)._p_oid, 1),
        )
        registry[TITLE] = "Written"
        transaction.commit()
        self.assertEqual(aq_base(registry)._records.counter, 2)
        self.assertEqual(decode_marker(namespace(registry).get(MARKER))[1], 2)
        # A second move does nothing.
        backend.move(registry)
        self.assertEqual(aq_base(registry)._records.counter, 2)

    # The check.

    def test_a_namespace_without_its_marker_raises_unsupported(self):
        registry = self.moved()
        namespace(registry).delete(MARKER)
        transaction.commit()
        backend._cache.clear()
        with opened(self.db) as (app, _):
            with self.assertRaises(Unsupported):
                app[PLONE_SITE_ID].portal_registry[TITLE]

    def test_a_marker_of_another_oid_raises_unsupported(self):
        registry = self.moved()
        self.set_marker(registry, b"\x00" * 7 + b"\x01", 1)
        with opened(self.db) as (app, _):
            with self.assertRaises(Unsupported):
                app[PLONE_SITE_ID].portal_registry[TITLE]

    def test_a_marker_below_the_counter_raises_unsupported(self):
        registry = self.moved()
        registry[TITLE] = "Second write"
        transaction.commit()
        oid = aq_base(registry)._p_oid
        self.set_marker(registry, oid, 1)
        with opened(self.db) as (app, _):
            with self.assertRaises(Unsupported):
                app[PLONE_SITE_ID].portal_registry[TITLE]
        # A marker above the counter passes: a later commit wrote the rows.
        self.set_marker(registry, oid, 5)
        with opened(self.db) as (app, _):
            self.assertEqual(app[PLONE_SITE_ID].portal_registry[TITLE], "Second write")

    def test_a_move_back_after_the_snapshot_raises_a_read_conflict(self):
        self.moved()
        with opened(self.db) as (app, manager):
            # The snapshot of this connection holds the moved registry.
            reader = app[PLONE_SITE_ID].portal_registry
            aq_base(reader)._p_activate()
            backend.move_back(self.registry)
            transaction.commit()
            backend._cache.clear()
            with self.assertRaises(ReadConflictError):
                reader[TITLE]
            manager.abort()
            # The retry reads the stock registry.
            self.assertIsInstance(
                aq_base(app[PLONE_SITE_ID].portal_registry)._records, _Records
            )
            self.assertTrue(app[PLONE_SITE_ID].portal_registry[TITLE])

    # The data manager.

    def test_a_rollback_to_a_savepoint_drops_a_decoded_value(self):
        registry = self.moved()
        registry[TITLE] = "Before"
        savepoint = transaction.savepoint()
        registry[TITLE] = "After"
        self.assertEqual(registry[TITLE], "After")
        savepoint.rollback()
        self.assertEqual(registry[TITLE], "Before")
        transaction.commit()
        with opened(self.db) as (app, _):
            self.assertEqual(app[PLONE_SITE_ID].portal_registry[TITLE], "Before")

    def test_a_field_changed_in_place_is_written_at_the_commit(self):
        registry = self.moved()
        counter = aq_base(registry)._records.counter
        registry.records[TITLE].field.title = "Changed in place"
        transaction.commit()
        self.assertEqual(aq_base(registry)._records.counter, counter + 1)
        with opened(self.db) as (app, _):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            self.assertEqual(field.title, "Changed in place")

    def test_a_savepoint_keeps_a_field_changed_in_place(self):
        registry = self.moved()
        before = registry.records[TITLE].field.title
        first = transaction.savepoint()
        registry.records[TITLE].field.title = "Kept by the savepoint"
        transaction.savepoint()
        # A rollback to the first savepoint drops the change.
        first.rollback()
        self.assertEqual(registry.records[TITLE].field.title, before)
        registry.records[TITLE].field.title = "Changed again"
        transaction.savepoint()
        transaction.commit()
        with opened(self.db) as (app, _):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            self.assertEqual(field.title, "Changed again")

    def test_a_read_writes_nothing(self):
        registry = self.moved()
        serial = aq_base(registry)._p_serial
        registry.records[TITLE].field  # noqa: B018
        registry[TITLE]  # noqa: B018
        transaction.commit()
        self.assertEqual(aq_base(registry)._p_serial, serial)
        self.assertEqual(aq_base(registry)._records.counter, 1)

    # The pickler.

    def test_a_value_with_a_persistent_object_is_refused(self):
        registry = self.moved()
        with self.assertRaises(TypeError):
            registry.records._values[TITLE] = PersistentMapping()
        transaction.abort()

    # The guard.

    def test_the_guard_makes_a_write_that_races_a_move_conflict(self):
        with opened(self.db) as (app, manager):
            writer = app[PLONE_SITE_ID].portal_registry
            writer[TITLE] = "Written before the move"
            self.moved()
            with self.assertRaises(ConflictError):
                manager.commit()
        with opened(self.db) as (app, _):
            self.assertNotEqual(
                app[PLONE_SITE_ID].portal_registry[TITLE], "Written before the move"
            )

    def test_without_the_guard_a_write_that_races_a_move_is_lost(self):
        backend.uninstall_guard()
        self.addCleanup(backend.install_guard)
        with opened(self.db) as (app, manager):
            writer = app[PLONE_SITE_ID].portal_registry
            writer[TITLE] = "Written before the move"
            self.moved()
            manager.commit()
        with opened(self.db) as (app, _):
            self.assertNotEqual(
                app[PLONE_SITE_ID].portal_registry[TITLE], "Written before the move"
            )

    # The move back, the copy and the reset.

    def test_a_move_back_gives_back_each_record_and_deletes_the_rows(self):
        registry = self.portal.portal_registry
        fields = {
            name: type(registry.records[name].field) for name in registry.records.keys()
        }
        self.moved()
        registry[TITLE] = "Moved"
        transaction.commit()
        values = {name: registry[name] for name in registry.records.keys()}
        rows = namespace(registry)
        backend.move_back(registry)
        transaction.commit()
        records = aq_base(registry)._records
        self.assertIsInstance(records._values, OOBTree)
        self.assertEqual({name: registry[name] for name in records.keys()}, values)
        self.assertEqual(
            {name: type(records[name].field) for name in records.keys()}, fields
        )
        self.assertEqual(rows.scan(), [])

    def copy(self):
        """Copy the records of the site to a new `zoe.Storage`. Return its
        database."""
        import zoe

        directory = tempfile.mkdtemp(prefix="zoe-registry-copy-")
        self.addCleanup(shutil.rmtree, directory, ignore_errors=True)
        storage = zoe.Storage(f"{directory}/zoe")
        try:
            copy_records(self.db.storage, storage)
        except BaseException:
            storage.close()
            raise
        db = DB(storage)
        self.addCleanup(db.close)
        return db

    def test_copy_rows_refreshes_stale_rows(self):
        registry = self.moved()
        registry[TITLE] = "Source"
        transaction.commit()
        source_rows = dict(namespace(registry).scan())
        db = self.copy()
        with opened(db) as (app, manager):
            # Stale rows of the copy itself, under the id of the source, as
            # a store restored from a backup holds them: the marker holds
            # the oid of the copy and a lower counter.
            copy = app[PLONE_SITE_ID].portal_registry
            stale = namespace(copy)
            for key, value in source_rows.items():
                stale.set(key, value)
            stale.set(b"vplone.site_title", dumps("Stale"))
            stale.set(MARKER, encode_marker(aq_base(copy)._p_oid, 1))
            manager.commit()
        backend._cache.clear()
        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            old_name = aq_base(copy)._records.name
            with self.assertRaises(Unsupported):
                copy[TITLE]
            backend.copy_rows(registry, copy)
            manager.commit()
            self.assertNotEqual(aq_base(copy)._records.name, old_name)
        with opened(db) as (app, _):
            copy = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(copy[TITLE], "Source")
            # The stale rows of the copy are gone.
            old = zoe.kv.Namespace(aq_base(copy)._p_jar, "durable", old_name)
            self.assertEqual(old.scan(), [])

    def test_copy_rows_refuses_a_registry_of_another_id(self):
        registry = self.moved()
        db = self.copy()
        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            aq_base(copy)._records.id = "0" * 32
            with self.assertRaises(ValueError):
                backend.copy_rows(registry, copy)

    def test_copy_rows_refuses_a_registry_that_reads_its_rows(self):
        registry = self.moved()
        with self.assertRaises(ValueError):
            backend.copy_rows(registry, registry)

    def test_reset_and_the_import_of_the_profile_give_back_the_records(self):
        self.moved()
        db = self.copy()
        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            backend.reset(copy)
            manager.commit()
        with opened(db) as (app, manager):
            site = app[PLONE_SITE_ID]
            self.assertEqual(len(site.portal_registry.records), 0)
            from zope.component.hooks import getSite, setSite

            previous = getSite()
            setSite(site)
            try:
                site.portal_setup.runImportStepFromProfile(
                    "profile-Products.CMFPlone:dependencies", "plone.app.registry"
                )
            finally:
                setSite(previous)
            manager.commit()
        with opened(db) as (app, _):
            self.assertIn(TITLE, app[PLONE_SITE_ID].portal_registry)

    def test_reset_refuses_a_registry_that_reads_its_rows(self):
        registry = self.moved()
        with self.assertRaises(ValueError):
            backend.reset(registry)
