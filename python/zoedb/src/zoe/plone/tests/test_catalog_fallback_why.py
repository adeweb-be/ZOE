"""Acceptance test of the cause of a fallback (`why`) in the counts and the
log of `zoe.plone.catalog` (F17; ADR 0059, section 11).

The test copies the site of `zoe.plone.testing.ZOE_SITE` to a store of its
own, adds Documents, and puts a ghost rid, a rid that `paths` does not
hold, in the reverse map of `portal_type`. The mirror builds, with
`portal_type` unavailable. A query on `portal_type` takes the Python path:
the counts hold the reason `index_unavailable` with the text of the reply,
which names `portal_type`, and the log holds one WARNING with that text. A
second such query adds to the count and logs nothing at WARNING.

A second class checks the log of a text that the repair does not mend:
the first text of a reason at WARNING, a later new text at DEBUG, a known
text not again, and the bound on the texts of a reason.

The layers of plone.app.testing need the interpreter of a Plone instance:
`just plone zoe backends -t test_catalog_fallback_why` runs this module.
"""

import collections
import contextlib
import logging
import time
import unittest
from unittest import mock

import transaction
from Acquisition import aq_base
from plone.app.testing import SITE_OWNER_NAME
from plone.testing import zope as zope_testing
from Testing.makerequest import makerequest
from ZODB.DB import DB
from zope.component.hooks import getSite, setSite

from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.tests.helpers import copied_store

# The longest wait for the mirror, in seconds.
TIMEOUT = 30

DOCUMENTS = [f"doc-{i}" for i in range(4)]


@contextlib.contextmanager
def opened(db):
    """Open a connection of `db` with the transaction manager of the thread,
    log in as the site owner and make the site the current site. Yield the
    site. At the end, abort the transaction."""
    old = getSite()
    connection = db.open()
    try:
        app = makerequest(connection.root()["Application"])
        zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
        site = app["plone"]
        setSite(site)
        yield site
    finally:
        transaction.abort()
        zope_testing.logout()
        setSite(old)
        connection.close()


class Ghost:
    """An object that was never in `paths`."""

    portal_type = "Document"


def wait_ready(storage, key):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        status = storage._catalog_status(key)
        if status is not None and status["state"] == "ready":
            return
        time.sleep(0.05)
    raise AssertionError(f"the mirror is not ready: {storage._catalog_status(key)}")


def isolated(test):
    """Give `test` empty counts, texts and hints of `zoe.plone.catalog`."""
    for name, value in (
        ("_counts", collections.Counter()),
        ("_whys", {}),
        ("_hinted", set()),
    ):
        patcher = mock.patch.object(catalog, name, value)
        patcher.start()
        test.addCleanup(patcher.stop)


def warnings_with(records, text):
    return [
        r for r in records if r.levelno >= logging.WARNING and text in r.getMessage()
    ]


class TestTheCauseOfAFallback(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        saved = dict(catalog._root)
        catalog._root.clear()
        self.addCleanup(catalog._root.update, saved)
        if not catalog.installed():
            with mock.patch.dict("os.environ", {"ZOE_PLONE_CATALOG_MODE": "on"}):
                catalog.install()
            self.addCleanup(catalog.uninstall)
        isolated(self)
        storage, _ = copied_store(self, self.layer["zoeStorage"], "zoe-plone-why-")
        self.storage = storage
        self.db = DB(storage)
        self.addCleanup(lambda: self.db.close())
        with opened(self.db) as site:
            for name in DOCUMENTS:
                if name not in site.objectIds():
                    site.invokeFactory("Document", name, title=name.title())
            transaction.commit()
            cat = site.portal_catalog._catalog
            ghost = 3_000_000
            while ghost in cat.paths:
                ghost += 1
            cat.getIndex("portal_type").index_object(ghost, Ghost())
            transaction.commit()
            self.key = aq_base(site.portal_catalog)._catalog._p_oid
        catalog.register(self.db)
        wait_ready(self.storage, self.key)

    def search(self):
        with opened(self.db) as site:
            site.portal_catalog.unrestrictedSearchResults(portal_type="Document")
            self.assertEqual(catalog._last.outcome, ("index_unavailable", "server"))

    def test_the_counts_and_the_log_give_the_cause(self):
        with self.assertLogs("zoe.plone.catalog", logging.WARNING) as logs:
            self.search()
        whys = catalog._counts_now()["whys"]["index_unavailable"]
        named = [why for why in whys if "portal_type" in why]
        self.assertEqual(len(named), 1, whys)
        (why,) = named
        self.assertIn("is not in paths", why)
        self.assertEqual(whys[why], 1)
        self.assertEqual(len(warnings_with(logs.records, why)), 1, logs.output)
        self.assertEqual(len(warnings_with(logs.records, "")), 1, logs.output)

        with self.assertNoLogs("zoe.plone.catalog", logging.WARNING):
            self.search()
        counts = catalog._counts_now()
        self.assertEqual(counts["whys"]["index_unavailable"][why], 2)
        self.assertEqual(
            counts["fallbacks"][("index_unavailable", "server")], 2, counts
        )


class TestTheLogOfACause(unittest.TestCase):
    OID = (0x2A).to_bytes(8, "big")

    def setUp(self):
        isolated(self)

    def why(self, reason, text):
        catalog._fallback_why(self.OID, reason, text, log=True)

    def test_the_first_text_of_a_reason_warns_and_a_later_one_debugs(self):
        with self.assertLogs("zoe.plone.catalog", logging.DEBUG) as logs:
            self.why("not_ready", "the mirror is building")
            self.why("not_ready", "the mirror is building")
            self.why("not_ready", "the store is closed")
            self.why("plan", "a plan key 'x'")
        levels = [
            (r.levelname, r.getMessage().rsplit(": ", 1)[1]) for r in logs.records
        ]
        self.assertEqual(
            levels,
            [
                ("WARNING", "the mirror is building"),
                ("DEBUG", "the store is closed"),
                ("WARNING", "a plan key 'x'"),
            ],
        )
        self.assertEqual(
            catalog._counts_now()["whys"],
            {
                "not_ready": {"the mirror is building": 2, "the store is closed": 1},
                "plan": {"a plan key 'x'": 1},
            },
        )

    def test_the_texts_of_a_reason_are_cut_and_bounded(self):
        with self.assertLogs("zoe.plone.catalog", logging.DEBUG):
            for i in range(catalog._WHYS + 3):
                self.why("plan", f"{i:03d}" + "x" * 300)
        whys = catalog._counts_now()["whys"]["plan"]
        self.assertEqual(len(whys), catalog._WHYS + 1)
        self.assertEqual(whys[catalog._OTHER], 3)
        self.assertTrue(all(len(why) <= 200 for why in whys))
