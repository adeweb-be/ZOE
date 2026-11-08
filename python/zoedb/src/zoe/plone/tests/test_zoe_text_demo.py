"""Acceptance tests of `ZoeTextIndex` on a test storage (ADR 0060, section
9): a stack of `DemoStorage`s whose base is a ZOE storage.

The tests run on a stacked `DemoStorage` over an empty `zoe.Storage`, as
the Plone suites run, and over the `zoe.Storage` of `ZOE_TEXT_SITE`, which
holds a site with `ZoeTextIndex` indexes and their deltas. The layer
installs the backend `catalog`. They check:

- a save writes its deltas in the memory of the top `DemoStorage` and
  nothing in the base;
- a search finds the texts of each `DemoStorage` of the stack and of the
  transaction;
- a delta above replaces the same key below;
- a clear in a `DemoStorage` gives a search on the staged texts alone;
- the base holds no delta after the test storage goes;
- texts over the cap of a call fail the search with `Unavailable`.

`just plone zoe backends` runs these tests.
"""

import shutil
import tempfile
import unittest

import transaction
from OFS.Application import Application
from plone.app.testing import SITE_OWNER_NAME, SITE_OWNER_PASSWORD
from plone.testing import zope as zope_testing
from Products.CMFPlone.factory import addPloneSite
from Testing.makerequest import makerequest
from ZODB.DB import DB
from ZODB.DemoStorage import DemoStorage
from zope.component.hooks import getSite, setSite

import zoe
from zoe.plone import testing as zoe_testing
from zoe.plone import textindex
from zoe.plone.tests.test_catalog_mirror import key_of, opened, wait_ready
from zoe.plone.tests.test_zoe_text_index import (
    CORPUS,
    make_corpus,
    search,
    text_of,
    wait_text,
)
from zoe.storage import Unavailable


def make_site(db):
    """Add a Zope application with the site owner and a Plone site `plone`
    to the empty database `db`, then import the profile `zoe.plone:text`."""
    old = getSite()
    connection = db.open()
    try:
        app = Application()
        connection.root()["Application"] = app
        app.acl_users._doAddUser(SITE_OWNER_NAME, SITE_OWNER_PASSWORD, ["Manager"], [])
        transaction.commit()
        app = makerequest(connection.root()["Application"])
        zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
        setSite(None)
        addPloneSite(
            app,
            "plone",
            setup_content=False,
            extension_ids=("plone.app.contenttypes:default",),
        )
        transaction.commit()
        setSite(app["plone"])
        app["plone"].portal_setup.runAllImportStepsFromProfile("profile-zoe.plone:text")
        make_corpus(app["plone"])
        transaction.commit()
    finally:
        transaction.abort()
        zope_testing.logout()
        setSite(old)
        connection.close()


def ids(result):
    return {brain.getId for brain in result}


def titled(site, text):
    """The ids of the hits of `Title`, which has no boosts."""
    return ids(site.portal_catalog.unrestrictedSearchResults(Title=text))


class Big:
    """An object whose `SearchableText` passes the cap of a call."""

    SearchableText = "word " * (4 * 1024 * 1024)


class StackTests:
    """The tests of a stack of two `DemoStorage`s over `self.base`. The
    corpus is in the base or in the lower `DemoStorage` (`self.lower`)."""

    def push(self, below):
        storage = DemoStorage(base=below, close_base_on_close=False)
        db = DB(storage)
        self.addCleanup(db.close)
        return storage, db

    def test_a_save_writes_its_deltas_in_the_top_demostorage_only(self):
        top, db = self.push(self.lower)
        last = self.base.lastTransaction()
        with opened(db) as site:
            site["n5"].text = text_of("<p>Walnut cake</p>")
            site["n5"].reindexObject()
            rid = site.portal_catalog.getrid("/plone/n5")
            key = key_of(site)
            transaction.commit()
        overlay = textindex._overlays[top]
        self.assertIn("Walnut cake", overlay.texts[(key, "SearchableText", rid)][1])
        below = textindex._overlays.get(self.lower)
        if below is not None and (key, "SearchableText", rid) in below.texts:
            self.assertNotIn("Walnut", below.texts[(key, "SearchableText", rid)][1])
        self.assertEqual(self.base.lastTransaction(), last)

    def test_a_search_finds_the_texts_of_the_stack_and_of_the_transaction(self):
        top, db = self.push(self.lower)
        with opened(db) as site:
            site["n4"].text = text_of("<p>Hazelnut tart</p>")
            site["n4"].reindexObject()
            transaction.commit()
        with opened(db) as site:
            site.invokeFactory("Document", "t-new", title="Hazelnut pie")
            self.assertEqual(ids(search(site, "hazelnut")), {"n4", "t-new"})
            # The texts of the corpus below the top.
            self.assertEqual(ids(search(site, "stars")), {"n6"})
            self.assertEqual(ids(search(site, "roast")), {"n5"})

    def test_a_delta_above_replaces_the_same_key_below(self):
        middle, db1 = self.push(self.lower)
        with opened(db1) as site:
            site["n5"].text = text_of("<p>Walnut cake</p>")
            site["n5"].reindexObject()
            transaction.commit()
        top, db2 = self.push(middle)
        with opened(db2) as site:
            self.assertEqual(ids(search(site, "walnut")), {"n5"})
            site["n5"].text = text_of("<p>Pecan cake</p>")
            site["n5"].reindexObject()
            transaction.commit()
        with opened(db2) as site:
            self.assertEqual(ids(search(site, "walnut")), set())
            self.assertEqual(ids(search(site, "pecan")), {"n5"})
            self.assertEqual(ids(search(site, "roast")), set())
        with opened(db1) as site:
            self.assertEqual(ids(search(site, "walnut")), {"n5"})

    def test_a_clear_in_a_demostorage_gives_a_search_on_the_staged_texts_alone(self):
        top, db = self.push(self.lower)
        with opened(db) as site:
            self.assertEqual(titled(site, "coffee"), {"n5", "n7"})
            pc = site.portal_catalog
            pc.manage_clearIndex(["Title"])
            pc.reindexObject(site["n5"], idxs=["Title"])
            transaction.commit()
        with opened(db) as site:
            self.assertEqual(titled(site, "coffee"), {"n5"})
            self.assertEqual(titled(site, "night"), set())
            site.portal_catalog.reindexObject(site["n6"], idxs=["Title"])
            self.assertEqual(titled(site, "night"), {"n6"})

    def test_the_base_holds_no_delta_after_the_test_storage_goes(self):
        top, db = self.push(self.lower)
        last = self.base.lastTransaction()
        with opened(db) as site:
            site["n5"].text = text_of("<p>Walnut cake</p>")
            site["n5"].reindexObject()
            site.portal_catalog.manage_clearIndex(["Title"])
            transaction.commit()
        db.close()
        del top, db
        self.assertEqual(self.base.lastTransaction(), last)
        self.check_base()

    def test_texts_over_the_cap_of_a_call_fail_the_search_with_unavailable(self):
        top, db = self.push(self.lower)
        with opened(db) as site:
            site.portal_catalog._catalog.catalogObject(Big(), "/plone/big")
            transaction.commit()
        with opened(db) as site:
            with self.assertRaises(Unavailable):
                list(search(site, "word"))

    def check_base(self):
        """Check that the base holds the texts that it held before."""


class TestOverAnEmptyStore(StackTests, unittest.TestCase):
    """A stack over an empty `zoe.Storage`: the site and its corpus are in
    the lower `DemoStorage`, and the base holds no record."""

    layer = zoe_testing.ZOE_TEXT_SITE

    @classmethod
    def setUpClass(cls):
        cls.directory = tempfile.mkdtemp(prefix="zoe-text-demo-")
        cls.base = zoe.Storage(f"{cls.directory}/zoe")
        cls.lower = DemoStorage(base=cls.base, close_base_on_close=False)
        cls.lower_db = DB(cls.lower)
        make_site(cls.lower_db)

    @classmethod
    def tearDownClass(cls):
        cls.lower_db.close()
        cls.base.close()
        shutil.rmtree(cls.directory, ignore_errors=True)

    def check_base(self):
        # The base holds no record, so it holds no catalog either.
        self.assertEqual(self.base.lastTransaction(), b"\0" * 8)
        self.assertEqual(len(self.base), 0)


class TestOverAStoreWithASite(StackTests, unittest.TestCase):
    """A stack over the `zoe.Storage` of `ZOE_TEXT_SITE`: the corpus is in
    the base, with its deltas."""

    layer = zoe_testing.ZOE_TEXT_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.base = self.layer["zoeStorage"]
        with opened(self.db) as site:
            make_corpus(site)
            for oid in CORPUS:
                doc = site["box"][oid] if CORPUS[oid][1] else site[oid]
                doc.text = text_of(f"<p>{CORPUS[oid][4]}</p>")
                doc.reindexObject()
            transaction.commit()
            self.key = key_of(site)
            self.rid = site.portal_catalog.getrid("/plone/n5")
            self.entries = len(site.portal_catalog._catalog.uids)
        wait_ready(self.base, self.key)
        wait_text(self.base, self.key)
        self.lower = DemoStorage(base=self.base, close_base_on_close=False)
        self.lower_db = DB(self.lower)
        self.addCleanup(self.lower_db.close)

    def check_base(self):
        tid = textindex._next(self.base.lastTransaction())
        info = self.base.text_info(self.key, "SearchableText", tid, self.rid)
        self.assertIn("roast", info["text"])
        self.assertNotIn("Walnut", info["text"])
        title = self.base.text_info(self.key, "Title", tid)
        self.assertEqual(title["docs"], self.entries)
