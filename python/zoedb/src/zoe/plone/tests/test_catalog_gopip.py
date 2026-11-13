"""Acceptance tests of the order clause of the catalog backend of
`zoe.plone` (ADR 0059, the amendment of ZOE-302): the listings of a folder
of 300 items sorted on `getObjPositionInParent`, the `GopipIndex` of
plone.folder, answered by the catalog mirror in the order of the Python
path.

The hook finds the folder of a listing at depth 1, as `documentToKeyMap`
finds it, and sends the oid of its order with the plan. The mirror reads
that order as of the snapshot of the search, or from the transaction
overlay, and sorts the rows by the place of their ids. A search over
several folders, such as the navigation, takes the Python path (`sort`).

The tests run on the layer of a `zoe.Storage` with its mirror in the
process (`ZOE_SITE`), and a few of them on the layer of a native client of
`zoe serve` (`ZOE_SERVE_SITE`). `just plone zoe backends` runs them.
"""

import unittest

import transaction
from Testing.makerequest import makerequest
from zope.component.hooks import setSite
from zope.event import notify
from zope.processlifetime import DatabaseOpenedWithRoot

import zoe.policy
from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests import test_catalog_zeo
from zoe.plone.tests.test_catalog_mirror import (
    answer,
    key_of,
    opened,
    stock,
    subscribe,
    switch_on,
    wait_ready,
)

# The folder of the tests and its number of items.
FOLDER = "gopip"
ITEMS = 300
GOPIP = "getObjPositionInParent"
# The merge policy of the order merge of ADR 0052, with the tier 2 entry of
# the class of the folder.
POLICY = {
    "classes": {
        "plone.app.contenttypes.content.Folder": {
            "tier2": "on",
            "default": "strict",
            "fields": {"modification_date": "max"},
        }
    },
    "split_merges": "on",
    "folder_orders": "on",
}


def item_id(i):
    return f"g-{i:03d}"


def listings(site):
    """The listings of the folder sorted on its order: both orders, a
    `sort_limit`, batches at the start, past the middle and in reverse, and
    a filter."""
    path = "/".join(site[FOLDER].getPhysicalPath())
    base = {"path": {"query": path, "depth": 1}, "sort_on": GOPIP}
    return [
        base,
        {**base, "sort_order": "reverse"},
        {**base, "sort_limit": 10},
        {**base, "sort_limit": 10, "sort_order": "reverse"},
        {**base, "sort_limit": 120},
        {**base, "b_start": 0, "b_size": 20},
        {**base, "b_start": 250, "b_size": 20},
        {**base, "b_start": 40, "b_size": 20, "sort_order": "reverse"},
        {**base, "portal_type": "Document"},
    ]


def make_folder(site):
    """The folder of the tests with its items, Documents and News Items."""
    site.invokeFactory("Folder", FOLDER, title="Order")
    folder = site[FOLDER]
    for i in range(ITEMS):
        kind = "News Item" if i % 4 == 3 else "Document"
        folder.invokeFactory(kind, item_id(i), title=f"Order {i}")


class OrderTests:
    """The tests of both layers."""

    def wait(self):
        wait_ready(self.storage, self.key)

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        subscribe(self)
        switch_on(self)
        with opened(self.db) as site:
            if FOLDER not in site.objectIds():
                make_folder(site)
                transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        with opened(self.db) as site:
            self.key = key_of(site)
        self.wait()
        catalog._counts.clear()

    def counts(self):
        queries = catalog._counts_now()["queries"]
        return queries.get("native", 0), queries.get("python", 0)

    def check(self, site, queries=None):
        """Each listing gives the brains of the Python path, with the same
        `len()` and `actual_result_count`, and counts as native. Return the
        answers."""
        queries = listings(site) if queries is None else queries
        native, python = self.counts()
        found = []
        for query in queries:
            expected = stock(site, query)
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, expected, query)
            found.append(expected)
        self.assertEqual(
            self.counts(), (native + len(queries), python), catalog._counts_now()
        )
        return found

    def commit_and_check(self, change):
        """Commit `change(folder)` in a transaction of its own, then check
        the listings."""
        with opened(self.db) as site:
            change(site[FOLDER])
            transaction.commit()
        self.wait()
        with opened(self.db) as site:
            return self.check(site)

    def test_each_listing_gives_the_order_of_the_python_path_natively(self):
        with opened(self.db) as site:
            found = self.check(site)
            ids = list(site[FOLDER].objectIds())
            self.assertGreaterEqual(found[0][1], ITEMS)
            # The listing follows the order of the folder.
            brains = site.portal_catalog.unrestrictedSearchResults(**listings(site)[0])
            self.assertEqual([b.getId for b in brains], ids)

    def test_a_move_a_rename_a_creation_and_a_delete_each_in_a_transaction(self):
        before = self.commit_and_check(lambda folder: None)
        moved = self.commit_and_check(
            lambda folder: folder.moveObjectsByDelta([item_id(10), item_id(20)], -5)
        )
        self.assertNotEqual(moved[0], before[0])
        self.commit_and_check(lambda folder: folder.moveObjectsToTop([item_id(299)]))

        def rename(folder):
            old, new = item_id(50), f"{item_id(50)}-renamed"
            if old not in folder.objectIds():
                old, new = new, old
            folder.manage_renameObject(old, new)

        self.commit_and_check(rename)
        self.commit_and_check(
            lambda folder: folder.invokeFactory("Document", "g-new", title="New")
        )
        self.commit_and_check(lambda folder: folder.manage_delObjects(["g-new"]))
        self.commit_and_check(lambda folder: folder.manage_delObjects([item_id(100)]))
        self.commit_and_check(
            lambda folder: folder.invokeFactory("Document", item_id(100), title="Back")
        )

    def test_a_listing_inside_a_transaction_after_a_move_stays_native(self):
        with opened(self.db) as site:
            folder = site[FOLDER]
            before = self.check(site)
            folder.moveObjectsByDelta([item_id(30), item_id(31)], 7)
            folder.moveObjectsToBottom([item_id(0)])
            self.assertTrue(site._p_jar._registered_objects)
            after = self.check(site)
            self.assertNotEqual(after[0], before[0])
            brains = site.portal_catalog.unrestrictedSearchResults(**listings(site)[0])
            self.assertEqual([b.getId for b in brains], list(folder.objectIds()))

    def test_a_listing_at_an_older_snapshot_gives_the_older_order(self):
        old = self.db.open(transaction_manager=transaction.TransactionManager())
        try:
            app = makerequest(old.root()["Application"])
            setSite(app.plone)
            queries = listings(app.plone)
            before = [
                answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
                for query in queries
            ]
            self.commit_and_check(
                lambda folder: folder.moveObjectsToTop([item_id(150), item_id(151)])
            )
            setSite(app.plone)
            native, python = self.counts()
            again = [
                answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
                for query in queries
            ]
            self.assertEqual(again, before)
            self.assertEqual(self.counts(), (native + len(queries), python))
        finally:
            setSite(None)
            old.transaction_manager.abort()
            old.close()

    def test_a_navigation_over_several_folders_takes_the_python_path_with_sort(self):
        with opened(self.db) as site:
            base = "/".join(site.getPhysicalPath())
            queries = [
                {
                    "path": {"query": f"{base}/{FOLDER}/{item_id(5)}", "navtree": 1},
                    "sort_on": GOPIP,
                },
                {"path": {"query": base, "depth": 3}, "sort_on": GOPIP},
                {"path": {"query": f"{base}/{FOLDER}", "depth": 2}, "sort_on": GOPIP},
                {"portal_type": "Document", "sort_on": GOPIP},
            ]
            native, python = self.counts()
            for query in queries:
                expected = stock(site, query)
                got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
                self.assertEqual(got, expected, query)
        counts = catalog._counts_now()
        self.assertEqual(self.counts(), (native, python + len(queries)))
        self.assertEqual(counts["fallbacks"][("sort", "client")], len(queries))


class TestTheOrderClause(OrderTests, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def test_after_two_creations_from_one_snapshot_that_the_order_merge_merges(self):
        with opened(self.db) as site:
            jar = site._p_jar
            kept = zoe.policy.read(jar)
            zoe.policy.install(jar, POLICY)
            transaction.commit()
        self.addCleanup(self.restore_policy, kept)
        new = ["g-merged-a", "g-merged-b"]
        # Two connections create one item each in the folder, from one
        # snapshot, without events, then commit one after the other: the
        # order merge of ADR 0052 merges the order and the positions.
        managers = [transaction.TransactionManager() for _ in new]
        connections = [self.db.open(transaction_manager=tm) for tm in managers]
        try:
            for connection, id in zip(connections, new, strict=True):
                folder = connection.root()["Application"]["plone"][FOLDER]
                folder._setOb(id, self.item(connection, id))
                # A creation changes the folder, which the tier 2 entry
                # merges.
                folder._p_changed = True
            for tm in managers:
                tm.commit()
        finally:
            for tm, connection in zip(managers, connections, strict=True):
                tm.abort()
                connection.close()
        with opened(self.db) as site:
            folder = site[FOLDER]
            ids = list(folder.objectIds())
            self.assertEqual(ids[-2:], new)
            for id in new:
                folder[id].indexObject()
            transaction.commit()
        self.wait()
        with opened(self.db) as site:
            self.check(site)
            brains = site.portal_catalog.unrestrictedSearchResults(**listings(site)[0])
            self.assertEqual([b.getId for b in brains][-2:], new)
            site[FOLDER].manage_delObjects(new)
            transaction.commit()

    def item(self, connection, id):
        from plone.dexterity.utils import createContent

        app = makerequest(connection.root()["Application"])
        setSite(app.plone)
        try:
            return createContent("Document", id=id, title=f"Merged {id}")
        finally:
            setSite(None)

    def restore_policy(self, kept):
        with opened(self.db) as site:
            jar = site._p_jar
            if kept is None:
                del jar.root()[zoe.policy.NAME]
            else:
                kept = dict(kept)
                kept.pop("version", None)
                zoe.policy.install(jar, kept)
            transaction.commit()


class TestTheOrderClauseOverZoeServe(OrderTests, unittest.TestCase):
    layer = zoe_testing.ZOE_SERVE_SITE

    def wait(self):
        test_catalog_zeo.wait_ready(self.storage, self.key)
