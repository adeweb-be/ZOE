"""Tests of the security backend of `zoe.plone` (ADR 0061) next to the
acceptance tests of `test_security_index`, on the same synthetic sites:

- the mirror and the Python path give the same rids for each token that
  an entry can hold, so a token that only one of them holds fails;
- the Indexer of queued indexing applies a folder that turns opaque before
  the older edit of an entry below it;
- `install()` puts the index into a site that exists, through the thread
  and the lease, and takes no lease when the catalog holds the index;
- a connection with a change of its transaction resolves without the
  cache;
- the index answers through the `Indexes` of the catalog tool, as the ZMI
  wraps it;
- with `security` in `ZOE_PLONE_BACKENDS`, the site of the fixture of
  plone.app.testing holds the index.

The layers of plone.app.testing need the interpreter of a Plone instance:
`just plone zoe backends` runs these tests.
"""

import random
import time
import unittest
from unittest import mock

import transaction
from Acquisition import aq_base, aq_inner, aq_parent
from plone.app.testing import PLONE_INTEGRATION_TESTING
from zope.component import getGlobalSiteManager
from zope.event import notify
from zope.interface import alsoProvides
from zope.processlifetime import DatabaseOpenedWithRoot

import zoe.kv
from zoe.plone import catalog, indexing, security
from zoe.plone import testing as zoe_testing
from zoe.plone.tests import test_security_index as acceptance

INDEX = acceptance.INDEX
MEMBER = acceptance.MEMBER
opened = acceptance.opened
# The longest wait for a thread of the backend, in seconds.
JOIN = 20


def make_site(test):
    """Make the site of the acceptance tests, with the catalog backend and
    the security backend installed first."""
    test.db = acceptance.new_db(test)
    test.storage = test.db.storage
    acceptance.subscribe(test)
    acceptance.switch_on(test)
    with opened(test.db, ()) as app:
        site = acceptance.add_site(app)
        acceptance.make_content(site)
        transaction.commit()
    notify(DatabaseOpenedWithRoot(test.db))
    security._join()


def register_provider(test):
    """Register the local role provider of the acceptance tests until the
    end of the test."""
    registry = getGlobalSiteManager()
    args = (acceptance.Provider, (acceptance.IProvided,), acceptance.ILocalRoleProvider)
    registry.registerAdapter(*args, "tests")
    test.addCleanup(registry.unregisterAdapter, *args, "tests")


def local_principals(obj):
    """Return the principals with a local role on `obj` and on each
    container above it."""
    found = set()
    while obj is not None:
        roles = getattr(aq_base(obj), "__ac_local_roles__", None)
        if isinstance(roles, dict):
            found.update(roles)
        obj = aq_parent(aq_inner(obj))
    return found


def candidate_tokens(site):
    """Return each token that an entry of `site` can hold: each role, and
    `user:` with each user, each group and each principal that holds a
    local role on an entry or on a container above it."""
    tokens = set(site.valid_roles()) | {"Anonymous", "Authenticated"}
    principals = {"nobody", acceptance.SITE_OWNER_NAME}
    principals.update(site.acl_users.source_users.getUserIds())
    principals.update(site.acl_users.source_groups.getGroupIds())
    for brain in site.portal_catalog.unrestrictedSearchResults():
        principals |= local_principals(brain._unrestrictedGetObject())
    tokens.update("user:" + principal for principal in principals)
    return tokens


class TokenChecks(acceptance.Checks):
    """The check of each token, on the mirror and on the Python path."""

    def check_tokens(self, db, storage, path=("plone",), what=""):
        """For each token that an entry can hold, the mirror gives the rids
        whose value on the Python path holds the token, and no other."""
        with opened(db, path) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            resolver = index.resolver()
            python = {}
            for rid in index._inputs.keys():
                for token in resolver.value(rid):
                    python.setdefault(token, set()).add(rid)
            tokens = sorted(set(python) | candidate_tokens(site))
            acceptance.wait_applied(storage, aq_base(pc)._catalog._p_oid)
            catalog._counts.clear()
            for token in tokens:
                found = pc.unrestrictedSearchResults(**{INDEX: [token]})
                native = {brain.getRID() for brain in found}
                self.assertEqual(
                    native, python.get(token, set()), f"{what}: the token {token}"
                )
            counts = catalog._counts_now()["queries"]
            self.assertEqual(counts.get("native", 0), len(tokens), what)
            self.assertNotIn("python", counts, what)


class TestTheMirrorAndThePythonPathTokenByToken(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    change = acceptance.TestTheSecurityIndex.change
    folders = acceptance.TestTheSecurityIndex.folders

    def setUp(self):
        make_site(self)

    def test_each_token_gives_the_same_rids_after_each_change(self):
        self.check_tokens(self.db, self.storage, what="after the install")
        rng = random.Random(acceptance.SEED + 1)
        for step in range(12):
            with opened(self.db) as site:
                what = self.change(site, rng, step)
                transaction.commit()
            self.check_tokens(self.db, self.storage, what=f"step {step}: {what}")

    def test_each_token_gives_the_same_rids_below_an_opaque_folder(self):
        register_provider(self)
        with opened(self.db) as site:
            alsoProvides(site["private"], acceptance.IProvided)
            site["private"].reindexObjectSecurity()
            transaction.commit()
        self.check_tokens(self.db, self.storage, what="a provider")


class TestQueuedIndexingAndAFolderThatTurnsOpaque(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)
        indexing.install()
        self.addCleanup(indexing.uninstall)

    def test_an_older_edit_below_the_folder_gets_the_value_of_the_indexer(self):
        register_provider(self)
        with opened(self.db) as site:
            # The edit of the entry below waits in the queue.
            secret = site["private"]["secret"]
            secret.setTitle("Edited")
            secret.reindexObject()
            transaction.commit()
        with opened(self.db) as site:
            alsoProvides(site["private"], acceptance.IProvided)
            site["private"].reindexObjectSecurity()
            transaction.commit()
        with opened(self.db) as site:
            indexing.Indexer(site).run()
        with opened(self.db) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            rid = pc._catalog.uids["/plone/private/secret"]
            self.assertIn(rid, index._opaque)
            self.assertIn("Manager", index.getEntryForObject(rid))
        self.check(self.db, storage=self.storage, what="one run of the queue")
        self.check_tokens(self.db, self.storage, what="one run of the queue")


class TestTheCacheOfTheSnapshots(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)

    def test_a_connection_with_changes_resolves_without_the_cache(self):
        from Products.CMFCore.indexing import processQueue

        token = "user:" + MEMBER
        with opened(self.db) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            rid = pc._catalog.uids["/plone/public/doc-0"]
            before = acceptance.stock_search(site, {INDEX: [token]})
            self.assertNotIn(rid, before)
            # The search filled the cache of the snapshot.
            self.assertIn(rid, security._snapshot_values(index))
            site["public"].manage_setLocalRoles(MEMBER, ["Reader"])
            site["public"].reindexObjectSecurity()
            processQueue()
            self.assertIsNone(security._snapshot_values(index))
            self.assertIn(rid, acceptance.stock_search(site, {INDEX: [token]}))
        # The abort leaves the cache of the snapshot as it was.
        with opened(self.db) as site:
            self.assertEqual(acceptance.stock_search(site, {INDEX: [token]}), before)

    def test_the_cache_keeps_the_two_newest_snapshots(self):
        with security._cache_lock:
            security._cache.clear()
        # A connection of its own keeps its snapshot while the others
        # commit.
        manager = transaction.TransactionManager()
        connection = self.db.open(transaction_manager=manager)
        self.addCleanup(connection.close)
        self.addCleanup(manager.abort)
        old = connection.root()["Application"]["plone"]
        index = security.security_index(old.portal_catalog)
        self.assertIsNotNone(security._snapshot_values(index))
        starts = []
        for name in ("one", "two"):
            with opened(self.db) as site:
                site.manage_setLocalRoles(name, ["Reader"])
                transaction.commit()
            with opened(self.db) as site:
                # The old snapshot is in use again: it stays the oldest.
                self.assertIsNotNone(security._snapshot_values(index))
                newer = security.security_index(site.portal_catalog)
                self.assertIsNotNone(security._snapshot_values(newer))
                starts.append(site._p_jar._storage._start)
        # The snapshot of the old connection is older than the two of the
        # cache, so it resolves without the cache.
        self.assertIsNone(security._snapshot_values(index))
        with security._cache_lock:
            kept = sorted(key[2] for key in security._cache)
        self.assertEqual(kept, sorted(starts))


class TestInstallOnASiteThatExists(acceptance.Checks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def catalog_oid(self):
        with opened(self.db) as site:
            return aq_base(site.portal_catalog)._p_oid

    def set_lease(self, value):
        """Write the lease of queued indexing of the catalog, or delete it
        when `value` is None."""
        with opened(self.db) as site:
            lease = zoe.kv.Namespace(site._p_jar, "ephemeral", indexing.LEASE)
            if value is None:
                lease.delete(self.oid)
            else:
                lease.set(self.oid, value, ttl=120)
            transaction.commit()

    def get_lease(self):
        with opened(self.db) as site:
            return zoe.kv.Namespace(site._p_jar, "ephemeral", indexing.LEASE).get(
                self.oid
            )

    def test_install_puts_the_index_in_through_the_thread_and_the_lease(self):
        self.db = acceptance.new_db(self)
        self.storage = self.db.storage
        acceptance.subscribe(self)
        with mock.patch.dict("os.environ", {"ZOE_PLONE_CATALOG_MODE": "on"}):
            catalog.install()
        self.addCleanup(catalog._root.clear)
        self.addCleanup(catalog.uninstall)
        with opened(self.db, ()) as app:
            site = acceptance.add_site(app)
            acceptance.make_content(site)
            transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        self.oid = self.catalog_oid()
        with opened(self.db) as site:
            self.assertIsNone(security.security_index(site.portal_catalog))
        # A queued indexer holds the lease of the site.
        self.set_lease(b"another indexer")
        security.install()
        self.addCleanup(security._root.clear)
        self.addCleanup(security.uninstall)
        self.addCleanup(self.set_lease, None)
        time.sleep(1.5)
        with opened(self.db) as site:
            # The thread waits for the lease.
            self.assertIsNone(security.security_index(site.portal_catalog))
        self.assertTrue(any(t.is_alive() for t in security._threads))
        self.set_lease(None)
        security._join(JOIN)
        self.assertFalse(any(t.is_alive() for t in security._threads))
        with opened(self.db) as site:
            self.assertIsNotNone(security.security_index(site.portal_catalog))
        # The thread gave the lease back.
        self.assertIsNone(self.get_lease())
        self.check(self.db, storage=self.storage, what="install on a site")
        # At the next open of the database, the catalog holds the index:
        # the thread takes no lease, so a held lease does not stop it.
        self.set_lease(b"another indexer")
        began = time.monotonic()
        notify(DatabaseOpenedWithRoot(self.db))
        security._join(JOIN)
        self.assertFalse(any(t.is_alive() for t in security._threads))
        self.assertLess(time.monotonic() - began, JOIN)
        self.assertEqual(self.get_lease(), b"another indexer")


class TestTheIndexInTheZMI(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)

    def test_the_index_answers_through_the_indexes_of_the_tool(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            wrapped = pc.unrestrictedTraverse(f"Indexes/{INDEX}")
            self.assertIsInstance(aq_base(wrapped), security.ZoeSecurityIndex)
            self.assertEqual(
                [(token, list(rids)) for token, rids in wrapped.items()],
                [(token, list(rids)) for token, rids in index.items()],
            )
            self.assertEqual(list(wrapped.uniqueValues()), list(index.uniqueValues()))
            self.assertEqual(wrapped.documentToKeyMap(), index.documentToKeyMap())
            # The browse page of the ZMI.
            html = wrapped.manage_browse(wrapped, site.REQUEST)
            self.assertIn("distinct values", html)


class TestTheFixtureOfTheSuites(unittest.TestCase):
    """With `security` in `ZOE_PLONE_BACKENDS`, the suites of Plone run on
    a site that holds the index. `just plone zoe suites` with `-s
    zoe.plone -t TestTheFixtureOfTheSuites` checks it."""

    layer = PLONE_INTEGRATION_TESTING

    def test_the_site_of_the_fixture_holds_the_index(self):
        if not security.installed():
            self.skipTest("ZOE_PLONE_BACKENDS does not name security")
        portal = self.layer["portal"]
        index = security.security_index(portal.portal_catalog)
        self.assertIsNotNone(index)
        self.assertEqual(len(index._inputs), len(portal.portal_catalog._catalog.uids))
        print(
            f"\nthe site of the fixture holds the {aq_base(index).meta_type}"
            f" with {len(index._inputs)} entries"
        )
