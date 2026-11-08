"""Acceptance tests of `install()` and `uninstall()` of the backends of
`zoe.plone` while requests run:

- a call into a replaced method that runs while `uninstall()` runs ends
  with the stock result: indexing, the moves of the registry backend, the
  guard of a stock registry, and the catalog backend;
- two threads that run `install()` at the same time leave one
  installation: indexing, the guard of a stock registry, the catalog
  backend, locking and the patch of the mutable property sheet.

A test of two installs holds the first `install()` after its check, until
the second `install()` ends or a time runs out. Without a lock, the second
`install()` installs, then the first installs again over it, and keeps the
replacement of the second as the stock method.

They need the interpreter of a Plone instance: `just plone zoe backends`
runs them. They need no layer.
"""

import os
import threading
import unittest
from unittest import mock

import transaction
import ZODB

import zoe
from zoe.plone import (
    catalog,
    indexing,
    locking,
    properties,
    registry_guard,
    registry_ops,
)
from zoe.plone.tests.helpers import new_directory

# The longest wait of the first `install()` for the second one, in seconds.
WAIT = 1.0


class Late(dict):
    """A dict whose first truth test reads its length, then waits until
    `go` is set or `WAIT` runs out, and gives the length that it read."""

    def __init__(self):
        super().__init__()
        self.entered = threading.Event()
        self.go = threading.Event()
        self._first = True

    def __len__(self):
        length = super().__len__()
        if self._first:
            self._first = False
            self.entered.set()
            self.go.wait(WAIT)
        return length


def race(install, late):
    """Run `install` in two threads. The second thread starts while the
    first holds the answer of its check of `late`. Raise the first error
    of a thread."""
    errors = []

    def run():
        try:
            install()
        except BaseException as error:
            errors.append(error)

    first = threading.Thread(target=run)
    second = threading.Thread(target=run)
    first.start()
    if not late.entered.wait(10 * WAIT):
        raise AssertionError("the first install() made no check")
    second.start()
    second.join(WAIT / 2)
    late.go.set()
    first.join()
    second.join()
    if errors:
        raise errors[0]


class TestIndexing(unittest.TestCase):
    def setUp(self):
        from Products.CMFCore.interfaces import IPortalCatalogQueueProcessor
        from zope.component import getGlobalSiteManager

        if indexing.installed():
            self.skipTest("the indexing backend is installed")
        self.provided = IPortalCatalogQueueProcessor
        self.registry = getGlobalSiteManager()
        self.addCleanup(self.restore, self.current())

    def current(self):
        return self.registry.queryUtility(self.provided, name=indexing.UTILITY)

    def restore(self, original):
        """Remove each utility of the test from the registry, and give it
        `original` back."""
        while self.current() is not None and self.current() is not original:
            if not self.registry.unregisterUtility(
                self.current(), self.provided, indexing.UTILITY
            ):
                break
        if original is not None and self.current() is not original:
            self.registry.registerUtility(original, self.provided, indexing.UTILITY)

    def test_a_call_in_progress_during_uninstall_ends_with_the_stock_utility(self):
        calls = []

        class Stock:
            def reindex(self, obj, attributes=None, update_metadata=1):
                calls.append((obj, attributes, update_metadata))

        stock = Stock()
        self.registry.registerUtility(stock, self.provided, indexing.UTILITY)
        indexing.install()
        self.addCleanup(indexing.uninstall)
        processor = self.current()
        self.assertIsInstance(processor, indexing.QueueProcessor)

        def queue(obj, op, idxs, metadata):
            indexing.uninstall()
            return False

        with mock.patch.object(processor, "_queue", queue):
            processor.reindex("obj", ["Title"], update_metadata=0)
        self.assertEqual(calls, [("obj", ["Title"], 0)])
        self.assertIs(self.current(), stock)

    def test_two_installs_at_the_same_time_leave_one_installation(self):
        late = Late()
        with mock.patch.object(indexing, "_installed", late):
            try:
                race(indexing.install, late)
                processor = self.current()
                self.assertIsInstance(processor, indexing.QueueProcessor)
                self.assertNotIsInstance(processor._stock, indexing.QueueProcessor)
            finally:
                indexing.uninstall()


class TestRegistryOps(unittest.TestCase):
    def setUp(self):
        self.addCleanup(
            registry_ops.install if registry_ops._installed else registry_ops.uninstall
        )

    def test_a_move_in_progress_during_uninstall_ends(self):
        from plone.registry.registry import Registry

        db = ZODB.DB(zoe.Storage(f"{new_directory(self, 'zoe-races-')}/zoe"))
        self.addCleanup(db.close)
        manager = transaction.TransactionManager()
        connection = db.open(transaction_manager=manager)
        self.addCleanup(connection.close)
        connection.root()["registry"] = registry = Registry()
        manager.commit()
        registry_ops.install()
        new_id = registry_ops.registry_rows.new_id

        def late_id():
            registry_ops.uninstall()
            return new_id()

        with mock.patch.object(registry_ops.registry_rows, "new_id", late_id):
            registry_ops.move(registry)
        manager.commit()
        self.assertIsInstance(registry._records, registry_ops.MovedRecords)
        self.assertFalse(registry_ops._installed)


class TestRegistryGuard(unittest.TestCase):
    def setUp(self):
        from plone.registry.registry import Registry

        self.Registry = Registry
        self.addCleanup(
            registry_guard.install_guard
            if registry_guard._guard
            else registry_guard.uninstall_guard
        )

    def test_a_read_in_progress_during_uninstall_ends_with_the_stock_records(self):
        registry_guard.install_guard()
        stock = registry_guard._guard["records"]
        read_current = registry_guard._read_current

        def late_read(registry, records):
            registry_guard.uninstall_guard()
            read_current(registry, records)

        registry = self.Registry()
        with mock.patch.object(registry_guard, "_read_current", late_read):
            found = registry.records
        self.assertIs(found, registry._records)
        self.assertIs(self.Registry.__dict__["records"], stock)

    def test_two_installs_at_the_same_time_leave_one_installation(self):
        registry_guard.uninstall_guard()
        stock = self.Registry.__dict__["records"]
        late = Late()
        with mock.patch.object(registry_guard, "_guard", late):
            try:
                race(registry_guard.install_guard, late)
                self.assertIs(late["records"], stock)
            finally:
                registry_guard.uninstall_guard()
        self.Registry.records = stock


class TestCatalog(unittest.TestCase):
    def setUp(self):
        from Products.ZCatalog.Catalog import Catalog

        if catalog.installed():
            self.skipTest("the catalog backend is installed")
        self.Catalog = Catalog
        self.addCleanup(setattr, Catalog, "search", Catalog.__dict__["search"])
        for each in (catalog._root, catalog._originals, catalog._counts):
            patch = mock.patch.dict(each, clear=each is catalog._root)
            patch.start()
            self.addCleanup(patch.stop)

    def test_a_search_in_progress_during_uninstall_ends_with_the_stock_result(self):
        result = object()

        def stock(self, query, sort_index=None, reverse=False, limit=None, merge=True):
            catalog.uninstall()
            return result

        self.Catalog.search = stock
        with mock.patch.dict(os.environ, {"ZOE_PLONE_CATALOG_MODE": "compare"}):
            catalog.install()
        self.addCleanup(catalog.uninstall)
        native = mock.patch.object(
            catalog, "_native", return_value=(object(), None, {})
        )
        with native, mock.patch.object(catalog, "_compare") as compare:
            found = self.Catalog().search({"portal_type": "Document"})
        self.assertIs(found, result)
        self.assertEqual(compare.call_count, 1)
        self.assertIs(self.Catalog.search, stock)

    def test_two_installs_at_the_same_time_leave_one_installation(self):
        stock = self.Catalog.search
        late = Late()
        with mock.patch.object(catalog, "_installed", late):
            try:
                race(catalog.install, late)
                self.assertIs(late["stock"], stock)
            finally:
                catalog.uninstall()


class PatchRace:
    """Two installs at the same time of a backend that patches methods of
    classes and has no `uninstall()`. `module` is the backend, and
    `methods()` gives the class and the name of each method."""

    module = None

    def methods(self):
        raise NotImplementedError

    def test_two_installs_at_the_same_time_leave_one_installation(self):
        stock = {}
        for cls, name in self.methods():
            self.addCleanup(setattr, cls, name, getattr(cls, name))
            stock[name] = self.module._stock.get(name, getattr(cls, name))
            setattr(cls, name, stock[name])
        late = Late()
        with mock.patch.object(self.module, "_stock", late):
            race(self.module.install, late)
        self.assertEqual(dict(late), stock)


class TestLocking(PatchRace, unittest.TestCase):
    module = locking

    def methods(self):
        from OFS.Lockable import LockableItem
        from plone.locking.lockable import TTWLockable

        return [(LockableItem, "wl_lockmapping"), (TTWLockable, "_locks")]


class TestProperties(PatchRace, unittest.TestCase):
    module = properties

    def methods(self):
        from Products.PlonePAS.plugins.property import ZODBMutablePropertyProvider

        provider = ZODBMutablePropertyProvider
        return [
            (provider, "getPropertiesForUser"),
            (provider, "setPropertiesForUser"),
        ]
