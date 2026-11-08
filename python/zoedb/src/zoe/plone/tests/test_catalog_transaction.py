"""Acceptance tests of the transaction overlay of the catalog backend of
`zoe.plone` (ADR 0059, section 8): a search in a transaction that changed
the catalog sends the changed records with the query, and the mirror
answers over them, natively, with the brains of the Python path in that
transaction.

The tests run on the layer of a `zoe.Storage` with its mirror in the
process (`ZOE_SITE`) and on the layer of a native client of `zoe serve`
with the mirror in the server (`ZOE_SERVE_SITE`), with the backend on and
without queued indexing. Each test changes content without a commit, and
the transaction aborts at its end. `just plone zoe backends` runs these
tests.
"""

import contextlib
import unittest
from unittest import mock

import transaction
from zope.event import notify
from zope.processlifetime import DatabaseOpenedWithRoot

from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests import test_catalog_zeo
from zoe.plone.tests.test_catalog_mirror import (
    TYPES,
    answer,
    key_of,
    opened,
    stock,
    subscribe,
    switch_on,
    wait_ready,
)

# The folder that holds a Document at the deepest path of the tests, so a
# move into it does not raise `_depth` of the path index.
FOLDER = "t-folder"


def searches(site):
    """The searches of each check, on the indexes that the mirror serves."""
    base = "/".join(site.getPhysicalPath())
    return [
        {"portal_type": "Document"},
        {"portal_type": "Document", "sort_on": "sortable_title"},
        {"getId": ["t-new", "item-0", "item-6-moved", "t-sp"]},
        {"sortable_title": {"query": ["changed", "item 99"], "range": "min:max"}},
        {"path": {"query": f"{base}/{FOLDER}", "depth": 1}},
        {"path": base, "portal_type": ["Document", "Folder"], "sort_on": "getId"},
        {"review_state": "private", "sort_on": "modified", "sort_order": "reverse"},
        {"is_folderish": True},
    ]


@contextlib.contextmanager
def overlays():
    """Yield the list of the overlays that the hook builds."""
    sent = []
    original = catalog._overlay

    def spy(jar):
        records = original(jar)
        sent.append(records)
        return records

    with mock.patch.object(catalog, "_overlay", spy):
        yield sent


class TransactionTests:
    """The tests of both layers."""

    def wait(self):
        wait_ready(self.storage, self.key)

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
            if FOLDER not in site.objectIds():
                site.invokeFactory("Folder", FOLDER, title="Folder")
                site[FOLDER].invokeFactory("Document", "t-deep", title="Deep")
            transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        with opened(self.db) as site:
            self.key = key_of(site)
        self.wait()
        catalog._counts.clear()

    def counts(self):
        queries = catalog._counts_now()["queries"]
        return queries.get("native", 0), queries.get("python", 0)

    def check(self, site, native=True):
        """Each search gives the brains of the Python path in the
        transaction, with the same `len()` and `actual_result_count`, and
        counts as native when `native`."""
        before = self.counts()
        queries = searches(site)
        for query in queries:
            expected = stock(site, query)
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, expected, query)
        if native:
            self.assertEqual(
                self.counts(), (before[0] + len(queries), before[1]), catalog._counts
            )

    def test_each_search_after_each_change_gives_the_python_path_natively(self):
        with opened(self.db) as site, overlays() as sent:
            self.check(site)
            self.assertEqual(sent, [])
            site.invokeFactory("Document", "t-new", title="Item 99 new")
            self.check(site)
            site["item-0"].setTitle("Changed title")
            site["item-0"].reindexObject()
            self.check(site)
            # A rename moves the entry to another path.
            site.manage_renameObject("item-6", "item-6-moved")
            self.check(site)
            site.manage_delObjects(["item-3"])
            self.check(site)
            # A cut and paste into another folder takes a savepoint.
            cut = site.manage_cutObjects(["item-9"])
            site[FOLDER].manage_pasteObjects(cut)
            self.check(site)
            self.assertTrue(sent)
            self.assertTrue(all(sent))

    def test_a_search_after_a_savepoint_sees_its_records_and_a_rollback_undoes_them(
        self,
    ):
        with opened(self.db) as site:
            site.invokeFactory("Document", "t-sp", title="Item 99 savepoint")
            first = transaction.savepoint()
            jar = site._p_jar
            self.assertFalse(jar._registered_objects)
            self.assertTrue(catalog._savepoint_records(jar))
            self.check(site)
            site["item-0"].setTitle("Changed after the savepoint")
            site["item-0"].reindexObject()
            second = transaction.savepoint()
            site.manage_delObjects(["item-12"])
            self.check(site)
            second.rollback()
            self.check(site)
            self.assertIn("item-12", site.objectIds())
            first.rollback()
            self.check(site)
            self.assertNotIn("t-sp", site.objectIds())

    def test_two_searches_with_an_edit_between_them_send_the_whole_overlay(self):
        with opened(self.db) as site, overlays() as sent:
            site["item-0"].setTitle("Changed once")
            site["item-0"].reindexObject()
            query = {"portal_type": "Document", "sort_on": "sortable_title"}
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, stock(site, query))
            site["item-6"].setTitle("Changed twice")
            site["item-6"].reindexObject()
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, stock(site, query))
            first, second = (sent[0], sent[-1])
            named = {oid for oid, _ in first if not oid[0] & 0x80}
            self.assertTrue(named)
            self.assertLessEqual(named, {oid for oid, _ in second})
        self.assertEqual(self.counts(), (2, 0))

    def test_new_objects_without_an_oid_take_provisional_oids(self):
        with opened(self.db) as site, overlays() as sent:
            for i in range(80):
                site[FOLDER].invokeFactory("Document", f"many-{i}", title=f"Many {i}")
            jar = site._p_jar
            registered = list(jar._registered_objects)
            added = dict(jar._added)
            self.check(site)
            provisional = {oid for oid, _ in sent[-1] if oid[0] & 0x80}
            self.assertTrue(provisional)
            # The hook registered and added no object.
            self.assertEqual(jar._registered_objects, registered)
            self.assertEqual(jar._added, added)

    def test_a_brain_made_late_reads_the_state_at_that_moment(self):
        with opened(self.db) as site:
            site["item-0"].setTitle("Changed early")
            site["item-0"].reindexObject()
            pc = site.portal_catalog
            found = pc.unrestrictedSearchResults(portal_type="Document")
            site["item-0"].setTitle("Changed late")
            site["item-0"].reindexObject()
            cat = site.portal_catalog._catalog
            for k, rid in enumerate(found._seq):
                self.assertEqual(found[k].Title, cat[rid].Title)
            titles = [b.Title for b in found]
            self.assertIn("Changed late", titles)
            self.assertNotIn("Changed early", titles)

    def test_an_overlay_past_the_cap_of_a_call_takes_too_large(self):
        with opened(self.db) as site, mock.patch.object(catalog, "_CAP_VALUES", 60):
            site["item-0"].setTitle("Changed")
            site["item-0"].reindexObject()
            query = {"portal_type": "Document"}
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, stock(site, query))
        counts = catalog._counts_now()
        self.assertEqual(counts["fallbacks"][("too_large", "client")], 1)
        self.assertEqual(counts["queries"], {"python": 1})

    def test_after_an_abort_the_next_search_sees_the_committed_state(self):
        query = {"portal_type": "Document", "sort_on": "sortable_title"}
        with opened(self.db) as site, overlays() as sent:
            committed = stock(site, query)
            site.invokeFactory("Document", "t-new", title="Item 99 new")
            changed = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertNotEqual(changed, committed)
            transaction.abort()
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, committed)
            self.assertEqual(len(sent), 1)
        self.assertEqual(self.counts(), (2, 0))


class TestTheOverlayOfATransaction(TransactionTests, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE


class TestTheOverlayOfATransactionOverZoeServe(TransactionTests, unittest.TestCase):
    layer = zoe_testing.ZOE_SERVE_SITE

    def wait(self):
        test_catalog_zeo.wait_ready(self.storage, self.key)
