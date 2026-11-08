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
- an entry that leaves the catalog leaves its inputs in the chains of the
  entries below it, so their values stay those of the stock index;
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
# The entries below the folder of the large delete.
LARGE = 5000


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

    def check_tokens(self, db, storage, path=("plone",), what="", stock=False):
        """For each token that an entry can hold, the mirror gives the rids
        whose value on the Python path holds the token, and no other. With
        `stock`, the stock index, indexed again in full, gives them too."""
        if stock:
            self.index_stock(db, path)
        with opened(db, path) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            resolver = index.resolver()
            python = {}
            for rid in index._inputs.keys():
                for token in resolver.value(rid):
                    python.setdefault(token, set()).add(rid)
            tokens = set(python) | candidate_tokens(site)
            if stock:
                tokens.update(pc._catalog.getIndex(acceptance.STOCK).uniqueValues())
            tokens = sorted(tokens)
            acceptance.wait_applied(storage, aq_base(pc)._catalog._p_oid)
            catalog._counts.clear()
            for token in tokens:
                found = pc.unrestrictedSearchResults(**{INDEX: [token]})
                native = {brain.getRID() for brain in found}
                self.assertEqual(
                    native, python.get(token, set()), f"{what}: the token {token}"
                )
                if stock:
                    query = {acceptance.STOCK: [token]}
                    self.assertEqual(
                        native,
                        set(acceptance.stock_search(site, query)),
                        f"{what}: the stock index, {token}",
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


class TestAContainerThatLeavesTheCatalog(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)

    def checks(self, what):
        self.check(self.db, storage=self.storage, what=what)
        self.check_tokens(self.db, self.storage, what=what, stock=True)

    def test_the_entries_below_resolve_through_its_inputs_until_it_comes_back(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            pc.unindexObject(site["private"]["blocked"])
            pc.unindexObject(site["private"])
            transaction.commit()
        with opened(self.db) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            rid = pc._catalog.uids["/plone/private/blocked/below"]
            self.assertEqual(
                [item[0] for item in index._inputs[rid][4]],
                ["/plone/private/blocked", "/plone/private"],
            )
        self.checks("two containers out of the catalog")
        # A share on a container outside the catalog reindexes the entries
        # below it, which read the container again.
        with opened(self.db) as site:
            site["private"].manage_setLocalRoles(MEMBER, ["Editor"])
            site["private"].reindexObjectSecurity()
            transaction.commit()
        self.checks("a share on a container out of the catalog")
        with opened(self.db) as site:
            site["private"].reindexObject()
            site["private"]["blocked"].reindexObject()
            transaction.commit()
        self.checks("the containers back in the catalog")

    def test_the_indexer_writes_the_chains_once_at_the_end_of_its_batch(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            rid = pc._catalog.uids["/plone/private/secret"]
            before = index._inputs[rid]
            with security._deferred(index) as (removed, fresh):
                pc.uncatalog_object("/plone/private")
                # The chain waits for the end of the batch.
                self.assertEqual(index._inputs[rid], before)
            self.assertEqual([item[1] for item in removed], ["/plone/private"])
            index._write_chains(removed, fresh)
            self.assertEqual(index._inputs[rid][4][0][0], "/plone/private")
            transaction.commit()
        self.checks("a container out of the catalog in a batch")

    def test_a_delete_across_batches_of_the_queue(self):
        indexing.install()
        self.addCleanup(indexing.uninstall)
        with opened(self.db) as site:
            site.manage_delObjects(["private"])
            transaction.commit()
        with opened(self.db) as site:
            indexing.Indexer(site, batch_size=2).run()
        with opened(self.db) as site:
            found = site.portal_catalog.unrestrictedSearchResults(path="/plone/private")
            self.assertEqual(list(found), [])
        self.checks("a delete")


class TestTheQueueAndALargeDelete(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)
        indexing.install()
        self.addCleanup(indexing.uninstall)

    def test_a_delete_of_a_large_folder_writes_as_a_delete_in_one_transaction(self):
        from ZODB.Connection import Connection

        written = []
        commit = Connection.commit

        def counted(connection, txn):
            written.append(len(connection._registered_objects))
            return commit(connection, txn)

        def records(call):
            """Return the records that the commits of `call` write."""
            del written[:]
            with mock.patch.object(Connection, "commit", counted):
                call()
            return sum(written)

        with opened(self.db) as site:
            for name in ("large", "twin"):
                site.invokeFactory("Folder", name, title=name)
                folder = site[name]
                for i in range(LARGE // 50):
                    folder.invokeFactory("Folder", f"f{i}", title=f"f{i}")
                    sub = folder[f"f{i}"]
                    for j in range(49):
                        sub.invokeFactory("Document", f"d{j}", title=f"d{j}")
                    transaction.savepoint(optimistic=True)
            transaction.commit()
        with opened(self.db) as site:
            indexing.Indexer(site).run()
        with opened(self.db) as site:
            deleted = len(
                site.portal_catalog.unrestrictedSearchResults(path="/plone/large")
            )
        self.assertEqual(deleted, LARGE + 1)
        # An edit of the folder puts its path first in the queue: the first
        # batch finds the folder gone before the entries below it.
        with opened(self.db) as site:
            site["large"].setTitle("Large")
            site["large"].reindexObject()
            transaction.commit()

        # The stock processor deletes the twin in the transaction of the
        # delete: the records of one transaction.
        def delete_twin():
            with opened(self.db) as site:
                site.manage_delObjects(["twin"])
                transaction.commit()

        indexing.uninstall()
        try:
            stock = records(delete_twin)
        finally:
            indexing.install()
        with opened(self.db) as site:
            site.manage_delObjects(["large"])
            transaction.commit()

        left = []

        def after(indexer, keys):
            # The entries of the folder left after each batch.
            manager = transaction.TransactionManager()
            connection = self.db.open(transaction_manager=manager)
            try:
                pc = connection.root()["Application"]["plone"].portal_catalog
                paths = pc._catalog.uids.keys("/plone/large", "/plone/large/\uffff")
                left.append(
                    sum(
                        1
                        for path in paths
                        if path == "/plone/large" or path.startswith("/plone/large/")
                    )
                )
            finally:
                manager.abort()
                connection.close()

        def run():
            with opened(self.db) as site:
                indexer = indexing.Indexer(site, batch_size=100)
                indexer.after_batch = after
                indexer.run()

        queued = records(run)
        # The first batch holds the path of the folder: it finds the folder
        # gone and removes the whole subtree.
        self.assertEqual(left[0], 0, left)
        print(
            f"\na delete of {deleted} entries wrote {queued} records through"
            f" the queue, and {stock} in one transaction"
        )
        self.assertLess(queued, stock * 1.25)
        with opened(self.db) as site:
            for name in ("large", "twin"):
                found = site.portal_catalog.unrestrictedSearchResults(
                    path=f"/plone/{name}"
                )
                self.assertEqual(list(found), [])
        self.check(self.db, storage=self.storage, what="a large delete")


class TestTheQueueAndADeletedComment(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)
        indexing.install()
        self.addCleanup(indexing.uninstall)

    def comments(self):
        with opened(self.db) as site:
            found = site.portal_catalog.unrestrictedSearchResults(
                path="/plone/public/doc-0", portal_type="Discussion Item"
            )
            return sorted(brain.getPath() for brain in found)

    def test_the_other_comment_of_the_document_stays_in_the_catalog(self):
        from plone.app.discussion.interfaces import IConversation

        with opened(self.db) as site:
            acceptance.comment(site["public"]["doc-0"], "A second comment")
            transaction.commit()
        with opened(self.db) as site:
            indexing.Indexer(site).run()
        before = self.comments()
        self.assertEqual(len(before), 2, before)
        with opened(self.db) as site:
            conversation = IConversation(site["public"]["doc-0"])
            gone = before[0].rpartition("/")[2]
            del conversation[gone]
            transaction.commit()
        with opened(self.db) as site:
            indexing.Indexer(site).run()
        self.assertEqual(self.comments(), before[1:])
        self.check(self.db, storage=self.storage, what="a deleted comment")
        self.check_tokens(self.db, self.storage, what="a deleted comment", stock=True)


class TestASearchInATransactionThatChangedSecurity(TokenChecks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        make_site(self)

    def test_the_search_with_the_overlay_gives_the_stock_result(self):
        from Products.CMFCore.indexing import processQueue

        token = "user:" + MEMBER
        with opened(self.db) as site:
            pc = site.portal_catalog
            acceptance.wait_applied(self.storage, aq_base(pc)._catalog._p_oid)
            site["public"].manage_setLocalRoles(MEMBER, ["Reader"])
            site["public"].reindexObjectSecurity()
            processQueue()
            self.assertTrue(site._p_jar._registered_objects)
            # The stock index of the same transaction.
            pc.reindexIndex(acceptance.STOCK, None)
            expected = acceptance.stock_search(site, {acceptance.STOCK: [token]})
            rid = pc._catalog.uids["/plone/public/doc-0"]
            self.assertIn(rid, expected)
            catalog._counts.clear()
            found = pc.unrestrictedSearchResults(**{INDEX: [token]})
            self.assertEqual(sorted(brain.getRID() for brain in found), expected)
            counts = catalog._counts_now()["queries"]
            self.assertEqual(counts, {"native": 1}, catalog._counts_now())


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
        oid = aq_base(index)._p_oid

        def kept():
            with security._cache_lock:
                cache = security._cache[self.db]
                return sorted(start for each, start in cache if each == oid)

        self.assertEqual(kept(), sorted(starts))
        # The snapshots of another index have slots of their own. The
        # catalog tool stands for it: its oid is another key.
        with opened(self.db) as site:
            other = site.portal_catalog
            self.assertIsNotNone(security._snapshot_values(other))
            # The old connection has a slot for this other key too.
            self.assertIsNotNone(security._snapshot_values(old.portal_catalog))
        self.assertEqual(kept(), sorted(starts))


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
