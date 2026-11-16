"""Acceptance test of the repair of a damaged catalog (`zoe.plone.catalog_repair`)
for the catalog mirror (ADR 0059, sections 1 and 2).

Each test copies the site of `zoe.plone.testing.ZOE_SITE` to a store of its
own, adds Documents, and damages the catalog through the stock ZCatalog
API and direct writes to `paths`, `uids`, `data` and the indexes, with one
case of each kind:

1. a path catalogued twice, and an entry of `uids` whose rid is not in
   `paths`, and a `Length` one above `len(data)`;
2. a ghost rid in the reverse maps of `portal_type`, `UID`, `created`,
   `modified` and `path`, outside `paths`;
3. a UUIDIndex without the forward entry of a rid;
4. a half-catalogued row: no entry in `portal_type`, `effective`,
   `expires` and `effectiveRange`.

The tests check that the dry run reports each kind and writes nothing;
that after the repair and a commit the catalog is consistent and each
object is found once by its UID and once by its path; and that the mirror
is ready (after a reopen of the store, as the report says) and answers a
query on `portal_type` and one on `UID` natively.

The view `@@zoe-catalog-repair` runs on a site of the functional layer of
plone.app.testing, with the ZCML of the view loaded: a Manager's GET gives
the dry run as JSON, an anonymous GET and a POST without the token of
plone.protect are refused, and a POST with the token repairs.

The layers of plone.app.testing need the interpreter of a Plone instance:
`just plone zoe backends -t test_catalog_repair` runs this module.
"""

import contextlib
import json
import time
import unittest
import urllib.parse
from unittest import mock

import transaction
from Acquisition import aq_base
from DateTime import DateTime
from plone.app.testing import (
    PLONE_FIXTURE,
    SITE_OWNER_NAME,
    SITE_OWNER_PASSWORD,
    TEST_USER_ID,
    FunctionalTesting,
    PloneSandboxLayer,
    setRoles,
)
from plone.testing import zope as zope_testing
from plone.testing.zope import Browser
from plone.uuid.interfaces import IUUID
from Testing.makerequest import makerequest
from ZODB.DB import DB
from zope.component.hooks import getSite, setSite
from zope.globalrequest import getRequest, setRequest

import zoe
from zoe.plone import catalog
from zoe.plone import testing as zoe_testing
from zoe.plone.catalog_repair import find_damage, repair
from zoe.plone.tests.helpers import copied_store

# The longest wait for the mirror, in seconds.
TIMEOUT = 30

# The Documents of the site, and the one of each kind of damage.
DOCUMENTS = [f"doc-{i}" for i in range(6)]
TWICE, GHOST_PATH, NO_FORWARD, HALF = "doc-1", "doc-2", "doc-3", "doc-4"

# The UUID of the ghost rid.
GHOST_UUID = "0" * 32

# The indexes that lose the entries of the half-catalogued row.
HALF_INDEXES = ("portal_type", "effective", "expires", "effectiveRange")


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


def path_of(obj):
    return "/".join(obj.getPhysicalPath())


def free_rid(cat, start, taken=()):
    """A rid at or after `start` that is not in `paths` nor in `taken`."""
    rid = start
    while rid in cat.paths or rid in taken:
        rid += 1
    return rid


class Ghost:
    """An object that was never in `paths`: a ghost rid indexes it."""

    portal_type = "Document"
    UID = GHOST_UUID
    created = DateTime("2001/01/01")
    modified = DateTime("2001/01/02")

    def __init__(self, path):
        self._path = path

    def getPhysicalPath(self):
        return tuple(self._path.split("/"))


def damage(site):
    """Damage the catalog of `site` with one case of each kind. Return the
    rids of the cases by kind."""
    pc = site.portal_catalog
    cat = pc._catalog
    # Kind 1: the path catalogued twice. The stock API catalogs the path
    # under a new rid while `uids` does not name the first one.
    twice = path_of(site[TWICE])
    good = cat.uids[twice]
    del cat.uids[twice]
    pc.catalog_object(site[TWICE], uid=twice)
    extra = cat.uids[twice]
    cat.uids[twice] = good
    # An entry of `uids` whose rid is not in `paths`, and a `Length` off.
    stale = free_rid(cat, 1_000_000)
    cat.uids["/plone/gone"] = stale
    cat._length.change(1)
    # Kind 2: a ghost rid in the reverse maps of five indexes.
    ghost = free_rid(cat, 2_000_000, (stale,))
    shadowed = path_of(site[GHOST_PATH])
    for name in ("portal_type", "UID", "created", "modified", "path"):
        cat.getIndex(name).index_object(ghost, Ghost(shadowed))
    # Kind 3: `removeForwardIndexEntry` of the UUIDIndex deletes the forward
    # entry of a rid that keeps its UUID.
    no_forward = cat.uids[path_of(site[NO_FORWARD])]
    uid_index = cat.getIndex("UID")
    uid_index.removeForwardIndexEntry(IUUID(site[NO_FORWARD]), no_forward)
    # Kind 4: a row without its entries in four indexes.
    half = cat.uids[path_of(site[HALF])]
    for name in HALF_INDEXES:
        cat.getIndex(name).unindex_object(half)
    return {
        "extra": extra,
        "good": good,
        "stale": stale,
        "ghost": ghost,
        "no_forward": no_forward,
        "half": half,
    }


def reverse_maps(cat):
    """The reverse map of each index of `cat` that has one, by name."""
    out = {}
    for name, index in cat.indexes.items():
        base = aq_base(index)
        found = getattr(base, "_inputs", None)
        if found is None:
            found = getattr(base, "_unindex", None)
        if found is None:
            found = getattr(getattr(base, "index", None), "_docwords", None)
        if found is not None:
            out[name] = found
    return out


def state_of(cat):
    """The state of `cat`: `paths`, `uids`, `data`, its `Length`, and the
    trees of each index."""
    indexes = {}
    for name, index in cat.indexes.items():
        base = aq_base(index)
        trees = {}
        for attr in ("_unindex", "_index", "_index_items", "_inputs"):
            tree = getattr(base, attr, None)
            if tree is not None and hasattr(tree, "items"):
                trees[attr] = [
                    (k, list(v) if hasattr(v, "keys") else v) for k, v in tree.items()
                ]
        length = getattr(base, "_length", None)
        trees["_length"] = length() if callable(length) else None
        indexes[name] = trees
    return {
        "paths": list(cat.paths.items()),
        "uids": list(cat.uids.items()),
        "data": list(cat.data.items()),
        "Length": len(cat),
        "indexes": indexes,
    }


def wait_state(storage, key, states):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        status = storage._catalog_status(key)
        if status is not None and status["state"] in states:
            return status
        time.sleep(0.05)
    raise AssertionError(
        f"the mirror is not in {states}: {storage._catalog_status(key)}"
    )


class TestTheRepairOfADamagedCatalog(unittest.TestCase):
    layer = zoe_testing.ZOE_SITE
    maxDiff = None

    def setUp(self):
        saved = dict(catalog._root)
        catalog._root.clear()
        self.addCleanup(catalog._root.update, saved)
        if not catalog.installed():
            with mock.patch.dict("os.environ", {"ZOE_PLONE_CATALOG_MODE": "on"}):
                catalog.install()
            self.addCleanup(catalog.uninstall)
        catalog._hinted.clear()
        storage, self.path = copied_store(
            self, self.layer["zoeStorage"], "zoe-plone-repair-"
        )
        self.storage = storage
        self.db = DB(storage)
        self.addCleanup(lambda: self.db.close())
        with opened(self.db) as site:
            for name in DOCUMENTS:
                if name not in site.objectIds():
                    site.invokeFactory("Document", name, title=name.title())
            transaction.commit()
            self.rids = damage(site)
            transaction.commit()
            self.key = aq_base(site.portal_catalog)._catalog._p_oid
            self.uuids = {name: IUUID(site[name]) for name in DOCUMENTS}
        catalog.register(self.db)
        # The path catalogued twice: the store refuses the catalog.
        wait_state(self.storage, self.key, ("refused",))
        # A repair outside a request, as in `bin/instance run`.
        request = getRequest()
        setRequest(None)
        self.addCleanup(setRequest, request)

    def test_the_dry_run_reports_each_kind_and_writes_nothing(self):
        with opened(self.db) as site:
            cat = site.portal_catalog._catalog
            before = state_of(cat)
            last = self.storage.lastTransaction()
            report = repair(site.portal_catalog, dry_run=True)
            self.assertEqual(find_damage(site.portal_catalog)["damaged"], True)
            self.assertEqual(site._p_jar._registered_objects, [])
            self.assertEqual(state_of(cat), before)
            transaction.commit()
        self.assertEqual(self.storage.lastTransaction(), last)
        self.assertTrue(report["dry_run"])
        self.assertTrue(report["damaged"])
        found = {
            kind: report[kind]["found"]
            for kind in (
                "duplicates",
                "stale_uids",
                "orphan_data",
                "ghosts",
                "uuid_entries",
                "half_catalogued",
            )
        }
        self.assertEqual(
            found,
            {
                "duplicates": 1,
                "stale_uids": 1,
                "orphan_data": 0,
                "ghosts": 1,
                "uuid_entries": 1,
                "half_catalogued": 1,
            },
        )
        self.assertEqual(
            report["duplicates"]["rids"],
            [{"rid": self.rids["extra"], "path": f"/plone/{TWICE}"}],
        )
        self.assertEqual(
            report["stale_uids"]["rids"],
            [{"rid": self.rids["stale"], "path": "/plone/gone"}],
        )
        ghost = report["ghosts"]["rids"][0]
        self.assertEqual(ghost["rid"], self.rids["ghost"])
        self.assertIsNone(ghost["path"])
        self.assertEqual(
            sorted(ghost["indexes"]),
            ["UID", "created", "modified", "path", "portal_type"],
        )
        self.assertEqual(
            report["uuid_entries"]["rids"],
            [
                {
                    "rid": self.rids["no_forward"],
                    "path": f"/plone/{NO_FORWARD}",
                    "index": "UID",
                }
            ],
        )
        half = report["half_catalogued"]["rids"][0]
        self.assertEqual(half["rid"], self.rids["half"])
        self.assertEqual(half["path"], f"/plone/{HALF}")
        self.assertEqual(sorted(half["indexes"]), ["effectiveRange", "portal_type"])
        self.assertEqual(report["length"]["Length"], report["length"]["data"] + 1)
        json.dumps(report)

    def repaired(self):
        """Repair the catalog and commit. Return the report."""
        with opened(self.db) as site:
            report = repair(site.portal_catalog, dry_run=False)
            transaction.commit()
        return report

    def test_the_repair_gives_a_consistent_catalog(self):
        report = self.repaired()
        self.assertFalse(report["dry_run"])
        self.assertEqual(report["unrepaired"], [])
        for kind in ("duplicates", "stale_uids", "ghosts", "half_catalogued"):
            self.assertEqual(report[kind]["repaired"], 1, kind)
        self.assertGreaterEqual(report["uuid_entries"]["repaired"], 1)
        self.assertTrue(report["length"]["repaired"])
        self.assertEqual(report["mirror"], "next_open")
        with opened(self.db) as site:
            pc = site.portal_catalog
            cat = pc._catalog
            # `uids` is the inverse of `paths`.
            paths = dict(cat.paths.items())
            self.assertEqual(dict(cat.uids.items()), {p: r for r, p in paths.items()})
            self.assertEqual(len(cat.uids), len(paths))
            # No index holds a rid outside `paths`.
            for name, reverse in reverse_maps(cat).items():
                self.assertEqual(set(reverse.keys()) - set(paths), set(), name)
            # Each UUIDIndex is consistent.
            for name, index in cat.indexes.items():
                if aq_base(index).meta_type != "UUIDIndex":
                    continue
                base = aq_base(index)
                self.assertEqual(base._length(), len(base._index), name)
                self.assertEqual(len(base._index), len(base._unindex), name)
                for rid, uuid in base._unindex.items():
                    self.assertEqual(base._index[uuid], rid, name)
            # The half-catalogued object has its entries.
            half = self.rids["half"]
            for name in HALF_INDEXES:
                self.assertIn(half, aq_base(cat.indexes[name])._unindex, name)
            self.assertEqual(len(cat), len(cat.data))
            self.assertEqual(find_damage(pc)["damaged"], False)
            # Each object is found once by its UID and once by its path.
            for name in DOCUMENTS:
                rid = cat.uids[f"/plone/{name}"]
                by_uid = pc.unrestrictedSearchResults(UID=self.uuids[name])
                self.assertEqual([b.getRID() for b in by_uid], [rid], name)
                by_path = pc.unrestrictedSearchResults(
                    path={"query": f"/plone/{name}", "depth": 0}
                )
                self.assertEqual([b.getRID() for b in by_path], [rid], name)

    def test_the_mirror_is_ready_after_the_repair_and_answers_natively(self):
        with self.assertLogs("zoe.plone.catalog", "WARNING") as logs:
            with opened(self.db) as site:
                site.portal_catalog.unrestrictedSearchResults(portal_type="Document")
                self.assertEqual(catalog._last.outcome, ("refused", "server"))
        self.assertIn("@@zoe-catalog-repair", "\n".join(logs.output))
        report = self.repaired()
        self.assertEqual(report["mirror"], "next_open")
        # A refused mirror builds again at the next open of the store.
        self.db.close()
        self.storage = zoe.Storage(self.path)
        self.db = DB(self.storage)
        wait_state(self.storage, self.key, ("ready",))
        with opened(self.db) as site:
            pc = site.portal_catalog
            documents = pc.unrestrictedSearchResults(portal_type="Document")
            self.assertEqual(catalog._last.outcome, "native")
            # The layer site can hold Documents of other test modules.
            paths = {f"/plone/{name}" for name in DOCUMENTS}
            ours = [b.getPath() for b in documents if b.getPath() in paths]
            self.assertEqual(sorted(ours), sorted(paths))
            found = pc.unrestrictedSearchResults(UID=self.uuids[NO_FORWARD])
            self.assertEqual(catalog._last.outcome, "native")
            self.assertEqual([b.getRID() for b in found], [self.rids["no_forward"]])


class RepairViewLayer(PloneSandboxLayer):
    """The fixture of plone.app.testing with the ZCML of the view."""

    defaultBases = (PLONE_FIXTURE,)

    def setUpZope(self, app, configurationContext):
        import zoe.plone

        self.loadZCML("catalog_repair.zcml", package=zoe.plone)


REPAIR_VIEW_FIXTURE = RepairViewLayer()
REPAIR_VIEW_FUNCTIONAL = FunctionalTesting(
    bases=(REPAIR_VIEW_FIXTURE,), name="RepairView:Functional"
)


class TestTheRepairView(unittest.TestCase):
    layer = REPAIR_VIEW_FUNCTIONAL

    def setUp(self):
        self.app = self.layer["app"]
        self.portal = self.layer["portal"]
        setRoles(self.portal, TEST_USER_ID, ["Manager"])
        self.portal.invokeFactory("Document", HALF, title="Half")
        transaction.commit()
        cat = self.portal.portal_catalog._catalog
        self.half = cat.uids[path_of(self.portal[HALF])]
        for name in HALF_INDEXES:
            cat.getIndex(name).unindex_object(self.half)
        transaction.commit()
        self.url = f"{self.portal.absolute_url()}/@@zoe-catalog-repair"

    def browser(self, manager=True):
        browser = Browser(self.app)
        browser.handleErrors = True
        browser.raiseHttpErrors = False
        browser.followRedirects = False
        if manager:
            browser.addHeader(
                "Authorization", f"Basic {SITE_OWNER_NAME}:{SITE_OWNER_PASSWORD}"
            )
        return browser

    def half_found(self):
        transaction.begin()
        return find_damage(self.portal.portal_catalog)["half_catalogued"]["found"]

    def test_a_managers_get_gives_the_dry_run_as_json(self):
        browser = self.browser()
        browser.open(self.url)
        self.assertTrue(browser.headers["status"].startswith("200"))
        self.assertEqual(browser.headers["Content-Type"], "application/json")
        report = json.loads(browser.contents)
        self.assertTrue(report["dry_run"])
        self.assertEqual(report["half_catalogued"]["found"], 1)
        self.assertEqual(report["half_catalogued"]["rids"][0]["rid"], self.half)
        self.assertEqual(self.half_found(), 1)

    def test_an_anonymous_get_is_refused(self):
        browser = self.browser(manager=False)
        browser.open(self.url)
        self.assertNotIn("dry_run", str(browser.contents))
        self.assertIn(browser.headers["status"][:3], ("302", "401", "403"))

    def test_a_post_without_the_token_is_refused(self):
        browser = self.browser()
        browser.post(self.url, urllib.parse.urlencode({"repair": "1"}))
        self.assertTrue(browser.headers["status"].startswith("403"))
        self.assertNotIn("dry_run", str(browser.contents))
        self.assertEqual(self.half_found(), 1)

    def test_a_post_with_the_token_repairs(self):
        from plone.protect import createToken

        zope_testing.login(self.app["acl_users"], SITE_OWNER_NAME)
        try:
            token = createToken()
        finally:
            zope_testing.logout()
        browser = self.browser()
        browser.post(
            self.url, urllib.parse.urlencode({"repair": "1", "_authenticator": token})
        )
        self.assertTrue(browser.headers["status"].startswith("200"))
        report = json.loads(browser.contents)
        self.assertFalse(report["dry_run"])
        self.assertEqual(report["half_catalogued"]["repaired"], 1)
        self.assertEqual(report["unrepaired"], [])
        self.assertEqual(self.half_found(), 0)
