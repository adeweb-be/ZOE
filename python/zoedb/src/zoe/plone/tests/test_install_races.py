"""Acceptance tests of `install()` and `uninstall()` of the backends of
`zoe.plone` while requests run:

- a call into a replaced method that runs while `uninstall()` runs ends
  with the stock result: indexing, the moves of the registry backend, the
  guard of a stock registry, and the catalog backend;
- two threads that run `install()` at the same time leave one
  installation: indexing, the guard of a stock registry, the catalog
  backend, locking and the patch of the mutable property sheet;
- an `install()` and an `uninstall()` at the same time of the catalog
  backend or the security backend end in one of two states: installed,
  with the registration keys or the `ZoeSecurityIndex`, or not
  installed, with no keys or the stock index.

A test of two installs holds the first `install()` after its check. The
second thread meets the first one there at a barrier, then calls
`install()`. The first `install()` goes on when the second one ends, or
when `WAIT` runs out. Without a lock, the second `install()` installs,
then the first installs again over it, and keeps the replacement of the
second as the stock method. With a lock, the second `install()` waits for
the first one, so the first one goes on after `WAIT`.

A test of an install and an uninstall holds the first call after its
change of the installed state, before its transactions, in the same way.
Without the turn of the backend, the second call runs to its end, then
the first call writes for the state that the second call replaced. Then
the test runs the two calls at the same time, many times, with no hold.

They need the interpreter of a Plone instance: `just plone zoe backends`
runs them. Only the tests of an install and an uninstall need a layer:
they add a Plone site to a database of their own.
"""

import os
import threading
import unittest
from unittest import mock

import transaction
import ZODB
from Acquisition import aq_base
from zope.component.hooks import setSite

import zoe
from zoe.plone import (
    catalog,
    indexing,
    locking,
    properties,
    registry_guard,
    registry_ops,
    security,
)
from zoe.plone import testing as zoe_testing
from zoe.plone.tests import test_security_index as acceptance
from zoe.plone.tests.helpers import new_directory

# The longest wait of the first `install()` for the end of the second one,
# from the barrier, in seconds.
WAIT = 2.0
# The longest wait at the barrier, in seconds.
START = 30.0
# The rounds with no hold of each test of an install and an uninstall.
ROUNDS = 20
# The longest wait for the threads of the security backend, in seconds.
JOIN = 60.0


class Late(dict):
    """A dict whose first truth test reads its length, meets the second
    thread at `start`, then waits until `done` is set or `WAIT` runs out,
    and gives the length that it read."""

    def __init__(self):
        super().__init__()
        self.start = threading.Barrier(2, timeout=START)
        self.done = threading.Event()
        self._first = True

    def __len__(self):
        length = super().__len__()
        if self._first:
            self._first = False
            self.start.wait()
            self.done.wait(WAIT)
        return length


class Held:
    """A function whose first call meets the second thread at `start`,
    then waits until `done` is set or `WAIT` runs out, then calls
    `function`."""

    def __init__(self, function):
        self.function = function
        self.start = threading.Barrier(2, timeout=START)
        self.done = threading.Event()
        self._first = True

    def __call__(self, *args, **kwargs):
        if self._first:
            self._first = False
            self.start.wait()
            self.done.wait(WAIT)
        return self.function(*args, **kwargs)


def race(install, late, other=None):
    """Run `install` in a first thread, and `other`, or `install` again, in
    a second thread. The second thread calls it after the first one
    reached `late`, and the first one goes on when the second one ends.
    Raise the first error of a thread."""
    errors = []

    def first():
        try:
            install()
        except BaseException as error:
            errors.append(error)

    def second():
        try:
            late.start.wait()
            (other or install)()
        except BaseException as error:
            errors.append(error)
        finally:
            late.done.set()

    threads = [threading.Thread(target=first), threading.Thread(target=second)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
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
        for each in (
            catalog._root,
            catalog._originals,
            catalog._counts,
            catalog._counts_file,
        ):
            patch = mock.patch.dict(each, clear=each is catalog._root)
            patch.start()
            self.addCleanup(patch.stop)
        # Without the variable, `install()` registers no writer of counts.
        environ = mock.patch.dict(os.environ)
        environ.start()
        self.addCleanup(environ.stop)
        os.environ.pop("ZOE_PLONE_CATALOG_COUNTS", None)

    def install_compare(self, stock):
        """Make `stock` the stock search, and install the backend in the
        mode `compare` until the end of the test."""
        self.Catalog.search = stock
        with mock.patch.dict(os.environ, {"ZOE_PLONE_CATALOG_MODE": "compare"}):
            catalog.install()
        self.addCleanup(catalog.uninstall)

    def test_a_search_in_progress_during_uninstall_ends_with_the_stock_result(self):
        result = object()

        def stock(self, query, sort_index=None, reverse=False, limit=None, merge=True):
            catalog.uninstall()
            return result

        self.install_compare(stock)
        native = mock.patch.object(
            catalog, "_native", return_value=(object(), None, {})
        )
        with native, mock.patch.object(catalog, "_compare") as compare:
            found = self.Catalog().search({"portal_type": "Document"})
        self.assertIs(found, result)
        self.assertEqual(compare.call_count, 1)
        self.assertIs(self.Catalog.search, stock)

    def test_a_native_query_in_progress_during_uninstall_ends_with_the_stock_result(
        self,
    ):
        result = object()

        def stock(self, query, sort_index=None, reverse=False, limit=None, merge=True):
            return result

        def native(catalog_, query, sort_index, reverse, limit, merge):
            catalog.uninstall()
            return object(), None, {}

        self.install_compare(stock)
        with (
            mock.patch.object(catalog, "_native", native),
            mock.patch.object(catalog, "_compare") as compare,
        ):
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


def at_once(*calls):
    """Run each call in a thread of its own, all at the same time. Raise
    the first error of a thread."""
    start = threading.Barrier(len(calls), timeout=START)
    errors = []

    def run(call):
        try:
            start.wait()
            call()
        except BaseException as error:
            errors.append(error)

    threads = [threading.Thread(target=run, args=(call,)) for call in calls]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join()
    if errors:
        raise errors[0]


class InstallAndUninstall:
    """An `install()` and an `uninstall()` at the same time of a backend
    that writes in the databases of the process. `module` is the backend,
    and `held` the name of the function of the module that `install()` and
    `uninstall()` call after their change of the installed state and
    before their transactions. `check()` fails when the state of the
    databases does not agree with `installed()`."""

    layer = zoe_testing.ZOE_SITE
    module = None
    held = None

    def make_site(self):
        """Make a database of its own with a Plone site, and return it."""
        db = acceptance.new_db(self)
        with acceptance.opened(db, ()) as app:
            acceptance.add_site(app)
            # The commit reads no utility of the new site, as the layer
            # `ZOE_SITE` does.
            setSite(None)
            transaction.commit()
        return db

    def check(self):
        raise NotImplementedError

    def start_in(self, installed):
        """Install or uninstall the backend, then check its state."""
        (self.module.install if installed else self.module.uninstall)()
        self.assertEqual(self.module.installed(), installed)
        self.check()

    def test_an_install_and_an_uninstall_at_the_same_time_end_in_one_state(self):
        module = self.module
        for first, second in (
            (module.install, module.uninstall),
            (module.uninstall, module.install),
        ):
            with self.subTest(first=first.__name__):
                self.start_in(first is module.uninstall)
                held = Held(getattr(module, self.held))
                with mock.patch.object(module, self.held, held):
                    race(first, held, second)
                self.assertFalse(held._first)
                self.check()
        for turn in range(ROUNDS):
            with self.subTest(turn=turn):
                self.start_in(bool(turn % 2))
                calls = [module.install, module.uninstall]
                at_once(*(calls if turn % 4 < 2 else calls[::-1]))
                self.check()


class TestCatalogInstallAndUninstall(InstallAndUninstall, unittest.TestCase):
    module = catalog
    held = "_databases"

    def setUp(self):
        from Products.ZCatalog.Catalog import Catalog

        if catalog.installed():
            self.skipTest("the catalog backend is installed")
        self.db = self.make_site()
        self.storage = self.db.storage
        with acceptance.opened(self.db) as site:
            self.key = aq_base(site.portal_catalog)._catalog._p_oid
        self.addCleanup(setattr, Catalog, "search", Catalog.__dict__["search"])
        for each, values in (
            (catalog._root, {"db": self.db}),
            (catalog._originals, {}),
            (catalog._counts, {}),
            (catalog._counts_file, {}),
        ):
            patch = mock.patch.dict(each, values, clear=each is catalog._root)
            patch.start()
            self.addCleanup(patch.stop)
        environ = mock.patch.dict(os.environ, {"ZOE_PLONE_CATALOG_MODE": "on"})
        environ.start()
        self.addCleanup(environ.stop)
        os.environ.pop("ZOE_PLONE_CATALOG_COUNTS", None)
        self.addCleanup(catalog.uninstall)

    def check(self):
        found = {
            bytes(k) for k, _ in self.storage.kv_scan("durable", catalog.NAMESPACE)
        }
        self.assertEqual(found, {self.key} if catalog.installed() else set())


class TestSecurityInstallAndUninstall(InstallAndUninstall, unittest.TestCase):
    module = security
    held = "_root_db"

    def setUp(self):
        if security.installed():
            self.skipTest("the security backend is installed")
        self.db = self.make_site()
        patch = mock.patch.dict(security._root, {"db": self.db}, clear=True)
        patch.start()
        self.addCleanup(patch.stop)
        self.addCleanup(security.uninstall)

    def check(self):
        security._join(JOIN)
        self.assertFalse(any(t.is_alive() for t in security._threads))
        with acceptance.opened(self.db) as site:
            index = security.security_index(site.portal_catalog)
            self.assertEqual(index is not None, security.installed())
