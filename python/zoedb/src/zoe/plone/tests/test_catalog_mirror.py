"""Acceptance tests of the catalog backend of `zoe.plone` (ADR 0059): the
searches of a Plone site on FieldIndexes, answered by the catalog mirror of
a ZOE store.

The site of `zoe.plone.testing.ZOE_SITE` lives in a database whose storage
is a `zoe.Storage` itself, so its store holds the mirror. The layers of
plone.app.testing need the interpreter of a Plone instance: `bin/test -s
zoe.plone` of the instance of the zoe scenario runs these tests, and
`just plone zoe backends` runs it. The layers do not load the ZCML of
`zoe.plone`, so the tests register the subscribers that `configure.zcml`
names.
"""

import contextlib
import logging
import shutil
import tempfile
import time
import unittest
from unittest import mock

import App.config
import transaction
import ZODB
from Acquisition import aq_base
from OFS.Application import Application
from OFS.Folder import Folder
from plone.app.testing import PLONE_INTEGRATION_TESTING, SITE_OWNER_NAME
from plone.testing import zope as zope_testing
from Products.CMFCore.interfaces import ISiteRoot
from Testing.makerequest import makerequest
from ZODB.DemoStorage import DemoStorage
from ZODB.FileStorage import FileStorage
from zope.component import getGlobalSiteManager
from zope.component.hooks import getSite, setSite
from zope.event import notify
from zope.lifecycleevent.interfaces import IObjectAddedEvent, IObjectRemovedEvent
from zope.processlifetime import DatabaseOpenedWithRoot, IDatabaseOpenedWithRoot
from ZTUtils.Lazy import LazyMap

import zoe
import zoe.kv
from zoe.plone import catalog
from zoe.plone import testing as zoe_testing

# The longest wait for the mirror, in seconds.
TIMEOUT = 30

TYPES = ("Document", "News Item", "Folder")
STATES = ("private", "published")

# The searches on FieldIndexes of the fixed set.
QUERIES = [
    {"portal_type": "Document"},
    {"portal_type": ["Document", "News Item"]},
    {"review_state": "published"},
    {"Creator": SITE_OWNER_NAME},
    {"getId": "item-7"},
    {"portal_type": "News Item", "sort_on": "sortable_title", "sort_limit": 3},
    {"portal_type": "Document", "sort_on": "sortable_title", "sort_order": "reverse"},
    {"sortable_title": {"query": ["item 0003", "item 0011"], "range": "min:max"}},
    {"portal_type": {"query": TYPES, "not": "Folder"}},
    {"review_state": {"not": "private"}, "sort_on": "getId"},
    {
        "portal_type": ["Document", "Folder"],
        "sort_on": ["portal_type", "sortable_title"],
        "sort_order": ["ascending", "descending"],
        # Without a limit, ZCatalog iterates the first sort index here, which
        # the mirror refuses for several sort indexes (`sort`).
        "sort_limit": 20,
    },
    {"portal_type": "Document", "sort_on": "getId", "b_start": 2, "b_size": 3},
]


def subscribe(test):
    """Register the subscribers of `zoe.plone.catalog` that `configure.zcml`
    names, until the end of the test."""
    registry = getGlobalSiteManager()
    for handler, required in (
        (catalog.database_opened, (IDatabaseOpenedWithRoot,)),
        (catalog.site_added, (ISiteRoot, IObjectAddedEvent)),
        (catalog.site_removed, (ISiteRoot, IObjectRemovedEvent)),
    ):
        registry.registerHandler(handler, required)
        test.addCleanup(registry.unregisterHandler, handler, required)


def switch_on(test, mode="on"):
    """Install the backend in `mode` until the end of the test."""
    with mock.patch.dict("os.environ", {"ZOE_PLONE_CATALOG_MODE": mode}):
        catalog.install()
    test.addCleanup(catalog._root.clear)
    test.addCleanup(catalog.uninstall)


@contextlib.contextmanager
def opened(db, name="plone"):
    """Open a connection of `db` with the transaction manager of the thread,
    log in as the site owner and make the site `name` the current site.
    Yield the site. At the end, abort the transaction."""
    old = getSite()
    connection = db.open()
    try:
        app = makerequest(connection.root()["Application"])
        zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
        site = app[name] if name else app
        if name:
            setSite(site)
        yield site
    finally:
        transaction.abort()
        zope_testing.logout()
        setSite(old)
        connection.close()


def key_of(site):
    """The registration key of the catalog of `site`."""
    return aq_base(site.portal_catalog)._catalog._p_oid


def keys(storage):
    """The registration keys in the namespace of `storage`."""
    return {bytes(k) for k, _ in storage.kv_scan("durable", catalog.NAMESPACE)}


def wait_ready(storage, key):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        status = storage._catalog_status(key)
        if status is not None and status["state"] == "ready":
            return status
        time.sleep(0.05)
    raise AssertionError(f"the mirror is not ready: {storage._catalog_status(key)}")


def answer(result):
    """The rids, `len()` and `actual_result_count` of a result."""
    rids = [brain.getRID() for brain in result]
    return rids, len(result), getattr(result, "actual_result_count", len(result))


def stock(site, query):
    """The answer of the stock search of ZCatalog."""
    from Products.ZCatalog.Catalog import Catalog

    with mock.patch.object(Catalog, "search", catalog._installed["stock"]):
        return answer(site.portal_catalog.unrestrictedSearchResults(**query))


class TestTheCatalogMirror(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        subscribe(self)
        switch_on(self)
        with opened(self.db) as site:
            if "item-0" not in site.objectIds():
                for i in range(16):
                    site.invokeFactory(TYPES[i % 3], f"item-{i}", title=f"Item {i:02d}")
                    if i % 2:
                        site.portal_workflow.doActionFor(site[f"item-{i}"], "publish")
                transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        with opened(self.db) as site:
            self.key = key_of(site)
        wait_ready(self.storage, self.key)
        catalog._counts.clear()

    def test_the_namespace_holds_the_key_of_the_site_and_the_mirror_is_ready(self):
        self.assertEqual(keys(self.storage), {self.key})
        self.assertEqual(self.storage._catalog_status(self.key)["state"], "ready")

    def test_the_searches_on_fieldindexes_give_the_brains_of_the_python_path(self):
        with opened(self.db) as site:
            for query in QUERIES:
                expected = stock(site, query)
                got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
                self.assertEqual(got, expected, query)
                self.assertTrue(expected[0], query)
        counts = catalog._counts_now()
        self.assertEqual(counts["queries"].get("native"), len(QUERIES))
        self.assertNotIn("python", counts["queries"])
        server = self.storage._catalog_counts()
        self.assertGreaterEqual(
            server["zoe_catalog_queries_total"]["native"], len(QUERIES)
        )

    def test_a_query_at_an_old_snapshot_gives_the_old_result(self):
        query = {"portal_type": "Document"}
        old = self.db.open(transaction_manager=transaction.TransactionManager())
        try:
            app = makerequest(old.root()["Application"])
            before = answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
            with opened(self.db) as site:
                site.invokeFactory("Document", "later", title="Later")
                transaction.commit()
                after = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(len(after[0]), len(before[0]) + 1)
            again = answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(again, before)
        finally:
            old.transaction_manager.abort()
            old.close()
            with opened(self.db) as site:
                site.manage_delObjects(["later"])
                transaction.commit()

    def test_a_text_query_and_an_index_out_of_the_skeleton_take_the_python_path(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            pc.unrestrictedSearchResults(SearchableText="Item")
            pc.unrestrictedSearchResults(path="/plone", portal_type="Document")
            pc.unrestrictedSearchResults(
                portal_type="Document", sort_on="getObjPositionInParent"
            )
        counts = catalog._counts_now()
        self.assertEqual(counts["queries"].get("python"), 3)
        self.assertEqual(counts["fallbacks"][("index", "client")], 2)
        self.assertEqual(counts["fallbacks"][("sort", "client")], 1)

    def test_a_search_after_a_change_of_the_catalog_takes_transaction(self):
        with opened(self.db) as site:
            site["item-0"].setTitle("Changed")
            site["item-0"].reindexObject()
            found = site.portal_catalog.unrestrictedSearchResults(
                sortable_title="changed"
            )
            self.assertEqual(len(found), 1)
        self.assertEqual(
            catalog._counts_now()["fallbacks"][("transaction", "client")], 1
        )

    def test_compare_finds_no_mismatch_and_logs_a_planted_one(self):
        catalog._installed["mode"] = "compare"
        with opened(self.db) as site:
            for query in QUERIES:
                site.portal_catalog.unrestrictedSearchResults(**query)
            self.assertEqual(catalog._counts_now()["mismatches"], 0)

            def plant(result):
                rids = list(reversed(result._seq))
                return LazyMap(
                    result._func, rids, len(rids), actual_result_count=len(rids)
                )

            with (
                mock.patch.object(catalog, "_plant", plant),
                self.assertLogs("zoe.plone.catalog", logging.ERROR) as logs,
            ):
                found = site.portal_catalog.unrestrictedSearchResults(
                    portal_type="Document", sort_on="getId"
                )
            self.assertEqual(
                answer(found),
                stock(site, {"portal_type": "Document", "sort_on": "getId"}),
            )
        self.assertEqual(catalog._counts_now()["mismatches"], 1)
        text = "\n".join(logs.output)
        self.assertIn("portal_type", text)
        self.assertIn("getId", text)
        self.assertNotIn("Document", text)

    def test_a_new_site_writes_its_key_and_a_deleted_site_removes_it(self):
        from Products.CMFPlone.factory import addPloneSite

        with opened(self.db, name=None) as app:
            setSite(None)
            addPloneSite(app, "other", setup_content=False)
            transaction.commit()
            other = key_of(app["other"])
        self.assertEqual(keys(self.storage), {self.key, other})
        wait_ready(self.storage, other)
        with opened(self.db, name=None) as app:
            app.manage_delObjects(["other"])
            transaction.commit()
        self.assertEqual(keys(self.storage), {self.key})
        deadline = time.monotonic() + TIMEOUT
        while self.storage._catalog_status(other) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertIsNone(self.storage._catalog_status(other))

    def test_uninstall_gives_the_stock_search_back_and_removes_the_keys(self):
        from Products.ZCatalog.Catalog import Catalog

        catalog.uninstall()
        self.assertFalse(catalog.installed())
        self.assertNotEqual(Catalog.search, catalog._search)
        self.assertEqual(keys(self.storage), set())
        deadline = time.monotonic() + TIMEOUT
        while self.storage._catalog_status(self.key) and time.monotonic() < deadline:
            time.sleep(0.05)
        self.assertIsNone(self.storage._catalog_status(self.key))
        with opened(self.db) as site:
            found = site.portal_catalog.unrestrictedSearchResults(portal_type="Folder")
            self.assertTrue(len(found))
        self.assertEqual(catalog._counts_now()["queries"], {})


class TestASiteInAMountedDatabase(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def test_a_site_in_a_mounted_database_writes_its_key_in_the_mounted_store(self):
        from Products.CMFPlone.factory import addPloneSite

        subscribe(self)
        switch_on(self)
        directory = tempfile.mkdtemp(prefix="zoe-plone-catalog-")
        self.addCleanup(shutil.rmtree, directory, True)
        root = ZODB.DB(FileStorage(f"{directory}/Data.fs"), database_name="main")
        self.addCleanup(root.close)
        mounted = ZODB.DB(
            zoe.Storage(f"{directory}/zoe"),
            databases=root.databases,
            database_name="mounted",
        )
        self.addCleanup(mounted.close)
        old = getSite()
        connection = root.open()
        try:
            app = Application()
            connection.root()["Application"] = app
            app.acl_users._doAddUser(SITE_OWNER_NAME, "catsecret", ["Manager"], [])
            folder = Folder("mounted")
            connection.get_connection("mounted").add(folder)
            app._setObject("mounted", folder)
            transaction.commit()
            app = makerequest(connection.root()["Application"])
            zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
            setSite(None)
            addPloneSite(app["mounted"], "site", setup_content=False)
            transaction.commit()
            key = key_of(app["mounted"]["site"])
        finally:
            transaction.abort()
            zope_testing.logout()
            setSite(old)
            connection.close()
        self.assertEqual(keys(mounted.storage), {key})
        wait_ready(mounted.storage, key)
        # At the open of the databases, the key stays in the mounted store.
        dbtab = App.config.getConfiguration().dbtab
        with mock.patch.dict(dbtab.mount_paths, {"/mounted": "mounted"}):
            notify(DatabaseOpenedWithRoot(root))
        self.assertEqual(keys(mounted.storage), {key})


class TestATestStorage(unittest.TestCase):
    layer = PLONE_INTEGRATION_TESTING

    def test_the_key_goes_to_the_overlay_and_a_search_is_unsupported(self):
        app = self.layer["app"]
        db = app._p_jar.db()
        storage = db.storage
        base = storage
        while isinstance(base, DemoStorage):
            base = base.base
        if not isinstance(storage, DemoStorage) or not isinstance(base, zoe.Storage):
            self.skipTest("the layers do not stack a DemoStorage on a ZOE storage")
        switch_on(self)
        catalog.register(db)
        site = self.layer["portal"]
        key = key_of(site)
        namespace = zoe.kv.Namespace(app._p_jar, "durable", catalog.NAMESPACE)
        self.assertEqual(namespace.get(key), b"")
        self.assertIsNone(base.kv_get("durable", catalog.NAMESPACE, key))
        catalog._counts.clear()
        site.portal_catalog.unrestrictedSearchResults(portal_type="Document")
        counts = catalog._counts_now()
        self.assertEqual(counts["fallbacks"][("unsupported", "client")], 1)
