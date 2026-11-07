"""Acceptance tests of the catalog backend of `zoe.plone` (ADR 0059) on
KeywordIndex, DateIndex, DateRangeIndex, BooleanIndex and UUIDIndex: the
searches of a Plone site, answered by the catalog mirror of a ZOE store,
give the brains of the Python path.

The site of `zoe.plone.testing.ZOE_SITE` lives in a database whose storage
is a `zoe.Storage` itself, so its store holds the mirror. `bin/test -s
zoe.plone` of the instance of the zoe scenario runs these tests, and
`just plone zoe backends` runs it.
"""

import unittest
from unittest import mock

import transaction
from DateTime import DateTime
from plone.app.testing import SITE_OWNER_NAME
from plone.testing import zope as zope_testing
from plone.uuid.interfaces import IUUID

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

# The member of the site that searches with `searchResults`.
MEMBER = "catalog-member"

WORDS = ("alpha", "beta", "gamma", "delta")

# The days of the dates of the content, from now.
EFFECTIVE = (-30, -10, -3, 5, 20)
EXPIRES = (-5, 15, 40)


def days(n):
    """The date `n` days from now, at a whole hour, so that no date of the
    content falls on the minute of a search."""
    now = DateTime()
    return DateTime(now.year(), now.month(), now.day(), now.hour(), 0, 0) + n + 0.5


def items(site):
    """The content of the tests: Documents and News Items in three workflow
    states, with subjects, dates, and effective and expiry dates."""
    return [site[f"idx-{i}"] for i in range(24) if f"idx-{i}" in site.objectIds()]


def make_content(site):
    for i in range(24):
        name = f"idx-{i}"
        site.invokeFactory(
            ("Document", "News Item")[i % 2], name, title=f"Index item {i:02d}"
        )
        obj = site[name]
        state = i % 3
        if state:
            site.portal_workflow.doActionFor(obj, "submit")
        if state == 2:
            site.portal_workflow.doActionFor(obj, "publish")
        obj.setSubject(tuple(WORDS[j] for j in range(4) if (i >> j) & 1))
        obj.setEffectiveDate(days(EFFECTIVE[i % 5]) if i % 7 else None)
        obj.setExpirationDate(days(EXPIRES[i % 3]) if i % 4 == 0 else None)
        obj.setModificationDate(days(-(i * 13 % 50)))
        obj.reindexObject(
            idxs=["Subject", "effective", "expires", "effectiveRange", "modified"]
        )


def queries(site):
    """The searches of the tests: each option of each index type."""
    uids = [IUUID(obj) for obj in items(site)[:4]]
    now = DateTime()
    out = [
        {"Subject": "alpha"},
        {"Subject": ["alpha", "beta"]},
        {"Subject": {"query": ["alpha", "beta"], "operator": "and"}},
        {"Subject": {"query": ["alpha", "gamma"], "not": "beta"}},
        {"Subject": {"not": "delta"}, "portal_type": "Document"},
        {"Subject": {"query": ["beta", "delta"], "range": "min:max"}},
        {"modified": {"query": [days(-30), days(-5)], "range": "min:max"}},
        {"modified": {"query": days(-20), "range": "min"}},
        {"effective": {"query": days(0), "range": "max"}},
        {"portal_type": "News Item", "sort_on": "modified", "sort_limit": 5},
        {
            "portal_type": "Document",
            "sort_on": "modified",
            "sort_order": "reverse",
            "sort_limit": 5,
        },
        {"portal_type": "Document", "sort_on": "effective", "sort_limit": 6},
        {
            "portal_type": ["Document", "News Item"],
            "sort_on": "effective",
            "sort_order": "descending",
            "sort_limit": 6,
        },
        {"is_folderish": False, "portal_type": "Document"},
        {"is_folderish": True},
        {"UID": uids[0]},
        {"UID": uids},
        {"portal_type": "News Item", "sort_on": "UID", "sort_limit": 4},
    ]
    # effectiveRange at five dates around the dates of the content.
    for n in (-40, -7, 0, 10, 30):
        out.append(
            {"effectiveRange": days(n), "portal_type": ["Document", "News Item"]}
        )
    out.append({"effectiveRange": now, "review_state": "published"})
    return out


def five_indexes():
    """The five-index intersection of a search of Plone, which
    `searchResults` completes with `allowedRolesAndUsers` and
    `effectiveRange`."""
    return {
        "portal_type": ["Document", "News Item"],
        "review_state": ["published", "pending"],
        "Subject": ["alpha", "beta", "gamma"],
        "sort_on": "modified",
        "sort_order": "reverse",
        "sort_limit": 10,
    }


def stock_search(site, query):
    """The answer of the stock search of `searchResults` of the user."""
    from Products.ZCatalog.Catalog import Catalog

    with mock.patch.object(Catalog, "search", catalog._installed["stock"]):
        return answer(site.portal_catalog.searchResults(**query))


class TestTheIndexesOfTheCatalogMirror(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        subscribe(self)
        switch_on(self)
        with opened(self.db) as site:
            if "idx-0" not in site.objectIds():
                make_content(site)
                site.acl_users.userFolderAddUser(MEMBER, "secret-pw", ["Member"], [])
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

    def check(self, site, searches):
        """Each search gives the brains of the Python path, with the same
        `len()` and `actual_result_count`, natively."""
        # The changes of the content search the catalog in their own
        # transactions, on the Python path (`transaction`).
        native, python = self.counts()
        for query in searches:
            expected = stock(site, query)
            got = answer(site.portal_catalog.unrestrictedSearchResults(**query))
            self.assertEqual(got, expected, query)
        self.assertEqual(self.counts(), (native + len(searches), python))

    def check_as_users(self, site):
        """The five-index intersection through `searchResults`, as a member
        and as anonymous."""
        native, python = self.counts()
        found = []
        for user in (MEMBER, None):
            if user:
                zope_testing.login(site.acl_users, user)
            else:
                zope_testing.logout()
            expected = stock_search(site, five_indexes())
            got = answer(site.portal_catalog.searchResults(**five_indexes()))
            self.assertEqual(got, expected, user)
            found.append(expected[2])
        zope_testing.login(site.getPhysicalRoot().acl_users, SITE_OWNER_NAME)
        self.assertEqual(self.counts(), (native + 2, python))
        return found

    def test_each_option_of_each_index_type_gives_the_brains_of_the_python_path(self):
        with opened(self.db) as site:
            searches = queries(site)
            self.check(site, searches)
            # The searches find content.
            self.assertTrue(all(stock(site, q)[2] for q in searches[:6]))

    def test_the_five_index_intersection_as_a_member_and_as_anonymous(self):
        with opened(self.db) as site:
            member, anonymous = self.check_as_users(site)
            self.assertTrue(anonymous)
            self.assertGreaterEqual(member, anonymous)

    def test_the_lookups_of_plone_app_uuid_are_native(self):
        from plone.app.uuid.utils import uuidToCatalogBrain

        with opened(self.db) as site:
            some = items(site)[5:9]
            native = catalog._counts_now()["queries"].get("native", 0)
            brain = uuidToCatalogBrain(IUUID(some[0]))
            self.assertEqual(brain.getPath(), "/".join(some[0].getPhysicalPath()))
            query = {"UID": [IUUID(obj) for obj in some]}
            expected = stock(site, query)
            self.assertEqual(
                answer(site.portal_catalog.unrestrictedSearchResults(**query)),
                expected,
            )
            self.assertEqual(len(expected[0]), 4)
            self.assertEqual(
                catalog._counts_now()["queries"].get("native", 0), native + 2
            )

    def test_edits_transitions_and_deletes_then_the_same_searches(self):
        with opened(self.db) as site:
            obj = items(site)[1]
            obj.setSubject(("gamma", "delta"))
            obj.setModificationDate(days(-1))
            obj.reindexObject(idxs=["Subject", "modified"])
            transaction.commit()
        with opened(self.db) as site:
            obj = items(site)[0]
            site.portal_workflow.doActionFor(obj, "submit")
            site.portal_workflow.doActionFor(obj, "publish")
            obj.setEffectiveDate(days(-2))
            obj.setExpirationDate(days(3))
            obj.reindexObject(idxs=["effective", "expires", "effectiveRange"])
            transaction.commit()
        with opened(self.db) as site:
            site.manage_delObjects([items(site)[-1].getId()])
            transaction.commit()
        with opened(self.db) as site:
            self.check(site, queries(site))
            self.check_as_users(site)
