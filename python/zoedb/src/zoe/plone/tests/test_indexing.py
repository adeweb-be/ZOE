"""Integration tests of queued indexing on a Plone site (ADR 0047).

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, whose base storage is a `zoe.Storage`, as
`just plone zoe backends` runs them. Each test installs the backend and
uninstalls it at its end.
"""

import json
import logging
import threading
import time
import unittest
from unittest import mock

import transaction
import Zope2
from Acquisition import aq_base
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    PLONE_SITE_ID,
    TEST_USER_ID,
    TEST_USER_NAME,
    login,
    logout,
    setRoles,
)
from Products.CMFCore.CatalogTool import CatalogTool
from Products.CMFCore.indexing import PortalCatalogProcessor
from Products.CMFCore.interfaces import IPortalCatalogQueueProcessor
from Testing.makerequest import makerequest
from zope.component import getUtility
from zope.component.hooks import setSite
from zope.event import notify
from zope.globalrequest import clearRequest, getRequest, setRequest
from zope.lifecycleevent import ObjectModifiedEvent

import zoe.kv
from zoe.plone import indexing

# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60


def edit(document, title):
    """Save `document` with the title `title`, as the edit form does."""
    document.title = title
    notify(ObjectModifiedEvent(document))


def path_of(obj):
    return "/".join(obj.getPhysicalPath())


def queue_of(portal):
    """Return the queue of the catalog of `portal`."""
    catalog = aq_base(portal.portal_catalog)
    return zoe.kv.Namespace(
        portal._p_jar, "durable", indexing.queue_name(catalog._p_oid)
    )


def drain(portal, **kwargs):
    try:
        return indexing.Indexer(portal, **kwargs).run()
    finally:
        transaction.begin()


def in_thread(db, work):
    """Run `work(site)` in a thread of its own, as a request of the test
    user, and commit. Return the error text of the thread, or None."""
    errors = []

    def run():
        connection = db.open()
        try:
            app = makerequest(connection.root()["Application"])
            setRequest(app.REQUEST)
            site = app[PLONE_SITE_ID]
            setSite(site)
            login(site, TEST_USER_NAME)
            work(site)
            transaction.commit()
        except BaseException as error:
            errors.append(f"{type(error).__name__}: {error}")
        finally:
            transaction.abort()
            logout()
            setSite(None)
            clearRequest()
            connection.close()

    thread = threading.Thread(target=run)
    thread.start()
    thread.join(TIMEOUT)
    assert not thread.is_alive()
    return errors[0] if errors else None


class IndexingTestCase(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        self.portal = self.layer["portal"]
        setRoles(self.portal, TEST_USER_ID, ["Manager"])
        setRequest(self.layer["request"])
        self.addCleanup(clearRequest)
        indexing.install()
        self.addCleanup(indexing.uninstall)
        self.tokens = iter(range(10_000))

    def token(self):
        return f"qitoken{next(self.tokens)}x"

    def search(self, **query):
        brains = self.portal.portal_catalog.unrestrictedSearchResults(**query)
        return sorted(brain.getPath() for brain in brains)


class TestProcessor(IndexingTestCase):
    def test_install_puts_the_processor_in_the_place_of_portal_catalog(self):
        utility = getUtility(IPortalCatalogQueueProcessor, name=indexing.UTILITY)
        self.assertIsInstance(utility, indexing.QueueProcessor)
        indexing.install()
        self.assertIs(
            getUtility(IPortalCatalogQueueProcessor, name=indexing.UTILITY), utility
        )
        indexing.uninstall()
        self.assertFalse(indexing.installed())
        utility = getUtility(IPortalCatalogQueueProcessor, name=indexing.UTILITY)
        self.assertIsInstance(utility, PortalCatalogProcessor)
        # After the uninstall, a save writes the catalog again.
        self.portal.invokeFactory("Document", "doc", title=f"Stock {self.token()}")
        word = self.token()
        edit(self.portal["doc"], f"Stock {word}")
        transaction.commit()
        self.assertEqual(
            self.search(SearchableText=word), [path_of(self.portal["doc"])]
        )
        self.assertEqual(queue_of(self.portal).scan(), [])
        indexing.install()

    def test_a_save_writes_one_key_for_each_operation(self):
        self.portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        drain(self.portal)
        before = time.time()
        edit(self.portal["doc"], f"Edited {self.token()}")
        transaction.commit()
        pairs = queue_of(self.portal).scan()
        self.assertEqual(len(pairs), 1)
        key, value = pairs[0]
        self.assertEqual(len(key), 20)
        self.assertGreaterEqual(indexing.key_time(key), before - 1)
        self.assertLessEqual(indexing.key_time(key), time.time() + 1)
        self.assertEqual(
            indexing.decode(value),
            {
                "op": "reindex",
                "path": path_of(self.portal["doc"]),
                "idxs": [],
                "metadata": True,
            },
        )

    def test_an_abort_drops_the_keys(self):
        self.portal.invokeFactory("Document", "doc", title="Document")
        transaction.abort()
        self.assertEqual(queue_of(self.portal).scan(), [])

    def test_a_storage_without_namespaces_keeps_the_stock_processor(self):
        word = self.token()
        with (
            mock.patch.object(zoe.kv, "_supported", return_value=False),
            self.assertLogs("zoe.plone.indexing", logging.WARNING) as logs,
        ):
            indexing._warned.clear()
            self.portal.invokeFactory("Document", "doc", title=f"Stock {word}")
            transaction.commit()
        self.assertIn("has no KV namespaces", logs.output[0])
        self.assertEqual(
            self.search(SearchableText=word), [path_of(self.portal["doc"])]
        )
        self.assertEqual(queue_of(self.portal).scan(), [])

    def test_a_save_sets_the_cookie_of_its_user(self):
        request = getRequest()
        self.portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        cookie = request.response.cookies[indexing.COOKIE]
        oid, key, path = indexing._parse_cookie(cookie["value"])
        self.assertEqual(oid, aq_base(self.portal.portal_catalog)._p_oid)
        self.assertEqual(key, queue_of(self.portal).scan()[-1][0])
        self.assertEqual(path, self.portal.absolute_url_path())
        self.assertTrue(cookie["HttpOnly"])
        self.assertEqual(cookie["SameSite"], "Lax")


class TestIndexer(IndexingTestCase):
    def test_a_batch_takes_at_most_batch_size_keys(self):
        for n in range(3):
            self.portal.invokeFactory("Document", f"doc-{n}", title=f"D {n}")
            transaction.commit()
        total = len(queue_of(self.portal).scan())
        batches = []
        indexer = indexing.Indexer(self.portal, batch_size=2)
        indexer.after_batch = lambda indexer, keys: batches.append(len(keys))
        self.assertEqual(indexer.run(), total)
        transaction.begin()
        self.assertTrue(all(size <= 2 for size in batches))
        self.assertEqual(sum(batches), total)
        self.assertEqual(queue_of(self.portal).scan(), [])

    def test_the_batch_size_is_an_int_of_one_or_more(self):
        for size in (0, -1, 1.5, "2"):
            with self.assertRaises(ValueError):
                indexing.Indexer(self.portal, batch_size=size)

    def test_a_security_change_reaches_a_document_that_the_queue_holds(self):
        # Documents have no workflow, so a Document takes the View of its
        # folder; the folder is published, so anonymous can View it.
        portal = self.portal
        workflow = portal.portal_workflow
        workflow.setChainForPortalTypes(("Document",), ())
        workflow.setChainForPortalTypes(("Folder",), ("simple_publication_workflow",))
        portal.invokeFactory("Folder", "folder", title="Folder")
        workflow.doActionFor(portal["folder"], "publish")
        transaction.commit()
        drain(portal)
        word = self.token()
        portal["folder"].invokeFactory("Document", "doc", title=f"Added {word}")
        transaction.commit()
        db = portal._p_jar.db()
        added = len(queue_of(portal).scan())
        retracted = []

        def retract(site):
            site.portal_workflow.doActionFor(site["folder"], "retract")

        class Window(indexing.Indexer):
            # The folder becomes private after the first batch took its
            # snapshot: the batch indexes the Document as a Document of
            # a published folder.
            def _begin(self, tm, connection):
                super()._begin(tm, connection)
                if not retracted:
                    retracted.append(in_thread(db, retract))

        indexer = Window(portal, batch_size=added)
        indexer.run()
        transaction.begin()
        self.assertEqual(retracted, [None])
        doc = path_of(portal["folder"]["doc"])
        self.assertEqual(self.search(SearchableText=word), [doc])
        logout()
        brains = portal.portal_catalog.searchResults(SearchableText=word)
        self.assertEqual([brain.getPath() for brain in brains], [])
        login(portal, TEST_USER_NAME)
        self.assertEqual(queue_of(portal).scan(), [])

    def test_two_moves_whose_keys_come_in_reverse_order(self):
        portal = self.portal
        portal.invokeFactory("Folder", "a", title="A")
        portal.invokeFactory("Folder", "b", title="B")
        portal.invokeFactory("Document", "doc", title="Moved")
        transaction.commit()
        drain(portal)
        uid = portal["doc"].UID()
        queue = queue_of(portal)
        portal["a"].manage_pasteObjects(portal.manage_cutObjects(["doc"]))
        transaction.commit()
        first = queue.scan()
        portal["b"].manage_pasteObjects(portal["a"].manage_cutObjects(["doc"]))
        transaction.commit()
        second = [pair for pair in queue.scan() if pair not in first]
        self.assertTrue(first and second)
        # The clock of the process of the second move was behind: its keys
        # come before the keys of the first move.
        earliest = min(indexing.key_time(key) for key, _ in first)
        for number, (key, value) in enumerate(second):
            queue.delete(key)
            micros = int((earliest - 1) * 1_000_000)
            queue.set(indexing.make_key(micros, b"\x00" * 8, number), value)
        transaction.commit()
        drain(portal, batch_size=len(second))
        moved = path_of(portal["b"]["doc"])
        self.assertEqual(self.search(UID=uid), [moved])
        uids = portal.portal_catalog._catalog.uids
        self.assertIn(moved, uids)
        self.assertNotIn("/".join((*portal.getPhysicalPath(), "doc")), uids)
        self.assertNotIn(path_of(portal["a"]) + "/doc", uids)

    def test_a_failing_path_leaves_the_queue_after_five_failures(self):
        portal = self.portal
        portal.invokeFactory("Document", "bad", title="Bad")
        portal.invokeFactory("Document", "good", title="Good")
        transaction.commit()
        drain(portal)
        bad = path_of(portal["bad"])
        edit(portal["bad"], f"Bad {self.token()}")
        word = self.token()
        edit(portal["good"], f"Good {word}")
        transaction.commit()
        stock = CatalogTool._reindexObject

        def failing(catalog, obj, *args, **kwargs):
            if path_of(obj) == bad:
                raise ValueError("an indexer of the test fails")
            return stock(catalog, obj, *args, **kwargs)

        catalog = aq_base(portal.portal_catalog)
        failed = zoe.kv.Namespace(
            portal._p_jar, "durable", indexing.failed_name(catalog._p_oid)
        )
        with mock.patch.object(CatalogTool, "_reindexObject", failing):
            for run in range(1, indexing.FAILURES + 1):
                with self.assertLogs("zoe.plone.indexing", logging.WARNING):
                    drain(portal)
                # The other path reached the catalog in the first run.
                self.assertEqual(
                    self.search(SearchableText=word), [path_of(portal["good"])]
                )
                pairs = queue_of(portal).scan()
                if run < indexing.FAILURES:
                    (value,) = [indexing.decode(value) for _, value in pairs]
                    self.assertEqual(value["failures"], run)
                    self.assertIn("an indexer of the test fails", value["error"])
                else:
                    self.assertEqual(pairs, [])
        (value,) = [indexing.decode(value) for _, value in failed.scan()]
        self.assertEqual(value["path"], bad)
        self.assertEqual(value["failures"], indexing.FAILURES)
        self.assertEqual(indexing.status(portal)["failed"], 1)

    def test_a_conflict_retries_the_batch(self):
        portal = self.portal
        portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        drain(portal)
        word = self.token()
        edit(portal["doc"], f"Edited {word}")
        transaction.commit()
        calls = []

        class Conflicting(indexing.Indexer):
            def _begin(self, tm, connection):
                super()._begin(tm, connection)
                if not calls:
                    calls.append(1)
                    from ZODB.POSException import ConflictError

                    raise ConflictError("a conflict of the test")

        with mock.patch.object(indexing, "PAUSES", (0.0,) * 4):
            Conflicting(portal).run()
        transaction.begin()
        self.assertEqual(calls, [1])
        self.assertEqual(self.search(SearchableText=word), [path_of(portal["doc"])])


class TestStatus(IndexingTestCase):
    def test_the_status_counts_the_queue_and_the_delays(self):
        portal = self.portal
        portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        state = indexing.status(portal)
        self.assertTrue(state["installed"])
        self.assertTrue(state["namespaces"])
        self.assertGreater(state["pending"], 0)
        self.assertGreaterEqual(state["oldest_age"], 0)
        self.assertEqual(state["failed"], 0)
        self.assertFalse(state["lease"])
        count = state["delays"]["count"]
        drain(portal)
        state = indexing.status(portal)
        self.assertEqual(state["pending"], 0)
        self.assertIsNone(state["oldest_age"])
        self.assertGreater(state["delays"]["count"], count)
        self.assertGreaterEqual(state["delays"]["max"], state["delays"]["p50"])
        # With `since`, only the batches that committed after it count.
        later = indexing.status(portal, since=time.time() + 60)
        self.assertEqual(later["delays"]["count"], 0)

    def test_the_view_gives_the_status_as_json(self):
        request = getRequest()
        body = indexing.StatusView(self.portal, request)()
        self.assertEqual(json.loads(body)["pending"], 0)
        self.assertEqual(request.response.getHeader("Content-Type"), "application/json")
        request.form["since"] = str(time.time() + 60)
        body = indexing.StatusView(self.portal, request)()
        self.assertEqual(json.loads(body)["delays"]["count"], 0)


class _Event:
    def __init__(self, request):
        self.request = request


class TestWait(IndexingTestCase):
    def setUp(self):
        super().setUp()
        self.portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        drain(self.portal)
        db = self.portal._p_jar.db()
        patcher = mock.patch.object(Zope2, "DB", db, create=True)
        patcher.start()
        self.addCleanup(patcher.stop)

    def save(self):
        """Save the Document; return the cookie that the save set."""
        request = getRequest()
        edit(self.portal["doc"], f"Edited {self.token()}")
        transaction.commit()
        return request.response.cookies[indexing.COOKIE]["value"]

    def later_request(self, value):
        request = makerequest(aq_base(self.layer["app"])).REQUEST
        request.cookies[indexing.COOKIE] = value
        return request

    def test_a_request_waits_until_the_indexer_applied_its_save(self):
        request = self.later_request(self.save())
        portal = self.portal
        indexer = indexing.Indexer(portal)

        def later():
            time.sleep(0.3)
            indexer.run()

        thread = threading.Thread(target=later)
        start = time.monotonic()
        thread.start()
        indexing.request_started(_Event(request))
        waited = time.monotonic() - start
        thread.join(TIMEOUT)
        self.assertGreaterEqual(waited, 0.25)
        self.assertLess(waited, indexing.WAIT)
        transaction.begin()
        self.assertEqual(queue_of(portal).scan(), [])
        # The request expired the cookie.
        self.assertEqual(request.response.cookies[indexing.COOKIE]["value"], "deleted")

    def test_a_request_waits_at_most_wait_seconds(self):
        request = self.later_request(self.save())
        with (
            mock.patch.object(indexing, "WAIT", 0.2),
            self.assertLogs("zoe.plone.indexing", logging.WARNING),
        ):
            start = time.monotonic()
            indexing.request_started(_Event(request))
            waited = time.monotonic() - start
        self.assertGreaterEqual(waited, 0.2)
        self.assertLess(waited, 1.5)
        self.assertEqual(request.response.cookies[indexing.COOKIE]["value"], "deleted")

    def test_a_request_without_the_cookie_does_not_wait(self):
        self.save()
        request = makerequest(aq_base(self.layer["app"])).REQUEST
        start = time.monotonic()
        indexing.request_started(_Event(request))
        self.assertLess(time.monotonic() - start, 0.1)
        self.assertNotIn(indexing.COOKIE, request.response.cookies)

    def test_a_bad_cookie_is_expired(self):
        request = self.later_request("not-a-cookie")
        indexing.request_started(_Event(request))
        self.assertEqual(request.response.cookies[indexing.COOKIE]["value"], "deleted")


class TestService(IndexingTestCase):
    def test_the_lease_has_one_holder(self):
        db = self.portal._p_jar.db()
        oid = aq_base(self.portal.portal_catalog)._p_oid
        first = indexing._Service(db)
        second = indexing._Service(db)
        tm = transaction.TransactionManager()
        connection = db.open(transaction_manager=tm)
        try:
            self.assertTrue(first._take(connection, tm, oid))
            self.assertTrue(first._take(connection, tm, oid))
            self.assertFalse(second._take(connection, tm, oid))
            first.stop()
            self.assertTrue(second._take(connection, tm, oid))
        finally:
            tm.abort()
            connection.close()
            second.stop()

    def test_the_thread_applies_the_queue_of_the_site(self):
        portal = self.portal
        db = portal._p_jar.db()
        portal.invokeFactory("Document", "doc", title="Document")
        transaction.commit()
        service = indexing._Service(db)
        indexing._services[id(db)] = service
        self.addCleanup(indexing._services.pop, id(db), None)
        self.addCleanup(service.stop)
        service.start()
        word = self.token()
        edit(portal["doc"], f"Edited {word}")
        transaction.commit()
        deadline = time.monotonic() + TIMEOUT
        while queue_of(portal).scan(limit=1) and time.monotonic() < deadline:
            time.sleep(0.05)
        transaction.begin()
        self.assertEqual(queue_of(portal).scan(), [])
        self.assertEqual(self.search(SearchableText=word), [path_of(portal["doc"])])
        self.assertTrue(indexing.status(portal)["lease"])
        service.stop()
        service.join(TIMEOUT)
        self.assertFalse(service.is_alive())
        transaction.begin()
        self.assertFalse(indexing.status(portal)["lease"])
