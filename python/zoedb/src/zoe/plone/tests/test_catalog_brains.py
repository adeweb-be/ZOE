"""Acceptance tests of the catalog backend of `zoe.plone` (ADR 0059) on the
metadata table: the brains of a native search come from the rows of the
reply, with the values, the types and the paths of the brains of the
Python path, and the Connection loads no bucket of `data` and no bucket of
`paths` for them.

The site of `zoe.plone.testing.ZOE_SITE` lives in a database whose storage
is a `zoe.Storage` itself, so its store holds the mirror. `bin/test -s
zoe.plone` of the instance of the zoe scenario runs these tests, and
`just plone zoe backends` runs it.
"""

import contextlib
import unittest
from unittest import mock

import Missing
import transaction
from Acquisition import aq_base, aq_parent
from DateTime import DateTime
from Products.ZCatalog.Catalog import Catalog
from ZODB.serialize import referencesf
from zope.i18nmessageid import Message

from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests.test_catalog_mirror import (
    key_of,
    opened,
    subscribe,
    switch_on,
    wait_ready,
)

# The folder of the listing, and its number of Documents.
FOLDER = "brains"
ITEMS = 200

LISTING = {"path": {"query": f"/plone/{FOLDER}", "depth": 1}}
MODIFIED = {
    "portal_type": "Document",
    "sort_on": "modified",
    "sort_order": "reverse",
    "sort_limit": 20,
}
BATCH = {**LISTING, "sort_on": "sortable_title", "b_start": 30, "b_size": 25}


def effective(i):
    """The effective date of the item `i`: in an offset zone, in a named
    zone, naive, or none."""
    day = 1 + i % 28
    return (
        DateTime(2026, 1, day, 10, 30, 0, "GMT+2"),
        DateTime(2026, 2, day, 11, 45, 30, "Etc/GMT-3"),
        DateTime(f"2026/03/{day:02d} 12:15:45"),
        None,
    )[i % 4]


def make_content(site):
    site.invokeFactory("Folder", FOLDER, title="Brains")
    folder = site[FOLDER]
    for i in range(ITEMS):
        name = f"doc-{i:03d}"
        folder.invokeFactory(
            "Document",
            name,
            title=f"Brain {i % 17:02d}",
            description=f"item {i}" if i % 3 else "",
            subject=("alpha", "beta")[: i % 3],
        )
        doc = folder[name]
        doc.setEffectiveDate(effective(i))
        if i % 2:
            site.portal_workflow.doActionFor(doc, "publish")
        doc.reindexObject()


def same(a, b):
    """Tell if `a` and `b` are one value of one type, as the unpickler of
    `data` makes them."""
    if type(a) is not type(b):
        return False
    if a is Missing.Value or b is Missing.Value:
        return a is b
    if isinstance(a, DateTime):
        return (
            a.__getstate__() == b.__getstate__()
            and a.ISO8601() == b.ISO8601()
            and a.timezone() == b.timezone()
        )
    if isinstance(a, Message):
        return a.__reduce__() == b.__reduce__()
    if isinstance(a, tuple):
        return len(a) == len(b) and all(same(x, y) for x, y in zip(a, b, strict=True))
    if isinstance(a, dict):
        return list(a) == list(b) and all(same(a[k], b[k]) for k in a)
    return a == b


def tree_oids(jar, root):
    """The oids of the tree at `root` and of each object that it reaches."""
    found, todo = set(), [root]
    while todo:
        oid = todo.pop()
        if oid in found:
            continue
        found.add(oid)
        data, _ = jar._storage.load(oid)
        todo.extend(referencesf(data))
    return found


@contextlib.contextmanager
def loads(jar):
    """Record the oids that the Connection `jar` loads."""
    seen = []
    storage = jar._storage
    load = storage.load

    def spy(oid, *args):
        seen.append(oid)
        return load(oid, *args)

    with mock.patch.object(storage, "load", spy):
        yield seen


@contextlib.contextmanager
def getitems():
    """Record the rids that the stock `Catalog.__getitem__` gets."""
    rids = []
    original = Catalog.__getitem__

    def spy(self, index):
        rids.append(index)
        return original(self, index)

    with mock.patch.object(Catalog, "__getitem__", spy):
        yield rids


def stock(site, query):
    """The brains of the stock search of ZCatalog."""
    with mock.patch.object(Catalog, "search", catalog._installed["stock"]):
        return site.portal_catalog.unrestrictedSearchResults(**query)


class TestTheBrainsOfTheCatalogMirror(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        subscribe(self)
        switch_on(self)
        with opened(self.db) as site:
            if FOLDER not in site.objectIds():
                make_content(site)
                transaction.commit()
        from zope.event import notify
        from zope.processlifetime import DatabaseOpenedWithRoot

        notify(DatabaseOpenedWithRoot(self.db))
        with opened(self.db) as site:
            self.key = key_of(site)
        wait_ready(self.storage, self.key)
        catalog._counts.clear()

    def native(self, site, query):
        """The brains of the native search of `query`, each column read, and
        the oids of the records of `data` and of `paths` that the Connection
        loaded for them."""
        jar = site._p_jar
        cat = aq_base(site.portal_catalog)._catalog
        trees = tree_oids(jar, cat.data._p_oid) | tree_oids(jar, cat.paths._p_oid)
        names = list(cat.schema)
        before = catalog._counts_now()["queries"].get("native", 0)
        jar.cacheMinimize()
        with loads(jar) as seen:
            found = site.portal_catalog.unrestrictedSearchResults(**query)
            brains = list(found)
            for brain in brains:
                for name in names:
                    getattr(brain, name)
                brain.getPath()
        self.assertEqual(catalog._counts_now()["queries"].get("native", 0), before + 1)
        return found, brains, trees & set(seen)

    def check(self, site, found, brains, query):
        """The brains equal the brains of the stock search, column by
        column, with the same batch and counts."""
        expected = stock(site, query)
        self.assertEqual(len(found), len(expected), query)
        self.assertEqual(
            getattr(found, "actual_result_count", None),
            getattr(expected, "actual_result_count", None),
            query,
        )
        expected = list(expected)
        self.assertEqual(len(brains), len(expected), query)
        self.assertTrue(brains, query)
        names = list(aq_base(site.portal_catalog)._catalog.schema)
        for got, want in zip(brains, expected, strict=True):
            self.same_brain(got, want, names)

    def same_brain(self, got, want, names):
        """`got` is the brain `want`, column by column."""
        self.assertIs(type(aq_base(got)), type(aq_base(want)))
        self.assertEqual(got.getRID(), want.getRID())
        self.assertEqual(got.data_record_id_, want.data_record_id_)
        self.assertEqual(got.data_record_score_, want.data_record_score_)
        self.assertEqual(
            got.data_record_normalized_score_, want.data_record_normalized_score_
        )
        self.assertEqual(got.getPath(), want.getPath())
        self.assertEqual(got.getURL(), want.getURL())
        self.assertIs(aq_base(aq_parent(got)), aq_base(aq_parent(want)))
        for name in names:
            a, b = getattr(got, name), getattr(want, name)
            self.assertTrue(same(a, b), f"{name} of {got.getPath()}: {a!r} {b!r}")

    def late(self, site, found):
        """Read each brain of the native result `found`, made before a
        change, and the brain that `Catalog.__getitem__` makes now for the
        same rid: each pair gives the same brain, or both raise `KeyError`
        for a rid that is gone. Return the rids that the native result made
        with `Catalog.__getitem__`, and its brains."""
        cat = site.portal_catalog._catalog
        names = list(aq_base(cat).schema)
        wanted = []
        for rid in found._seq:
            try:
                wanted.append(cat[rid])
            except KeyError:
                wanted.append(None)
        brains = []
        with getitems() as rids:
            for k, want in enumerate(wanted):
                if want is None:
                    with self.assertRaises(KeyError):
                        found[k]  # noqa: B018
                    continue
                got = found[k]
                self.same_brain(got, want, names)
                self.assertIs(
                    aq_base(got._unrestrictedGetObject()),
                    aq_base(want._unrestrictedGetObject()),
                )
                brains.append(got)
        return rids, brains

    def test_a_brain_made_after_a_write_of_the_transaction_is_the_stock_brain(self):
        from Products.CMFCore.indexing import processQueue

        with opened(self.db) as site:
            before = catalog._counts_now()["queries"].get("native", 0)
            found = site.portal_catalog.unrestrictedSearchResults(**LISTING)
            self.assertEqual(catalog._counts_now()["queries"]["native"], before + 1)
            folder = site[FOLDER]
            folder["doc-005"].setTitle("Late title")
            folder["doc-005"].reindexObject()
            folder.manage_renameObject("doc-006", "doc-006-moved")
            processQueue()
            rids, brains = self.late(site, found)
            self.assertEqual(len(rids), ITEMS)
            self.assertIn("Late title", [b.Title for b in brains])

    def test_a_brain_made_after_a_commit_and_a_new_transaction_is_the_stock_brain(
        self,
    ):
        with opened(self.db) as site:
            doc = site[FOLDER]["doc-008"]
            old = doc.Title()
            found = site.portal_catalog.unrestrictedSearchResults(**LISTING)
            doc.setTitle("Committed title")
            doc.reindexObject()
            transaction.commit()
            try:
                rids, brains = self.late(site, found)
                self.assertEqual(len(rids), ITEMS)
                self.assertIn("Committed title", [b.Title for b in brains])
            finally:
                doc.setTitle(old)
                doc.reindexObject()
                transaction.commit()

    def test_len_and_the_first_brain_decode_one_row(self):
        replies = []
        original = catalog._brains

        def spy(cat, reply, jar, start):
            replies.append(reply)
            return original(cat, reply, jar, start)

        with opened(self.db) as site, mock.patch.object(catalog, "_brains", spy):
            found = site.portal_catalog.unrestrictedSearchResults(**LISTING)
            [reply] = replies
            self.assertEqual(len(reply["rows"]), ITEMS)
            self.assertEqual(len(found), ITEMS)
            self.assertEqual(found.actual_result_count, ITEMS)
            self.assertEqual(reply["rows"]._decoded, 0)
            found[0].Title  # noqa: B018
            self.assertEqual(reply["rows"]._decoded, 1)

    def test_a_folder_listing_makes_its_brains_with_no_load_of_data_or_paths(self):
        with opened(self.db) as site:
            found, brains, loaded = self.native(site, LISTING)
            self.assertEqual(loaded, set())
            self.assertEqual(len(brains), ITEMS)
            self.check(site, found, brains, LISTING)
            # The stock search loads the buckets of `data`.
            jar = site._p_jar
            jar.cacheMinimize()
            with loads(jar) as seen:
                for brain in stock(site, LISTING):
                    brain.Title  # noqa: B018
            cat = aq_base(site.portal_catalog)._catalog
            self.assertTrue(tree_oids(jar, cat.data._p_oid) & set(seen))

    def test_a_datetime_comes_back_equal_in_a_named_an_offset_and_no_zone(self):
        with opened(self.db) as site:
            found, brains, loaded = self.native(site, LISTING)
            self.assertEqual(loaded, set())
            self.check(site, found, brains, LISTING)
            by_id = {brain.getId: brain for brain in brains}
            expected = {b.getId: b for b in stock(site, LISTING)}
            for i, zone, naive in (
                (0, "GMT+2", False),
                (1, "Etc/GMT-3", False),
                (2, None, True),
            ):
                name = f"doc-{i:03d}"
                got, want = by_id[name].effective, expected[name].effective
                held = site[FOLDER][name].effective_date
                self.assertIs(type(got), DateTime)
                for each in (want, held):
                    self.assertEqual(got._micros, each._micros, name)
                    self.assertEqual(got._tz, each._tz, name)
                    self.assertEqual(got._timezone_naive, each._timezone_naive, name)
                    self.assertEqual(got.ISO8601(), each.ISO8601(), name)
                    self.assertEqual(got.timezone(), each.timezone(), name)
                self.assertEqual(bool(got._timezone_naive), naive, name)
                if zone is not None:
                    self.assertEqual(got.timezone(), zone, name)

    def test_a_search_sorted_on_modified_with_sort_limit_gives_the_stock_brains(self):
        with opened(self.db) as site:
            found, brains, loaded = self.native(site, MODIFIED)
            self.assertEqual(loaded, set())
            self.assertEqual(len(brains), 20)
            self.check(site, found, brains, MODIFIED)

    def test_b_start_and_b_size_give_the_batch_of_the_python_path(self):
        with opened(self.db) as site:
            found, brains, loaded = self.native(site, BATCH)
            self.assertEqual(loaded, set())
            self.assertEqual(len(brains), 25)
            self.assertEqual(found.actual_result_count, ITEMS)
            self.check(site, found, brains, BATCH)

    def test_a_column_of_another_type_makes_the_stock_brain(self):
        with opened(self.db) as site:
            doc = site[FOLDER]["doc-007"]
            path = "/".join(doc.getPhysicalPath())
            doc.location = ["a", "list"]
            doc.reindexObject()
            transaction.commit()
        try:
            with opened(self.db) as site:
                rid = site.portal_catalog.getrid(path)
                with getitems() as rids:
                    found, brains, loaded = self.native(site, LISTING)
                self.assertEqual(rids, [rid])
                self.check(site, found, brains, LISTING)
                made = next(b for b in brains if b.getRID() == rid)
                self.assertEqual(made.location, ["a", "list"])
        finally:
            with opened(self.db) as site:
                doc = site[FOLDER]["doc-007"]
                del doc.location
                doc.reindexObject()
                transaction.commit()

    def test_a_brain_past_rn_comes_from_the_stock_getitem(self):
        with opened(self.db) as site:
            names = list(aq_base(site.portal_catalog)._catalog.schema)
            with (
                mock.patch.object(catalog, "_CELLS", 10 * len(names)),
                getitems() as rids,
            ):
                found, brains, _ = self.native(site, LISTING)
            self.assertEqual(rids, [b.getRID() for b in brains[10:]])
            self.check(site, found, brains, LISTING)
