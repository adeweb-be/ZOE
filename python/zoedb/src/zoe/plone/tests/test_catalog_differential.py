"""Acceptance tests of the differential suite (ADR 0059, sections 7 and 9):
the mode `compare` of the catalog backend of `zoe.plone` on a test storage,
through a throwaway mirror, and on a ZOE store, through its own mirror.

A test storage is a `DemoStorage` over a `zoe.Storage`, at any depth. It
has no mirror. In the mode `compare`, each search there builds a throwaway
mirror from what the Connection sees, compares its answer with the stock
search, and returns the stock result. The fixed set of searches follows the
queries of `crates/zoe-catalog/tests/data/indexes.py` on the indexes of a
Plone site, so the plans of the hook meet the answers of ZCatalog on the
same shapes that the data of the mirror tests hold.

The tests also cover the paths of the hook that build the transaction
overlay: an error of the database goes up to the caller, an object that the
hook cannot name takes `overlay`, and an overlay that does not pickle logs
once for each class.

`bin/test -s zoe.plone` of the instance of the zoe scenario runs these
tests, and `just plone zoe backends` runs it.
"""

import contextlib
import logging
import pathlib
import tempfile
import threading
import time
import unittest
from unittest import mock

import transaction
import ZODB
from DateTime import DateTime
from persistent.mapping import PersistentMapping
from persistent.wref import WeakRef
from plone.app.testing import PLONE_FIXTURE
from plone.uuid.interfaces import IUUID
from ZODB.DemoStorage import DemoStorage
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import ConflictError, POSKeyError
from zope.event import notify
from zope.processlifetime import DatabaseOpenedWithRoot
from ZTUtils.Lazy import LazyMap

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

# The longest wait for a mirror to leave the store, in seconds.
TIMEOUT = 30

# A value that only the query of the planted mismatch holds. No log line,
# no count and no report may hold it.
MARKER = "marker-7f3c9e"

TYPES = ("Document", "News Item")
STATES = ("private", "pending", "published")
WORDS = ("alpha", "beta", "gamma", "delta", "eps", "zeta")

# The site of the differential suite: the layer of `ZOE_SITE`, a Plone
# site in a database whose storage is a `zoe.Storage`, in a store of its
# own. So the content of these tests changes no other test.
SITE = zoe_testing.ZoeSite(bases=(PLONE_FIXTURE,), name="ZoeDifferentialSite")


def day(n):
    """The date `n` days from a fixed start, at a whole hour plus 30
    minutes, so no date of the content falls on the minute of a search."""
    return DateTime("2026/01/01 10:30:00 UTC") + n


def make_content(site):
    """Three folders, each with a subfolder, and Documents and News Items in
    three workflow states, with subjects, dates, effective and expiry dates
    and the flag `exclude_from_nav`."""
    workflow = site.portal_workflow
    for f in range(3):
        site.invokeFactory("Folder", f"dif-{f}", title=f"Folder {f}")
        site[f"dif-{f}"].invokeFactory("Folder", "sub", title=f"Sub {f}")
    for i in range(36):
        folder = site[f"dif-{i % 3}"]
        if i % 4 == 3:
            folder = folder["sub"]
        name = f"item-{i:02d}"
        folder.invokeFactory(TYPES[i % 2], name, title=f"Item {i:02d}")
        obj = folder[name]
        state = STATES[i % 3]
        if state != "private":
            workflow.doActionFor(obj, "submit")
        if state == "published":
            workflow.doActionFor(obj, "publish")
        obj.setSubject(tuple(WORDS[j] for j in range(6) if (i * 7 >> j) & 1))
        obj.setEffectiveDate(day(-(i * 11 % 90)) if i % 5 else None)
        obj.setExpirationDate(day(i * 17 % 120 - 20) if i % 4 == 0 else None)
        obj.setModificationDate(day(-(i * 13 % 80)))
        obj.exclude_from_nav = i % 6 == 0
        obj.reindexObject()


def minutes(site, date):
    """The key of `date` in the DateIndex `effective`."""
    return site.portal_catalog._catalog.getIndex("effective")._convert(date)


def queries(site):
    """The fixed set of searches, by name: the queries of `indexes.py` on
    the indexes of a Plone site, the path index, and the corner of `[None]`
    on a KeywordIndex."""
    base = "/".join(site.getPhysicalPath())
    found = site.portal_catalog.unrestrictedSearchResults(
        portal_type=list(TYPES), sort_on="getId"
    )
    objects = [brain.getObject() for brain in found]
    uids = [IUUID(obj) for obj in objects[:5]]
    effective = next(o.effective() for o in objects if o.EffectiveDate() != "None")
    expires = next(o.expires() for o in objects if o.ExpirationDate() != "None")
    modified = objects[3].modified()
    start = day(0)
    return {
        "kw-one": {"Subject": "alpha"},
        "kw-or": {"Subject": ["alpha", "beta"]},
        "kw-and": {"Subject": {"query": ["alpha", "beta"], "operator": "and"}},
        "kw-and-absent": {
            "Subject": {"query": ["alpha", "nothing"], "operator": "and"}
        },
        "kw-or-absent": {"Subject": {"query": ["nothing", "gamma"]}},
        "kw-not": {"Subject": {"query": ["alpha", "beta", "gamma"], "not": "delta"}},
        "kw-not-only": {
            "Subject": {"not": ["alpha", "eps"]},
            "portal_type": "Document",
        },
        "kw-not-only-alone": {"Subject": {"not": "zeta"}},
        "kw-not-only-absent": {"Subject": {"not": "nothing"}},
        "kw-none-not": {"Subject": {"query": [None], "not": "alpha"}},
        "fi-none-not": {"portal_type": {"query": [None], "not": "Document"}},
        "kw-none-key-not": {"Subject": {"query": [None, "alpha"], "not": "beta"}},
        "kw-none-range-not": {
            "Subject": {"query": [None], "range": "min", "not": "beta"}
        },
        "kw-range": {"Subject": {"query": ["beta", "eps"], "range": "min:max"}},
        "kw-range-min": {"Subject": {"query": "delta", "range": "min"}},
        "kw-range-not": {
            "Subject": {"query": ["alpha", "eps"], "range": "min:max", "not": "beta"}
        },
        "kw-roles": {"allowedRolesAndUsers": ["Anonymous", "Manager"]},
        "date-eq": {"modified": modified},
        "date-range": {"modified": {"query": [day(-60), day(-10)], "range": "min:max"}},
        "date-range-min": {"modified": {"query": day(-20), "range": "min"}},
        "date-range-max": {"modified": {"query": day(-40), "range": "max"}},
        "date-not-only": {"effective": {"not": effective}, "portal_type": "Document"},
        "date-range-not-minutes": {
            "effective": {
                "query": [day(-80), day(0)],
                "range": "min:max",
                "not": [minutes(site, effective)],
            }
        },
        "date-sort-desc-limit": {
            "portal_type": "Document",
            "sort_on": "modified",
            "sort_order": "reverse",
            "sort_limit": 5,
        },
        "date-sort-asc-limit": {
            "portal_type": list(TYPES),
            "sort_on": "modified",
            "sort_limit": 7,
        },
        "date-sort-all": {"review_state": "published", "sort_on": "modified"},
        "date-sort-effective": {
            "Subject": ["alpha", "beta"],
            "sort_on": "effective",
            "sort_order": "descending",
        },
        "date-sort-batch": {
            "portal_type": ["Document", "Folder"],
            "sort_on": "modified",
            "b_start": 4,
            "b_size": 5,
        },
        "date-multisort": {
            "review_state": "private",
            "sort_on": ["portal_type", "modified"],
            "sort_order": ["ascending", "descending"],
            "sort_limit": 10,
        },
        "drange-now": {"effectiveRange": start},
        "drange-now-type": {"effectiveRange": start, "portal_type": "Document"},
        "drange-early": {"effectiveRange": day(-500), "review_state": "published"},
        "drange-late": {"effectiveRange": day(400), "review_state": "published"},
        "drange-at-effective": {
            "effectiveRange": effective,
            "portal_type": "News Item",
        },
        "drange-at-expires": {"effectiveRange": expires, "portal_type": "News Item"},
        "bool-true": {"is_folderish": True},
        "bool-false": {"is_folderish": False, "portal_type": "Document"},
        "bool-list": {"is_folderish": [0, 1]},
        "bool-empty": {"is_folderish": {"query": []}},
        "bool-nav": {"exclude_from_nav": True},
        "uuid-one": {"UID": uids[0]},
        "uuid-list": {"UID": uids},
        "uuid-absent": {"UID": "0" * 32},
        "uuid-not": {"UID": {"not": uids[1:3]}, "portal_type": "Document"},
        "uuid-range": {"UID": {"query": ["4", "8"], "range": "min:max"}},
        "uuid-sort": {"portal_type": "News Item", "sort_on": "UID", "sort_limit": 4},
        "uuid-sort-all": {"review_state": "pending", "sort_on": "UID"},
        "path-depth": {"path": {"query": f"{base}/dif-0", "depth": 1}},
        "path-all": {"path": f"{base}/dif-1"},
        "path-several": {"path": {"query": [f"{base}/dif-0", f"{base}/dif-2/sub"]}},
        "path-navtree": {
            "path": {"query": f"{base}/dif-1/sub/item-07", "navtree": 1},
        },
        "path-navtree-start": {
            "path": {
                "query": f"{base}/dif-1/sub/item-07",
                "navtree": 1,
                "navtree_start": 1,
            },
        },
        "path-sort": {
            "path": {"query": f"{base}/dif-2", "depth": 2},
            "portal_type": list(TYPES),
            "sort_on": "sortable_title",
            "sort_order": "reverse",
        },
        "five-indexes": {
            "portal_type": list(TYPES),
            "review_state": "published",
            "Subject": ["alpha", "beta", "gamma"],
            "effectiveRange": start,
            "allowedRolesAndUsers": ["Anonymous", "Manager"],
            "sort_on": "modified",
            "sort_order": "reverse",
            "sort_limit": 10,
        },
        "five-indexes-member": {
            "portal_type": list(TYPES),
            "review_state": ["private", "pending"],
            "Subject": {"not": "zeta"},
            "effectiveRange": start,
            "allowedRolesAndUsers": ["Owner", "Manager"],
            "sort_on": "modified",
            "sort_limit": 10,
        },
    }


# The searches of the fixed set that take the Python path, with their
# reason. Keys that are all None with `not`: ZCatalog gives no row, and the
# plan would read as a query of `not` alone.
EXPECTED = {
    "kw-none-not": ("option", "client"),
    "fi-none-not": ("option", "client"),
}


@contextlib.contextmanager
def test_storage(storage, depth=1):
    """Yield a database on `depth` `DemoStorage`s over `storage`. The base
    stays open at the end."""
    top = storage
    for _ in range(depth):
        top = DemoStorage(base=top, close_base_on_close=False)
    db = ZODB.DB(top)
    try:
        yield db
    finally:
        db.close()


def counts():
    return catalog._counts_now()


def prepared(site):
    """The fixed set of searches of `site`, with the counts at zero after
    the search that makes it."""
    named = queries(site)
    catalog._counts.clear()
    return named


def setup_site(test, mode):
    """Give the site of the layer its content, install the backend in
    `mode`, and set the counts to zero."""
    test.db = test.layer["zoeDB"]
    test.storage = test.layer["zoeStorage"]
    with opened(test.db) as site:
        if "dif-0" not in site.objectIds():
            make_content(site)
            transaction.commit()
        test.key = key_of(site)
    switch_on(test, mode)
    catalog._counts.clear()
    catalog._mismatched.clear()
    catalog._logged.clear()
    test.addCleanup(catalog._logged.clear)


def empty_cache(site):
    """Drop the cache of ZCatalog in the request of `site`."""
    site.REQUEST.other.pop("__catalog_cache__", None)


def run(test, site, named):
    """Run each search of `named` through the hook. Return, by name, None
    for a search that the mode `compare` compared, or the reason of its
    Python path with its side. Each result equals the stock result."""
    out = {}
    for name, query in named.items():
        before = counts()
        # Each search starts with an empty cache of the request: a hit of
        # that cache can give another stock answer than a miss.
        empty_cache(site)
        expected = stock(site, query)
        empty_cache(site)
        got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
        test.assertEqual(got, expected, name)
        after = counts()
        if after["compared"] > before["compared"]:
            out[name] = None
            continue
        grown = [
            key
            for key, n in after["fallbacks"].items()
            if n > before["fallbacks"].get(key, 0)
        ]
        test.assertEqual(len(grown), 1, (name, grown))
        out[name] = grown[0]
    return out


def no_mirror(test, storage, key):
    """The store has no registration key and, once a mirror of a former
    test drops, no mirror for `key`."""
    test.assertEqual(list(storage.kv_scan("durable", catalog.NAMESPACE)), [])
    deadline = time.monotonic() + TIMEOUT
    while storage._catalog_status(key) and time.monotonic() < deadline:
        time.sleep(0.05)
    test.assertIsNone(storage._catalog_status(key))


class TestTheModeCompareOnATestStorage(unittest.TestCase):
    layer = SITE

    def setUp(self):
        setup_site(self, "compare")
        no_mirror(self, self.storage, self.key)

    def test_each_search_of_a_committed_state_is_compared_through_a_throwaway(self):
        with test_storage(self.storage) as db, opened(db) as site:
            named = prepared(site)
            got = run(self, site, named)
        compared = [name for name, reason in got.items() if reason is None]
        # Each search that the mirror does not serve takes a reason of the
        # table of ADR 0059, section 7.
        refused = {name: reason for name, reason in got.items() if reason}
        self.assertEqual(refused, EXPECTED, refused)
        now = counts()
        served = len(named) - len(EXPECTED)
        self.assertEqual(len(compared), served)
        self.assertEqual(now["compared"], served)
        self.assertEqual(now["throwaways"], served)
        self.assertEqual(now["queries"], {"native": served, "python": len(EXPECTED)})
        self.assertEqual(now["mismatches"], 0, catalog._mismatched)
        no_mirror(self, self.storage, self.key)

    def test_each_search_in_a_transaction_that_changed_the_catalog_is_compared(self):
        sent = []
        original = catalog._overlay

        def spy(jar):
            records = original(jar)
            sent.append(len(records))
            return records

        with (
            mock.patch.object(catalog, "_overlay", spy),
            test_storage(self.storage, depth=2) as db,
            opened(db) as site,
        ):
            named = prepared(site)
            site["dif-0"].invokeFactory("Document", "new-one", title="Item 99 new")
            new = site["dif-0"]["new-one"]
            new.setSubject(("alpha", "zeta"))
            new.setEffectiveDate(day(-3))
            new.reindexObject()
            transaction.savepoint()
            # After the savepoint: a change, a move and a delete.
            site["dif-1"]["item-01"].setTitle("Changed title")
            site["dif-1"]["item-01"].setSubject(("beta",))
            site["dif-1"]["item-01"].reindexObject()
            site["dif-2"].manage_renameObject("item-02", "item-02-moved")
            site["dif-0"].manage_delObjects(["item-00"])
            # The changes search the catalog too: count the fixed set only.
            catalog._counts.clear()
            sent.clear()
            got = run(self, site, named)
        refused = {name: reason for name, reason in got.items() if reason}
        self.assertEqual(refused, EXPECTED, refused)
        now = counts()
        served = len(named) - len(EXPECTED)
        self.assertEqual(now["compared"], served)
        self.assertEqual(now["throwaways"], served)
        self.assertEqual(now["mismatches"], 0, catalog._mismatched)
        # Each search with a plan sent the records of the transaction.
        self.assertEqual(len(sent), served)
        self.assertTrue(all(sent), sent)
        no_mirror(self, self.storage, self.key)

    def test_a_planted_mismatch_is_found_logged_with_no_value_and_counted(self):
        def plant(result):
            rids = list(reversed(result._seq))
            return LazyMap(result._func, rids, len(rids), actual_result_count=len(rids))

        query = {
            "portal_type": list(TYPES),
            "Subject": {"not": MARKER},
            "sort_on": "getId",
        }
        with (
            test_storage(self.storage) as db,
            opened(db) as site,
            mock.patch.object(catalog, "_plant", plant),
            self.assertLogs("zoe.plone.catalog", logging.ERROR) as logs,
        ):
            expected = stock(site, query)
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            # The mode `compare` returns the stock result.
            self.assertEqual(got, expected)
        now = counts()
        self.assertEqual(now["mismatches"], 1)
        self.assertEqual(now["cache_differences"], 0)
        text = "\n".join(logs.output)
        for name in ("Subject", "portal_type", "getId", "rids"):
            self.assertIn(name, text)
        for value in (MARKER, "Document", "News Item", "/plone"):
            self.assertNotIn(value, text)
        self.assertEqual(len(catalog._mismatched), 1)
        self.assertNotIn(MARKER, catalog._mismatched[0])

    def test_a_difference_of_the_cache_of_the_request_is_no_mismatch(self):
        with test_storage(self.storage) as db, opened(db) as site:
            query = prepared(site)["date-range-not-minutes"]
            with self.assertLogs("zoe.plone.catalog", logging.WARNING) as logs:
                # In one request, ZCatalog keeps the interim set of the
                # range, and converts the keys of `not` on a hit of its
                # cache: the second stock answer differs from the first.
                first = answer(site.portal_catalog.unrestrictedSearchResults(**query))
                second = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertNotEqual(first, second)
        now = counts()
        self.assertEqual(now["compared"], 2)
        self.assertEqual(now["mismatches"], 0, catalog._mismatched)
        self.assertEqual(now["cache_differences"], 1)
        text = "\n".join(logs.output)
        self.assertIn("cache of the request", text)
        self.assertIn("effective", text)

    def test_a_search_under_a_mock_of_the_catalog_is_unchecked(self):
        from Products.ZCatalog.Catalog import Catalog

        original = Catalog._search_index

        def mocked(catalog, *args):
            return original(catalog, *args)

        with (
            test_storage(self.storage) as db,
            opened(db) as site,
            mock.patch.object(Catalog, "_search_index", mocked),
        ):
            site.portal_catalog.unrestrictedSearchResults(portal_type="Document")
        now = counts()
        self.assertEqual(now["unchecked"], {"patched": 1})
        self.assertEqual(now["compared"], 0)
        self.assertEqual(now["mismatches"], 0)

    def test_a_brain_that_differs_is_a_mismatch(self):
        def plant(result):
            def other(rid):
                brain = result._func(rid)
                brain.__dict__["getPath"] = lambda: "/elsewhere"
                return brain

            return LazyMap(other, list(result._seq), len(result))

        query = {"portal_type": list(TYPES), "sort_on": "getId", "sort_limit": 5}
        with (
            test_storage(self.storage) as db,
            opened(db) as site,
            mock.patch.object(catalog, "_plant", plant),
            self.assertLogs("zoe.plone.catalog", logging.ERROR) as logs,
        ):
            site.portal_catalog.unrestrictedSearchResults(**query)
        self.assertEqual(counts()["mismatches"], 1)
        self.assertIn("a mismatch of rows", "\n".join(logs.output))
        self.assertNotIn("/elsewhere", "\n".join(logs.output))

    def test_only_a_method_that_a_search_calls_makes_it_unchecked(self):
        from Products.ZCatalog.Catalog import Catalog

        class Plain(Catalog):
            pass

        class Override(Catalog):
            def _search_index(self, *args):
                return super()._search_index(*args)

        self.assertIsNone(catalog._unchecked(Catalog()))
        self.assertIsNone(catalog._unchecked(Plain()))
        self.assertEqual(catalog._unchecked(Override()), "subclass")
        with mock.patch.object(Catalog, "addColumn", lambda *a: None):
            self.assertIsNone(catalog._unchecked(Catalog()))
        with mock.patch.object(Catalog, "sortResults", lambda *a: None):
            self.assertEqual(catalog._unchecked(Catalog()), "patched")

    def test_in_mode_on_a_test_storage_takes_unsupported(self):
        catalog._installed["mode"] = "on"
        with test_storage(self.storage) as db, opened(db) as site:
            got = run(self, site, {"one": {"portal_type": "Document"}})
        self.assertEqual(got, {"one": ("unsupported", "client")})
        now = counts()
        self.assertEqual(now["throwaways"], 0)
        self.assertEqual(now["compared"], 0)


class TestTheModeCompareOnAZoeStore(unittest.TestCase):
    """The layer of the mirror of a store: a `zoe.Storage` with no
    `DemoStorage` on it, as `ZOE_SITE`."""

    layer = SITE

    def setUp(self):
        setup_site(self, "compare")
        subscribe(self)
        notify(DatabaseOpenedWithRoot(self.db))
        wait_ready(self.storage, self.key)

    def test_a_search_is_compared_through_the_mirror_of_the_store(self):
        with opened(self.db) as site:
            named = prepared(site)
            before = self.storage._catalog_counts()["zoe_catalog_queries_total"]
            got = run(self, site, named)
        refused = {name: reason for name, reason in got.items() if reason}
        self.assertEqual(refused, EXPECTED, refused)
        served = len(named) - len(EXPECTED)
        now = counts()
        self.assertEqual(now["compared"], served)
        self.assertEqual(now["throwaways"], 0)
        self.assertEqual(now["mismatches"], 0, catalog._mismatched)
        after = self.storage._catalog_counts()["zoe_catalog_queries_total"]
        self.assertEqual(after["native"], before["native"] + served)


class TestThePathsOfTheOverlay(unittest.TestCase):
    """The paths of the hook that pickles the transaction overlay, on the
    mirror of a ZOE store in the mode `on`."""

    layer = SITE

    def setUp(self):
        setup_site(self, "on")
        subscribe(self)
        notify(DatabaseOpenedWithRoot(self.db))
        wait_ready(self.storage, self.key)
        catalog._counts.clear()

    def search(self, site):
        query = {"portal_type": "Document"}
        got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
        self.assertEqual(got, stock(site, query))

    def change(self, site):
        """Change an item, so the Connection holds a registered object."""
        item = site["dif-0"]["item-06"]
        item.setTitle("Changed")
        return item

    def test_a_conflict_or_a_missing_record_of_a_pickle_goes_up(self):
        for error in (ConflictError, POSKeyError):
            writer = catalog._overlay_writer()
            with (
                opened(self.db) as site,
                mock.patch.object(writer, "serialize", side_effect=error()),
            ):
                self.change(site)
                with self.assertRaises(error):
                    site.portal_catalog.unrestrictedSearchResults(
                        portal_type="Document"
                    )
        self.assertEqual(counts()["fallbacks"].get(("overlay", "client"), 0), 0)

    def test_a_weak_reference_into_another_database_takes_overlay(self):
        other = ZODB.DB(MappingStorage())
        self.addCleanup(other.close)
        manager = transaction.TransactionManager()
        connection = other.open(transaction_manager=manager)
        self.addCleanup(connection.close)
        self.addCleanup(manager.abort)
        with opened(self.db) as site:
            target = PersistentMapping()
            # The reference has no oid: its target had none when it was
            # made. The target then gets an oid in another database.
            reference = WeakRef(target)
            connection.add(target)
            self.change(site).reference = reference
            self.search(site)
        self.assertEqual(counts()["fallbacks"][("overlay", "client")], 1)

    def test_a_persistent_class_with_no_oid_takes_overlay(self):
        from ZODB.persistentclass import PersistentMetaClass

        klass = PersistentMetaClass("Floating", (object,), {"__module__": __name__})
        self.assertIsNone(klass._p_oid)
        with opened(self.db) as site:
            self.change(site).floating = klass
            self.search(site)
        self.assertEqual(counts()["fallbacks"][("overlay", "client")], 1)

    def test_an_overlay_that_does_not_pickle_logs_once_for_each_class(self):
        with self.assertLogs("zoe.plone.catalog", logging.WARNING) as logs:
            for _ in range(2):
                with opened(self.db) as site:
                    self.change(site).lock = threading.Lock()
                    self.search(site)
            with (
                opened(self.db) as site,
                mock.patch.object(catalog, "_overlay", side_effect=RuntimeError("x")),
            ):
                self.change(site)
                self.search(site)
                self.search(site)
        self.assertEqual(counts()["fallbacks"][("overlay", "client")], 4)
        pickled = [line for line in logs.output if "does not pickle" in line]
        # One line for the error of the lock, one for the RuntimeError.
        self.assertEqual(len(pickled), 2, logs.output)
        self.assertEqual(len([line for line in pickled if "RuntimeError" in line]), 1)


class TestThePlansOfTheMirrorData(unittest.TestCase):
    """The hook and `crates/zoe-catalog/tests/data/indexes.py`, which writes
    the plans of the tests of the mirror, give each query of that script
    the same plan."""

    layer = SITE

    def setUp(self):
        switch_on(self, "compare")

    def script(self):
        here = pathlib.Path(__file__).resolve()
        for parent in here.parents:
            found = parent / "crates" / "zoe-catalog" / "tests" / "data" / "indexes.py"
            if found.exists():
                return found
        self.skipTest("the tree of the repository is not above this module")

    def test_the_hook_and_the_data_script_give_the_same_plans(self):
        import runpy
        import sys

        import Products.ZCatalog.Catalog as catalog_module
        from Products.ZCatalog.Catalog import Catalog
        from ZTUtils.Lazy import LazyCat

        script = self.script()
        kept = (Catalog.instantiate, catalog_module.randint, sys.argv)
        transaction.abort()
        with tempfile.TemporaryDirectory() as directory:
            sys.argv = [str(script), f"{directory}/indexes.json"]
            try:
                names = runpy.run_path(str(script), run_name="indexes")
            finally:
                Catalog.instantiate, catalog_module.randint, sys.argv = kept
                transaction.abort()
        cat = names["cat"]
        compared = 0
        for name, query in names["QUERIES"].items():
            seen = []

            def capture(self, *args, _seen=seen, **kw):
                _seen.append((args, kw))
                return LazyCat([])

            with mock.patch.object(Catalog, "search", capture):
                cat.searchResults(dict(query))
            args, kw = seen[0]
            plan, _ = catalog.plan_of(cat, *args, **kw)
            theirs = names["plan_of"](query)
            if plan is None or theirs is None:
                self.assertEqual(plan, theirs, name)
                continue
            mine = {key: plan[key] for key in ("s", "l", "bs", "bz")}
            mine["c"] = sorted(plan["c"], key=lambda c: c["i"])
            theirs = dict(theirs, c=sorted(theirs["c"], key=lambda c: c["i"]))
            self.assertEqual(mine, theirs, name)
            compared += 1
        self.assertEqual(compared, len(names["QUERIES"]))
