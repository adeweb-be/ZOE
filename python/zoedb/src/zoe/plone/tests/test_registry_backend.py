"""Integration tests of the registry backend of zoe.plone on a Plone site
(ADR 0050): the check, the data manager, the guard, the pickler, the move
back, `copy_rows` and `reset`.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, as `just plone zoe backends` runs them. They install
the backend themselves. The tests through `zoe serve` need the `zoe` binary
of `zoe.testing.ZoeServer.binary()`.
"""

import contextlib
import shutil
import sys
import tempfile
import unittest

import transaction
from Acquisition import aq_base
from BTrees.OOBTree import OOBTree
from persistent.mapping import PersistentMapping
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, PLONE_SITE_ID
from plone.registry.field import TextLine
from plone.registry.record import Record
from plone.registry.registry import Registry, _Records
from ZODB.broken import Broken, PersistentBroken
from ZODB.DB import DB
from ZODB.FileStorage import FileStorage
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import ConflictError, ReadConflictError, Unsupported

import zoe.kv
from zoe.plone import registry as backend
from zoe.plone.registry_rows import MARKER, decode_marker, dumps, encode_marker
from zoe.plone.tests.test_concurrent_creations import copy_records

TITLE = "plone.site_title"
OTHER = "plone.email_from_name"
TIMEOUT = 30


class GoneField(TextLine):
    """A field whose class a test takes away, as an add-on that is gone."""


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


def buckets(tree):
    """Return the number of buckets of the `OOBTree` `tree`: 1 for a tree
    that keeps its keys in its root."""
    tree._p_activate()
    state = tree.__getstate__()
    if state is None or len(state) == 1:
        return 1
    count = 0
    bucket = state[1]
    while bucket is not None:
        count += 1
        bucket._p_activate()
        state = bucket.__getstate__()
        bucket = state[1] if len(state) > 1 else None
    return count


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

    def served_copy(self):
        """Copy the site to a new `zoe.Storage`, move its registry there and
        commit. Return the path of the store, closed."""
        import zoe

        directory = tempfile.mkdtemp(prefix="zoe-registry-served-")
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
                backend.move(app[PLONE_SITE_ID].portal_registry)
                manager.commit()
        finally:
            db.close()
        return path

    def test_a_move_back_through_zoe_serve_raises_a_read_conflict(self):
        # The probe asks the server for the serial of the registry, on the
        # native client and on the asyncio client of ZEO.
        import zoe.zeo
        from zoe.testing import ZoeServer

        path = self.served_copy()
        with ZoeServer(path) as server:
            dbs = {}
            # With server_sync, a new transaction of the reader waits for
            # the invalidation of the move back.
            for native in (True, False):
                storage = zoe.zeo.ClientStorage(
                    server.address,
                    storage="1",
                    wait_timeout=TIMEOUT,
                    server_sync=True,
                    native=native,
                )
                dbs[native] = DB(storage)
                self.addCleanup(dbs[native].close)
            for native in (True, False):
                reader_db, mover_db = dbs[native], dbs[not native]
                with self.subTest(reader_native=native):
                    with opened(mover_db) as (app, manager):
                        registry = app[PLONE_SITE_ID].portal_registry
                        backend.move(registry)
                        manager.commit()
                    with opened(reader_db) as (app, manager):
                        # The snapshot of the reader holds the moved
                        # registry. It reads no record yet.
                        reader = app[PLONE_SITE_ID].portal_registry
                        aq_base(reader)._p_activate()
                        self.assertIsInstance(
                            aq_base(reader)._records, backend.MovedRecords
                        )
                        with opened(mover_db) as (other, other_manager):
                            backend.move_back(other[PLONE_SITE_ID].portal_registry)
                            other_manager.commit()
                        with self.assertRaises(ReadConflictError):
                            reader[TITLE]
                        manager.abort()
                        # The retry reads the stock registry.
                        registry = app[PLONE_SITE_ID].portal_registry
                        self.assertIsInstance(aq_base(registry)._records, _Records)
                        self.assertNotIsInstance(
                            aq_base(registry)._records, backend.MovedRecords
                        )
                        self.assertTrue(registry[TITLE])

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

    def test_a_write_after_a_rollback_that_staged_a_field_raises_the_counter_once(
        self,
    ):
        registry = self.moved()
        transaction.commit()
        # The connection joins before the backend, so the savepoint of the
        # backend stages the field and the counter after the savepoint of
        # the connection.
        self.portal.zoe_test_attribute = "Joined first"
        registry.records[TITLE].field.title = "Changed in place"
        savepoint = transaction.savepoint()
        savepoint.rollback()
        self.assertEqual(aq_base(registry)._records.counter, 1)
        registry[OTHER] = "Written after the rollback"
        transaction.commit()
        # The write takes the counter of the staged marker, 2, and adds 1.
        # The commit adds nothing more.
        self.assertEqual(aq_base(registry)._records.counter, 3)
        self.assertEqual(decode_marker(namespace(registry).get(MARKER))[1], 3)
        with opened(self.db) as (app, _):
            other = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(other.records[TITLE].field.title, "Changed in place")
            self.assertEqual(other[OTHER], "Written after the rollback")

    def test_a_read_writes_nothing(self):
        registry = self.moved()
        serial = aq_base(registry)._p_serial
        registry.records[TITLE].field  # noqa: B018
        registry[TITLE]  # noqa: B018
        transaction.commit()
        self.assertEqual(aq_base(registry)._p_serial, serial)
        self.assertEqual(aq_base(registry)._records.counter, 1)

    # Conflicts and broken fields.

    def test_two_writes_of_a_moved_registry_conflict_and_the_retry_reads_the_rows(
        self,
    ):
        self.moved()
        self.assertIn(OTHER, self.registry)
        with opened(self.db) as (app, manager):
            first = app[PLONE_SITE_ID].portal_registry
            first[TITLE] = "First"
            with opened(self.db) as (other_app, other_manager):
                other_app[PLONE_SITE_ID].portal_registry[OTHER] = "Second"
                other_manager.commit()
            # The two writes change other records, but both write the
            # registry object.
            with self.assertRaises(ConflictError):
                manager.commit()
        with opened(self.db) as (app, manager):
            retry = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(retry[OTHER], "Second")
            retry[TITLE] = "First"
            manager.commit()
        with opened(self.db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertEqual((registry[TITLE], registry[OTHER]), ("First", "Second"))
            self.assertEqual(aq_base(registry)._records.counter, 3)

    def test_a_write_after_an_undo_never_repeats_a_counter(self):
        db = self.copy()
        with opened(db) as (app, manager):
            backend.move(app[PLONE_SITE_ID].portal_registry)
            manager.commit()
        with opened(db) as (app, manager):
            app[PLONE_SITE_ID].portal_registry[TITLE] = "First write"
            manager.get().note("the first write")
            manager.commit()
        with opened(db) as (app, _):
            # This read fills the cache with the rows of the first write.
            self.assertEqual(app[PLONE_SITE_ID].portal_registry[TITLE], "First write")
        undo_id = next(
            entry["id"]
            for entry in db.undoLog(0, 20)
            if entry["description"] in ("the first write", b"the first write")
        )
        manager = transaction.TransactionManager()
        db.undo(undo_id, manager.get())
        manager.commit()
        with opened(db) as (app, manager):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(aq_base(registry)._records.counter, 1)
            # The undo gives back no row.
            self.assertEqual(registry[TITLE], "First write")
            registry[TITLE] = "Second write"
            manager.commit()
            self.assertEqual(aq_base(registry)._records.counter, 3)
        with opened(db) as (app, _):
            self.assertEqual(app[PLONE_SITE_ID].portal_registry[TITLE], "Second write")

    def test_an_undo_of_a_move_gives_back_the_stock_trees(self):
        db = self.copy()
        with opened(db) as (app, manager):
            registry = app[PLONE_SITE_ID].portal_registry
            values = {name: registry[name] for name in registry.records.keys()}
            backend.move(registry)
            manager.get().note("the move")
            manager.commit()
        undo_id = next(
            entry["id"]
            for entry in db.undoLog(0, 20)
            if entry["description"] in ("the move", b"the move")
        )
        manager = transaction.TransactionManager()
        db.undo(undo_id, manager.get())
        manager.commit()
        with opened(db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertNotIsInstance(aq_base(registry)._records, backend.MovedRecords)
            self.assertEqual(
                {name: registry[name] for name in registry.records.keys()}, values
            )

    def test_a_field_changed_during_a_move_makes_the_move_conflict(self):
        backend.move(self.registry)
        with opened(self.db) as (app, manager):
            records = aq_base(app[PLONE_SITE_ID].portal_registry)._records
            field = records._fields[TITLE]
            self.assertNotIsInstance(field, str)
            field.title = "Changed during the move"
            manager.commit()
        with self.assertRaises(ConflictError):
            transaction.commit()
        transaction.abort()
        with opened(self.db) as (app, _):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            self.assertEqual(field.title, "Changed during the move")

    def test_a_read_in_an_after_commit_hook_joins_nothing(self):
        registry = self.moved()
        found = []
        registry[TITLE] = "Committed"
        transaction.get().addAfterCommitHook(
            lambda status: found.append(registry[TITLE])
        )
        transaction.commit()
        self.assertEqual(found, ["Committed"])

    def test_a_broken_field_moves_and_moves_back_as_it_is(self):
        name = "zoe.plone.tests.gone"
        gone = GoneField
        self.registry.records[name] = Record(gone(title="Gone"), "Kept")
        transaction.commit()
        module = sys.modules[__name__]
        del module.GoneField
        self.addCleanup(setattr, module, "GoneField", gone)
        with opened(self.db) as (app, manager):
            registry = app[PLONE_SITE_ID].portal_registry
            field = aq_base(registry)._records._fields[name]
            self.assertIsInstance(field, PersistentBroken)
            backend.move(registry)
            manager.commit()
        with opened(self.db) as (app, manager):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(registry[name], "Kept")
            self.assertIsInstance(aq_base(registry)._records._fields[name], Broken)
            backend.move_back(registry)
            manager.commit()
        # The class comes back: the field reads as it was.
        module.GoneField = gone
        with opened(self.db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            field = registry.records[name].field
            self.assertIsInstance(field, gone)
            self.assertEqual(field.title, "Gone")
            self.assertEqual(registry[name], "Kept")

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

    def test_a_record_write_during_a_move_makes_the_move_conflict(self):
        backend.move(self.registry)
        with opened(self.db) as (app, manager):
            app[PLONE_SITE_ID].portal_registry[TITLE] = "Written during the move"
            manager.commit()
        with self.assertRaises(ConflictError):
            transaction.commit()
        transaction.abort()
        with opened(self.db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertNotIsInstance(aq_base(registry)._records, backend.MovedRecords)
            self.assertEqual(registry[TITLE], "Written during the move")

    def test_an_attribute_write_of_the_registry_does_not_conflict_with_a_reader(
        self,
    ):
        records = self.registry.records
        for tree in (records._fields, records._values):
            self.assertFalse(backend._holds_its_bucket(tree))
        with opened(self.db) as (app, manager):
            site = app[PLONE_SITE_ID]
            self.assertTrue(site.portal_registry[TITLE])
            site.zoe_test_attribute = "Written by the reader"
            with opened(self.db) as (other_app, other_manager):
                # As the subscribers of the resources and of the theme do.
                registry = aq_base(other_app[PLONE_SITE_ID].portal_registry)
                registry.zoe_test_mtime = 1
                other_manager.commit()
            manager.commit()

    def test_a_write_that_races_a_move_conflicts_with_the_split_merge_on(self):
        # Rule 1 of the split merge lets a read-current node through when
        # the other side changed it only by splits and replacements. A new
        # record writes under both roots.
        import zoe.policy

        zoe.policy.install(self.portal._p_jar, {"classes": {}, "split_merges": "on"})
        transaction.commit()
        db = self.copy()
        name = "zoe.plone.tests.added"
        with opened(db) as (app, manager):
            records = app[PLONE_SITE_ID].portal_registry.records
            records[name] = Record(TextLine(title="Added"), "Added before the move")
            with opened(db) as (mover, mover_manager):
                backend.move(mover[PLONE_SITE_ID].portal_registry)
                mover_manager.commit()
            with self.assertRaises(ConflictError):
                manager.commit()
        with opened(db) as (app, _):
            self.assertNotIn(name, app[PLONE_SITE_ID].portal_registry)

    def split_merge_copy(self):
        """Turn the split merge on, and copy the site to a new
        `zoe.Storage`. Return its database."""
        import zoe.policy

        zoe.policy.install(self.portal._p_jar, {"classes": {}, "split_merges": "on"})
        transaction.commit()
        return self.copy()

    def add_records_that_split_a_bucket(self, registry):
        """Add records to `registry` in one range of names, until a bucket of
        each tree splits. Return their names."""
        records = registry.records
        trees = (records._fields, records._values)
        before = [buckets(tree) for tree in trees]
        names = [f"zoe.plone.tests.split.{index:03d}" for index in range(40)]
        for name in names:
            records[name] = Record(TextLine(title="Split"), name)
        for tree, count in zip(trees, before, strict=True):
            self.assertGreater(buckets(tree), count, "no bucket split")
        return names

    def test_a_write_that_splits_a_bucket_and_races_a_move_conflicts(self):
        # The split merge on: the writer stores the roots that it split,
        # and the move emptied them. That is not a change by splits.
        db = self.split_merge_copy()
        with opened(db) as (app, manager):
            names = self.add_records_that_split_a_bucket(
                app[PLONE_SITE_ID].portal_registry
            )
            with opened(db) as (mover, mover_manager):
                backend.move(mover[PLONE_SITE_ID].portal_registry)
                mover_manager.commit()
            with self.assertRaises(ConflictError):
                manager.commit()
        with opened(db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertIsInstance(aq_base(registry)._records, backend.MovedRecords)
            for name in names:
                self.assertNotIn(name, registry)

    def test_a_write_that_splits_a_bucket_during_a_move_makes_the_move_conflict(
        self,
    ):
        # The split merge on: the move read each node and bucket current,
        # and stores the roots that the writer split.
        db = self.split_merge_copy()
        with opened(db) as (mover, mover_manager):
            backend.move(mover[PLONE_SITE_ID].portal_registry)
            with opened(db) as (app, manager):
                names = self.add_records_that_split_a_bucket(
                    app[PLONE_SITE_ID].portal_registry
                )
                manager.commit()
            with self.assertRaises(ConflictError):
                mover_manager.commit()
        with opened(db) as (app, _):
            registry = app[PLONE_SITE_ID].portal_registry
            self.assertNotIsInstance(aq_base(registry)._records, backend.MovedRecords)
            for name in names:
                self.assertEqual(registry[name], name)

    def small_registry(self):
        """Store a registry of one record, whose trees keep their keys in
        their roots, at the Zope root. Return it."""
        app = self.layer["app"]
        app.zoe_small_registry = small = Registry()
        small.records["zoe.small"] = Record(TextLine(title="Small"), "Stored")
        transaction.commit()
        self.assertTrue(backend._holds_its_bucket(small.records._fields))
        return small

    def test_a_write_that_races_a_move_of_a_small_registry_conflicts(self):
        self.small_registry()
        with opened(self.db) as (app, manager):
            app.zoe_small_registry["zoe.small"] = "Written before the move"
            backend.move(self.layer["app"].zoe_small_registry)
            transaction.commit()
            with self.assertRaises(ConflictError):
                manager.commit()
        with opened(self.db) as (app, _):
            self.assertEqual(app.zoe_small_registry["zoe.small"], "Stored")

    def test_a_write_during_a_move_of_a_small_registry_makes_the_move_conflict(
        self,
    ):
        small = self.small_registry()
        backend.move(small)
        with opened(self.db) as (app, manager):
            app.zoe_small_registry["zoe.small"] = "Written during the move"
            manager.commit()
        with self.assertRaises(ConflictError):
            transaction.commit()
        transaction.abort()
        with opened(self.db) as (app, _):
            self.assertEqual(
                app.zoe_small_registry["zoe.small"], "Written during the move"
            )

    def test_the_guard_makes_a_field_change_that_races_a_move_conflict(self):
        with opened(self.db) as (app, manager):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            field.title = "Changed before the move"
            self.moved()
            with self.assertRaises(ConflictError):
                manager.commit()

    def test_without_the_guard_a_field_change_that_races_a_move_is_lost(self):
        # A write of a record reads the nodes of the tree current itself
        # (BTrees), so it conflicts without the guard. A change of a field
        # in place reads no node: only the guard makes it conflict.
        backend.uninstall_guard()
        self.addCleanup(backend.install_guard)
        with opened(self.db) as (app, manager):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            field.title = "Changed before the move"
            self.moved()
            manager.commit()
        with opened(self.db) as (app, _):
            field = app[PLONE_SITE_ID].portal_registry.records[TITLE].field
            self.assertNotEqual(field.title, "Changed before the move")

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

    def test_reset_goes_on_for_a_storage_opened_directly(self):
        # A FileStorage has no namespaces and is no ZEO client: it holds
        # no rows, so reset goes on.
        self.moved()
        directory = tempfile.mkdtemp(prefix="zoe-registry-file-")
        self.addCleanup(shutil.rmtree, directory, ignore_errors=True)
        storage = FileStorage(f"{directory}/Data.fs")
        try:
            copy_records(self.db.storage, storage)
        except BaseException:
            storage.close()
            raise
        db = DB(storage)
        self.addCleanup(db.close)
        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            with self.assertRaises(Unsupported):
                copy[TITLE]
            backend.reset(copy)
            manager.commit()
        with opened(db) as (app, _):
            records = aq_base(app[PLONE_SITE_ID].portal_registry)._records
            self.assertNotIsInstance(records, backend.MovedRecords)
            self.assertEqual(len(records), 0)

    def test_reset_refuses_a_registry_that_reads_its_rows(self):
        registry = self.moved()
        with self.assertRaises(ValueError):
            backend.reset(registry)
