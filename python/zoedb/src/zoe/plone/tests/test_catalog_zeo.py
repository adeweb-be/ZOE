"""Acceptance tests of the catalog backend of `zoe.plone` over `zoe serve`
(ADR 0059, section 9): the searches of a Plone site whose storage is a
`zoe.zeo.ClientStorage` with the native client inside, answered by the
catalog mirror of the server.

The site of `zoe.plone.testing.ZOE_SERVE_SITE` lives in a database whose
storage is the native client of a `zoe serve`, with no `DemoStorage` on
it, so the server holds the mirror. The registration writes the key of the
site through `zoe.kv` on the native client. A stock `ZEO.ClientStorage` of
the same server takes the Python path, with the reason `unsupported`.
`just plone zoe backends` runs these tests; the server is the `zoe` binary
that `ZOE_BINARY` names.
"""

import shutil
import tempfile
import time
import unittest

import transaction
import ZODB
from zope.event import notify
from zope.processlifetime import DatabaseOpenedWithRoot

from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests.test_catalog_mirror import (
    QUERIES,
    TIMEOUT,
    TYPES,
    answer,
    key_of,
    keys,
    opened,
    stock,
    subscribe,
    switch_on,
)

# A plan without clauses: the mirror answers it once it is ready.
EMPTY = {"c": [], "s": [], "l": None, "bs": 0, "bz": None, "cols": None, "rn": 0}


def after(tid):
    """The exclusive tid of the snapshot that holds `tid`."""
    return (int.from_bytes(tid, "big") + 1).to_bytes(8, "big")


def wait_ready(storage, key):
    """Wait until the mirror of the server answers on the catalog `key`."""
    deadline = time.monotonic() + TIMEOUT
    while True:
        reply = storage.catalog_query(key, after(storage.lastTransaction()), EMPTY)
        if "f" not in reply:
            return
        if time.monotonic() > deadline:
            raise AssertionError(f"the mirror of the server is not ready: {reply}")
        time.sleep(0.05)


class TestTheCatalogMirrorOverZoeServe(unittest.TestCase):
    layer = zoe_testing.ZOE_SERVE_SITE

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

    def test_the_registration_writes_the_key_through_the_native_client(self):
        self.assertTrue(self.storage.catalog_supported())
        self.assertEqual(keys(self.storage), {self.key})

    def test_the_searches_give_the_brains_of_the_python_path_and_count_as_native(self):
        with opened(self.db) as site:
            for query in QUERIES:
                expected = stock(site, query)
                got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
                self.assertEqual(got, expected, query)
                self.assertTrue(expected[0], query)
        counts = catalog._counts_now()
        self.assertEqual(counts["queries"].get("native"), len(QUERIES))
        self.assertNotIn("python", counts["queries"])

    def test_a_search_sees_a_commit_of_its_own_connection(self):
        query = {"portal_type": "Document"}
        with opened(self.db) as site:
            before = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            site.invokeFactory("Document", "later", title="Later")
            transaction.commit()
            later = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(len(later[0]), len(before[0]) + 1)
            self.assertEqual(later, stock(site, query))
            site.manage_delObjects(["later"])
            transaction.commit()
        self.assertEqual(catalog._counts_now()["queries"].get("native"), 2)

    def test_a_stock_zeo_client_takes_the_python_path_unsupported(self):
        from ZEO.ClientStorage import ClientStorage

        server = self.layer["zoeServer"]
        directory = tempfile.mkdtemp(prefix="zoe-plone-stock-")
        self.addCleanup(shutil.rmtree, directory, True)
        db = ZODB.DB(
            ClientStorage(
                server.address,
                blob_dir=f"{directory}/blobs",
                shared_blob_dir=False,
                wait_timeout=TIMEOUT,
            )
        )
        try:
            with opened(db) as site:
                for query in QUERIES[:3]:
                    expected = stock(site, query)
                    got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
                    self.assertEqual(got, expected, query)
        finally:
            db.close()
        counts = catalog._counts_now()
        self.assertEqual(counts["fallbacks"][("unsupported", "client")], 3)
        self.assertNotIn("native", counts["queries"])
