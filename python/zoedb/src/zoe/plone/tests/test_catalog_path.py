"""Acceptance tests of the catalog backend of `zoe.plone` (ADR 0059) on the
path index: the folder listings, the navigation and the searches of a
section of a Plone site, answered by the catalog mirror of a ZOE store,
give the brains of the Python path.

The site of `zoe.plone.testing.ZOE_SITE` lives in a database whose storage
is a `zoe.Storage` itself, so its store holds the mirror. `bin/test -s
zoe.plone` of the instance of the zoe scenario runs these tests, and
`just plone zoe backends` runs it.
"""

import unittest

import transaction
from Testing.makerequest import makerequest

from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests.test_catalog_mirror import (
    answer,
    key_of,
    opened,
    stock,
    subscribe,
    switch_on,
    wait_ready,
)


def make_content(site):
    """Three folders `pth-0` to `pth-2`, each with three subfolders of
    three Documents; `pth-0` also holds `mv`, a folder of three levels:
    `mv`, its Documents and its folder `deep` with two Documents."""
    for i in range(3):
        site.invokeFactory("Folder", f"pth-{i}", title=f"Path {i}")
        folder = site[f"pth-{i}"]
        for j in range(3):
            folder.invokeFactory("Folder", f"sub-{j}", title=f"Sub {i}{j}")
            for k in range(3):
                folder[f"sub-{j}"].invokeFactory(
                    "Document", f"doc-{k}", title=f"Doc {(i * 7 + j * 3 + k) % 10}"
                )
    folder = site["pth-0"]
    folder.invokeFactory("Folder", "mv", title="Moved")
    folder["mv"].invokeFactory("Document", "doc-0", title="Moved doc")
    folder["mv"].invokeFactory("Folder", "deep", title="Deep")
    for k in range(2):
        folder["mv"]["deep"].invokeFactory("Document", f"doc-{k}", title=f"Deep {k}")


def searches():
    """The searches of the tests: folder listings, the site, several paths,
    the navigation and the search of a section."""
    return [
        # A folder listing at depth 1.
        {"path": {"query": "/plone/pth-0", "depth": 1}},
        {"path": {"query": "/plone/pth-1/sub-0", "depth": 1}},
        # The site at depth -1.
        {"path": {"query": "/plone", "depth": -1}},
        {"path": "/plone"},
        {"path": {"query": "/plone", "depth": 2}},
        {"path": "/plone/pth-2"},
        # Several paths in one clause.
        {"path": ["/plone/pth-0/sub-1", "/plone/pth-2"]},
        {"path": {"query": ["/plone/pth-0", "/plone/pth-1"], "depth": 1}},
        # `navtree` and `navtree_start`, as the navigation portlet sends
        # them for a portlet without and with a top level.
        {"path": {"query": "/plone/pth-1/sub-0/doc-1", "navtree": 1}},
        {
            "path": {
                "query": "/plone/pth-1/sub-0/doc-1",
                "navtree": 1,
                "navtree_start": 2,
            },
            "portal_type": ["Document", "Folder"],
        },
        {"path": {"query": "/plone/pth-0/mv/deep", "navtree": 1, "depth": 2}},
        {"path": {"query": "sub-1", "level": 2}},
        # A path with portal_type and a sort on sortable_title.
        {
            "path": "/plone/pth-0",
            "portal_type": "Document",
            "sort_on": "sortable_title",
        },
        {
            "path": {"query": "/plone/pth-2", "depth": 2},
            "portal_type": ["Document", "Folder"],
            "sort_on": "sortable_title",
            "sort_order": "reverse",
            "sort_limit": 5,
        },
    ]


class TestThePathIndexOfTheCatalogMirror(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        subscribe(self)
        switch_on(self)
        with opened(self.db) as site:
            if "pth-0" not in site.objectIds():
                make_content(site)
                transaction.commit()
        from zope.event import notify
        from zope.processlifetime import DatabaseOpenedWithRoot

        notify(DatabaseOpenedWithRoot(self.db))
        with opened(self.db) as site:
            self.key = key_of(site)
        wait_ready(self.storage, self.key)
        catalog._counts.clear()

    def counts(self):
        """The searches of the process on the native path and on the Python
        path."""
        queries = catalog._counts_now()["queries"]
        return queries.get("native", 0), queries.get("python", 0)

    def check(self, site, queries):
        """Each search gives the brains of the Python path, with the same
        `len()` and `actual_result_count`, natively. Return the answers."""
        native, python = self.counts()
        found = []
        for query in queries:
            expected = stock(site, query)
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, expected, query)
            found.append(expected)
        self.assertEqual(self.counts(), (native + len(queries), python))
        return found

    def test_each_path_search_gives_the_brains_of_the_python_path(self):
        with opened(self.db) as site:
            found = self.check(site, searches())
            # The searches find content.
            self.assertTrue(all(each[2] for each in found[:10]))
            # The listing of a subfolder gives its three Documents.
            self.assertEqual(found[1][2], 3)

    def test_a_rename_a_move_and_a_delete_then_the_same_searches(self):
        old = self.db.open(transaction_manager=transaction.TransactionManager())
        try:
            app = makerequest(old.root()["Application"])
            before = [
                answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
                for query in searches()
            ]
            # A rename of a folder of two levels.
            with opened(self.db) as site:
                site["pth-1"].manage_renameObject("sub-2", "sub-renamed")
                transaction.commit()
            with opened(self.db) as site:
                self.check(site, searches())
            # A move of a folder of three levels to another folder.
            with opened(self.db) as site:
                data = site["pth-0"].manage_cutObjects(["mv"])
                site["pth-2"].manage_pasteObjects(data)
                transaction.commit()
            with opened(self.db) as site:
                self.assertIn("mv", site["pth-2"].objectIds())
                found = self.check(site, [*searches(), {"path": "/plone/pth-2/mv"}])
                self.assertEqual(found[-1][2], 5)
            # A delete of a folder and its subtree.
            with opened(self.db) as site:
                site["pth-2"].manage_delObjects(["sub-0"])
                transaction.commit()
            with opened(self.db) as site:
                self.check(site, searches())
            # The old snapshot gives the old results, natively.
            native, python = self.counts()
            again = [
                answer(app.plone.portal_catalog.unrestrictedSearchResults(**query))
                for query in searches()
            ]
            self.assertEqual(again, before)
            self.assertEqual(self.counts(), (native + len(searches()), python))
        finally:
            old.transaction_manager.abort()
            old.close()
            with opened(self.db) as site:
                if "mv" in site["pth-2"].objectIds():
                    site["pth-0"].manage_pasteObjects(
                        site["pth-2"].manage_cutObjects(["mv"])
                    )
                if "sub-renamed" in site["pth-1"].objectIds():
                    site["pth-1"].manage_renameObject("sub-renamed", "sub-2")
                transaction.commit()

    def test_a_sort_on_path_and_a_search_of_one_object_take_the_python_path(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            for query in (
                {"portal_type": "Document", "sort_on": "path"},
                # `_index_items`, which the mirror does not read.
                {"path": {"query": "/plone/pth-0/sub-1/doc-1", "depth": 0}},
            ):
                expected = stock(site, query)
                got = pc.unrestrictedSearchResults(**query)
                self.assertEqual(answer(got), expected, query)
                self.assertTrue(expected[2], query)
        counts = catalog._counts_now()
        self.assertEqual(counts["queries"].get("python"), 2)
        self.assertEqual(counts["fallbacks"][("sort", "client")], 1)
        server = self.storage._catalog_counts()
        self.assertGreaterEqual(
            server["zoe_catalog_fallbacks_total"].get("index_unavailable", 0), 1
        )
