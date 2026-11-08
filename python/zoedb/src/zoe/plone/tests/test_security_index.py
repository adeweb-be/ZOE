"""Acceptance tests of the security backend of `zoe.plone` (ADR 0061): the
`ZoeSecurityIndex` in the place of the `KeywordIndex`
`allowedRolesAndUsers`.

The tests make their own sites, each in a database on a new `zoe.Storage`:
nested folders in three workflow states, users and a group with local
roles, an Owner local role, a folder that blocks the inheritance, a
sequence setting below the site root, private items in public folders and
the reverse, a comment, and a site in a mounted database. Each catalog
also holds a stock `KeywordIndex` of the values of the indexer, which the
check indexes again in full, so it holds the value that a stock catalog
holds after `reindexObjectSecurity`.

After each change, for each entry, the resolved value of the index on the
Python path equals the value of the stock index, and the searches of each
token, of anonymous, of a member and of a member of the group give the
same rids from the index on the Python path, from the mirror of the store,
and from the stock index.

The layers of plone.app.testing need the interpreter of a Plone instance:
`bin/test -s zoe.plone` of the instance of the zoe scenario runs these
tests, and `just plone zoe backends` runs it. They install the backend
themselves.
"""

import contextlib
import random
import shutil
import tempfile
import time
import unittest
from unittest import mock

import App.config
import transaction
import ZODB
from Acquisition import aq_base
from OFS.Application import Application
from OFS.Folder import Folder
from plone.app.testing import SITE_OWNER_NAME
from plone.testing import zope as zope_testing
from Products.CMFCore.interfaces import ISiteRoot
from Products.ZCatalog.Catalog import Catalog
from Testing.makerequest import makerequest
from ZODB.FileStorage import FileStorage
from zope.component import getGlobalSiteManager
from zope.component.hooks import getSite, setSite
from zope.event import notify
from zope.interface import Interface, alsoProvides, implementer
from zope.lifecycleevent.interfaces import IObjectAddedEvent, IObjectRemovedEvent
from zope.processlifetime import DatabaseOpenedWithRoot, IDatabaseOpenedWithRoot

import zoe
import zoe.kv
from zoe.plone import catalog, indexing, security
from zoe.plone import testing as zoe_testing

try:
    from borg.localrole.interfaces import ILocalRoleProvider
except ImportError:  # pragma: no cover - outside a Plone instance
    ILocalRoleProvider = Interface

# The longest wait for the mirror, in seconds.
TIMEOUT = 60
INDEX = "allowedRolesAndUsers"
# The stock index of the values of the indexer.
STOCK = "stockAllowedRolesAndUsers"
MEMBER = "sec-member"
GROUPED = "sec-grouped"
AUTHOR = "sec-author"
GROUP = "sec-editors"
PASSWORD = "sec-secret-pw"
SEED = 6101


def subscribe(test):
    """Register the subscribers of `zoe.plone.catalog` that `configure.zcml`
    names, until the end of the test."""
    registry = getGlobalSiteManager()
    for handler, required in (
        (catalog.database_opened, (IDatabaseOpenedWithRoot,)),
        (catalog.site_added, (ISiteRoot, IObjectAddedEvent)),
        (catalog.site_removed, (ISiteRoot, IObjectRemovedEvent)),
    ):
        registry.registerHandler(handler, required)
        test.addCleanup(registry.unregisterHandler, handler, required)


def switch_on(test):
    """Install the catalog backend and the security backend until the end
    of the test."""
    with mock.patch.dict("os.environ", {"ZOE_PLONE_CATALOG_MODE": "on"}):
        catalog.install()
    test.addCleanup(catalog._root.clear)
    test.addCleanup(catalog.uninstall)
    security.install()
    test.addCleanup(security._root.clear)
    test.addCleanup(security.uninstall)


def new_db(test):
    """Return a database on a new `zoe.Storage`, with a Zope application and
    the site owner, until the end of the test."""
    directory = tempfile.mkdtemp(prefix="zoe-plone-security-")
    test.addCleanup(shutil.rmtree, directory, True)
    db = ZODB.DB(zoe.Storage(f"{directory}/zoe"))
    test.addCleanup(db.close)
    connection = db.open()
    try:
        app = Application()
        connection.root()["Application"] = app
        app.acl_users._doAddUser(SITE_OWNER_NAME, PASSWORD, ["Manager"], [])
        transaction.commit()
    finally:
        transaction.abort()
        connection.close()
    return db


@contextlib.contextmanager
def opened(db, path=("plone",)):
    """Open a connection of `db`, log in as the site owner and make the site
    at `path` the current site. Yield it, or the application for an empty
    path. At the end, abort the transaction."""
    old = getSite()
    connection = db.open()
    try:
        app = makerequest(connection.root()["Application"])
        zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
        site = app.unrestrictedTraverse(list(path)) if path else app
        if ISiteRoot.providedBy(aq_base(site)):
            setSite(site)
        yield site
    finally:
        transaction.abort()
        zope_testing.logout()
        setSite(old)
        connection.close()


def add_site(container, name="plone"):
    """Add a Plone site with plone.app.discussion, the stock index, the
    users and the group of the tests."""
    from Products.CMFPlone.factory import addPloneSite

    setSite(None)
    addPloneSite(
        container,
        name,
        setup_content=False,
        extension_ids=(
            "plone.app.contenttypes:default",
            "plone.app.discussion:default",
        ),
    )
    site = container[name]
    setSite(site)
    site.portal_catalog.addIndex(STOCK, "KeywordIndex", extra={"indexed_attrs": INDEX})
    users = site.acl_users
    users.userFolderAddUser(MEMBER, PASSWORD, ["Member"], [])
    users.userFolderAddUser(GROUPED, PASSWORD, ["Member"], [])
    users.userFolderAddUser(AUTHOR, PASSWORD, ["Manager"], [])
    site.portal_groups.addGroup(GROUP)
    site.portal_groups.addPrincipalToGroup(GROUPED, GROUP)
    return site


def transition(site, obj, *names):
    for name in names:
        site.portal_workflow.doActionFor(obj, name)


def comment(doc, text):
    from plone.app.discussion.interfaces import IConversation
    from zope.component import createObject

    reply = createObject("plone.Comment")
    reply.text = text
    reply.author_username = SITE_OWNER_NAME
    return IConversation(doc).addComment(reply)


def make_content(site):
    """The content of the synthetic site. The author owns a folder and its
    items, so they hold its Owner local role."""
    zope_testing.login(site.acl_users, AUTHOR)
    site.invokeFactory("Folder", "public", title="Public")
    public = site["public"]
    transition(site, public, "publish")
    for i in range(4):
        public.invokeFactory("Document", f"doc-{i}", title=f"Doc {i}")
    transition(site, public["doc-1"], "publish")
    transition(site, public["doc-2"], "submit")
    public.invokeFactory("File", "file", title="File")
    public.invokeFactory("Folder", "inner", title="Inner")
    transition(site, public["inner"], "publish")
    public["inner"].invokeFactory("Document", "deep", title="Deep")
    zope_testing.login(site.getPhysicalRoot().acl_users, SITE_OWNER_NAME)
    site.invokeFactory("Folder", "private", title="Private")
    private = site["private"]
    private.manage_setLocalRoles(GROUP, ["Editor"])
    private.manage_setLocalRoles(MEMBER, ["Reader"])
    private.invokeFactory("Document", "secret", title="Secret")
    private.invokeFactory("Document", "open", title="Open")
    transition(site, private["open"], "publish")
    private.invokeFactory("Folder", "blocked", title="Blocked")
    blocked = private["blocked"]
    blocked.__ac_local_roles_block__ = True
    blocked.manage_setLocalRoles(GROUPED, ["Reader"])
    blocked.invokeFactory("Document", "below", title="Below")
    site.invokeFactory("Folder", "listed", title="Listed")
    listed = site["listed"]
    # A sequence setting: these roles and the roles of the site root.
    listed.manage_permission(
        "Access contents information", ["Reviewer", "Manager"], acquire=1
    )
    listed.reindexObjectSecurity()
    listed.invokeFactory("Document", "item", title="Item")
    listed.invokeFactory("File", "attachment", title="Attachment")
    comment(public["doc-0"], "A comment")
    comment(private["secret"], "Another comment")


def tokens_of(site):
    """The tokens of the searches of anonymous, of a member and of a member
    of the group."""
    pc = site.portal_catalog
    users = site.acl_users
    out = [["Anonymous"]]
    for name in (MEMBER, GROUPED):
        out.append(pc._listAllowedRolesAndUsers(users.getUserById(name)))
    return out


def stock_search(site, query):
    """The rids of the stock search of ZCatalog."""
    stock = catalog._installed.get("stock", Catalog.search)
    with mock.patch.object(Catalog, "search", stock):
        found = site.portal_catalog.unrestrictedSearchResults(**query)
        return sorted(brain.getRID() for brain in found)


def last_tid(storage):
    return int.from_bytes(storage.lastTransaction(), "big")


def wait_applied(storage, key):
    """Wait until the mirror of `key` is ready and applied the last commit
    of the store."""
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        status = storage._catalog_status(key)
        if (
            status is not None
            and status["state"] == "ready"
            and (status["applied"] or 0) >= last_tid(storage)
        ):
            return
        time.sleep(0.05)
    raise AssertionError(f"the mirror lags: {storage._catalog_status(key)}")


class Checks:
    """The checks of a site of a test."""

    def index_stock(self, db, path):
        with opened(db, path) as site:
            site.portal_catalog.reindexIndex(STOCK, None)
            transaction.commit()

    def check(self, db, path=("plone",), storage=None, what=""):
        """Check each entry and each search of the site at `path`. With
        `storage`, the store of the catalog, the mirror answers the
        searches of the index."""
        self.index_stock(db, path)
        with opened(db, path) as site:
            pc = site.portal_catalog
            index = security.security_index(pc)
            self.assertIsNotNone(index, what)
            stock = pc._catalog.getIndex(STOCK)
            searches = [[token] for token in stock.uniqueValues()]
            for rid in pc._catalog.paths.keys():
                path_of = pc._catalog.paths[rid]
                self.assertEqual(
                    index.getEntryForObject(rid, []),
                    sorted(stock.getEntryForObject(rid, [])),
                    f"{what}: the value of {path_of}",
                )
            searches.extend(tokens_of(site))
            key = None
            if storage is not None:
                key = aq_base(pc)._catalog._p_oid
                wait_applied(storage, key)
                catalog._counts.clear()
            for tokens in searches:
                expected = stock_search(site, {STOCK: tokens})
                python = stock_search(site, {INDEX: tokens})
                self.assertEqual(python, expected, f"{what}: the Python path, {tokens}")
                if storage is not None:
                    found = pc.unrestrictedSearchResults(**{INDEX: tokens})
                    native = sorted(brain.getRID() for brain in found)
                    self.assertEqual(native, expected, f"{what}: the mirror, {tokens}")
            if storage is not None:
                counts = catalog._counts_now()["queries"]
                server = storage._catalog_counts()
                status = storage._catalog_status(key)
                self.assertEqual(
                    counts.get("native", 0),
                    len(searches),
                    f"{what}: {server} {status}",
                )
                self.assertNotIn("python", counts, what)
            return searches


class TestTheSecurityIndex(Checks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        # The database closes after the backends give their index back.
        self.db = new_db(self)
        self.storage = self.db.storage
        subscribe(self)
        switch_on(self)
        with opened(self.db, ()) as app:
            site = add_site(app)
            make_content(site)
            transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        security._join()

    def test_the_backend_puts_the_index_in_and_each_value_is_the_plone_value(self):
        self.assertTrue(security.installed())
        with opened(self.db) as site:
            index = security.security_index(site.portal_catalog)
            self.assertIsInstance(aq_base(index), security.ZoeSecurityIndex)
            # The Owner local role of the author gives its token to the
            # private items of the author.
            rid = site.portal_catalog._catalog.uids["/plone/public/doc-0"]
            self.assertIn(f"user:{AUTHOR}", index.getEntryForObject(rid))
            # The comment is below its conversation, outside the catalog.
            found = site.portal_catalog.unrestrictedSearchResults(
                portal_type="Discussion Item"
            )
            self.assertEqual(len(found), 2)
            self.assertIn("++conversation++default", found[0].getPath())
        self.check(self.db, storage=self.storage, what="after the install")

    def test_a_seeded_sequence_of_changes_keeps_every_value(self):
        rng = random.Random(SEED)
        made = 0
        for step in range(24):
            with opened(self.db) as site:
                what = self.change(site, rng, step)
                made += 1
                transaction.commit()
            self.check(self.db, storage=self.storage, what=f"step {step}: {what}")
        # The deletion of a user deletes its local roles on the whole site.
        with opened(self.db) as site:
            site.portal_membership.deleteLocalRoles(
                site, [MEMBER], reindex=1, recursive=1
            )
            transaction.commit()
        self.check(self.db, storage=self.storage, what="deleteLocalRoles")
        with opened(self.db) as site:
            for brain in site.portal_catalog.unrestrictedSearchResults():
                obj = brain._unrestrictedGetObject()
                self.assertNotIn(MEMBER, dict(obj.get_local_roles()))
        self.assertEqual(made, 24)

    def folders(self, site):
        found = site.portal_catalog.unrestrictedSearchResults(portal_type="Folder")
        return sorted(brain.getPath() for brain in found)

    def change(self, site, rng, step):
        """One change of security, of place or of content."""
        app = site.getPhysicalRoot()
        folders = self.folders(site)
        kind = ("share", "transition", "move", "create", "delete", "block")[step % 6]
        folder = app.unrestrictedTraverse(rng.choice(folders))
        if kind == "share":
            who = rng.choice((MEMBER, GROUPED, GROUP))
            role = rng.choice(("Reader", "Editor", "Contributor"))
            folder.manage_setLocalRoles(who, [role])
            folder.reindexObjectSecurity()
            return f"share {role} with {who} on {folder.getId()}"
        if kind == "transition":
            items = site.portal_catalog.unrestrictedSearchResults(
                path="/".join(folder.getPhysicalPath())
            )
            target = rng.choice(list(items))._unrestrictedGetObject()
            names = [t["id"] for t in site.portal_workflow.getTransitionsFor(target)]
            if not names:
                return "no transition"
            name = rng.choice(sorted(names))
            site.portal_workflow.doActionFor(target, name)
            return f"{name} of {target.getId()}"
        if kind == "move":
            moved = rng.choice([f for f in folders if f.count("/") > 2] or folders)
            source = app.unrestrictedTraverse(moved)
            target = app.unrestrictedTraverse(
                rng.choice(
                    [f for f in folders if not f.startswith(moved)] or ["/plone"]
                )
            )
            parent = source.aq_parent
            if parent is target:
                return "no move"
            data = parent.manage_cutObjects([source.getId()])
            target.manage_pasteObjects(data)
            return f"move {moved} to {target.getId()}"
        if kind == "create":
            name = f"new-{step}"
            folder.invokeFactory(rng.choice(("Document", "Folder")), name, title=name)
            if rng.random() < 0.5:
                transition(site, folder[name], "publish")
            return f"create {name} in {folder.getId()}"
        if kind == "delete":
            ids = [i for i in folder.objectIds() if not i.startswith("++")]
            if not ids:
                return "no delete"
            gone = rng.choice(sorted(ids))
            folder.manage_delObjects([gone])
            return f"delete {gone}"
        folder.__ac_local_roles_block__ = not getattr(
            aq_base(folder), "__ac_local_roles_block__", False
        )
        folder.reindexObjectSecurity()
        return f"block of {folder.getId()}"

    def test_a_share_and_a_transition_write_the_entry_of_the_folder_alone(self):
        from Products.CMFCore.indexing import processQueue

        written = {}
        with opened(self.db) as site:
            for name, count in (("small", 500), ("large", 5000)):
                site.invokeFactory("Folder", name, title=name)
                transition(site, site[name], "publish")
                folder = site[name]
                for i in range(count // 50):
                    folder.invokeFactory("Folder", f"f{i}", title=f"f{i}")
                    sub = folder[f"f{i}"]
                    for j in range(49):
                        sub.invokeFactory("Document", f"d{j}", title=f"d{j}")
                    if i % 4 == 0:
                        transition(site, sub, "publish")
                    transaction.savepoint(optimistic=True)
            transaction.commit()
        for name in ("small", "large"):
            for change in ("share", "transition"):
                with opened(self.db) as site:
                    folder = site[name]
                    if change == "share":
                        folder.manage_setLocalRoles(MEMBER, ["Reader"])
                        folder.reindexObjectSecurity()
                    else:
                        transition(site, folder, "retract")
                    processQueue()
                    written[name, change] = len(folder._p_jar._registered_objects)
                    transaction.commit()
        self.assertEqual(written["small", "share"], written["large", "share"])
        self.assertEqual(written["small", "transition"], written["large", "transition"])
        self.assertLess(written["large", "share"], 10)
        self.check(self.db, storage=self.storage, what="after the changes")

    def test_with_queued_indexing_a_share_and_a_transition_write_one_key(self):
        indexing.install()
        self.addCleanup(indexing.uninstall)
        with opened(self.db) as site:
            folder = site["private"]
            oid = aq_base(site.portal_catalog)._p_oid
            queue = indexing.queue_name(oid)
        for change in ("share", "transition"):
            with opened(self.db) as site:
                folder = site["private"]
                if change == "share":
                    folder.manage_setLocalRoles(MEMBER, ["Contributor"])
                    folder.reindexObjectSecurity()
                else:
                    transition(site, folder, "publish")
                transaction.commit()
            keys = list(self.storage.kv_scan("durable", queue))
            self.assertEqual(len(keys), 1, change)
            with opened(self.db) as site:
                indexing.Indexer(site).run()
            self.assertEqual(list(self.storage.kv_scan("durable", queue)), [])
            self.check(self.db, storage=self.storage, what=change)

    def test_install_twice_then_uninstall_gives_the_stock_index_back(self):
        from Products.CMFCore.CMFCatalogAware import CatalogAware
        from Products.CMFCore.MembershipTool import MembershipTool

        method = CatalogAware.reindexObjectSecurity
        stock_delete = security._stock["deleteLocalRoles"]
        stock_reindex = security._stock["reindexObjectSecurity"]
        security.install()
        self.assertIs(CatalogAware.reindexObjectSecurity, method)
        self.assertEqual(method, security.reindexObjectSecurity)
        self.assertTrue(security.installed())
        security.uninstall()
        self.assertFalse(security.installed())
        self.assertIs(CatalogAware.reindexObjectSecurity, stock_reindex)
        self.assertIs(MembershipTool.deleteLocalRoles, stock_delete)
        security.uninstall()
        self.index_stock(self.db, ("plone",))
        with opened(self.db) as site:
            pc = site.portal_catalog
            self.assertIsNone(security.security_index(pc))
            stock = pc._catalog.getIndex(STOCK)
            restored = pc._catalog.getIndex(INDEX)
            self.assertEqual(restored.meta_type, "KeywordIndex")
            for rid in pc._catalog.paths.keys():
                self.assertEqual(
                    sorted(restored.getEntryForObject(rid, [])),
                    sorted(stock.getEntryForObject(rid, [])),
                )

    def test_a_site_created_after_install_gets_the_index_in_its_transaction(self):
        with opened(self.db, ()) as app:
            add_site(app, "later")
            make_content(app["later"])
            transaction.commit()
        created = last_tid(self.storage)
        with opened(self.db, ("later",)) as site:
            self.assertIsNotNone(security.security_index(site.portal_catalog))
        # No commit after the one that created the site put it in.
        self.assertEqual(last_tid(self.storage), created)
        self.check(self.db, ("later",), what="a new site")


class IProvided(Interface):
    """The folders of a local role provider of the tests."""


@implementer(ILocalRoleProvider)
class Provider:
    """A local role provider other than the default adapter: Reader for
    the member of the group."""

    def __init__(self, context):
        self.context = context

    def getRoles(self, principal):
        return ["Reader"] if principal == GROUPED else []

    def getAllRoles(self):
        yield GROUPED, ["Reader"]


class TestOpaqueEntries(Checks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def setUp(self):
        # The database closes after the backends give their index back.
        self.db = new_db(self)
        self.storage = self.db.storage
        subscribe(self)
        switch_on(self)
        with opened(self.db, ()) as app:
            site = add_site(app)
            make_content(site)
            transaction.commit()
        with opened(self.db) as site:
            security.replace_index(site.portal_catalog)
            transaction.commit()
        notify(DatabaseOpenedWithRoot(self.db))
        security._join()

    def opaque_below(self, site, path):
        pc = site.portal_catalog
        index = security.security_index(pc)
        found = pc.unrestrictedSearchResults(path=path)
        rids = {brain.getRID() for brain in found}
        self.assertTrue(rids)
        self.assertEqual(rids - set(index._opaque), set())

    def test_a_local_role_provider_on_a_folder_makes_its_subtree_opaque(self):
        registry = getGlobalSiteManager()
        registry.registerAdapter(Provider, (IProvided,), ILocalRoleProvider, "tests")
        self.addCleanup(
            registry.unregisterAdapter,
            Provider,
            (IProvided,),
            ILocalRoleProvider,
            "tests",
        )
        with opened(self.db) as site:
            alsoProvides(site["private"], IProvided)
            site["private"].reindexObjectSecurity()
            transaction.commit()
        with opened(self.db) as site:
            self.opaque_below(site, "/plone/private")
        self.check(self.db, storage=self.storage, what="a provider")
        # A security reindex above the opaque entries reindexes them.
        with opened(self.db) as site:
            site.manage_setLocalRoles(MEMBER, ["Editor"])
            site.manage_permission(
                "Access contents information",
                ["Editor", "Manager", "Site Administrator"],
                acquire=0,
            )
            site.reindexObjectSecurity()
            transaction.commit()
        self.check(self.db, storage=self.storage, what="a share above")

    def test_a_setting_that_is_a_str_on_a_folder_makes_its_subtree_opaque(self):
        with opened(self.db) as site:
            folder = site["public"]
            folder._Access_contents_information_Permission = "View"
            folder.reindexObjectSecurity()
            transaction.commit()
        with opened(self.db) as site:
            self.opaque_below(site, "/plone/public")
        self.check(self.db, storage=self.storage, what="a str setting")
        with opened(self.db) as site:
            transition(site, site["public"], "retract")
            transaction.commit()
        with opened(self.db) as site:
            index = security.security_index(site.portal_catalog)
            self.assertEqual(
                [p for p in index._opaque.keys()],
                [],
            )
        self.check(self.db, storage=self.storage, what="a tuple again")


class TestASiteInAMountedDatabase(Checks, unittest.TestCase):
    layer = zoe_testing.ZOE_SITE

    def test_the_chain_of_a_site_below_a_mount_point_resolves(self):
        directory = tempfile.mkdtemp(prefix="zoe-plone-security-")
        self.addCleanup(shutil.rmtree, directory, True)
        root = ZODB.DB(FileStorage(f"{directory}/Data.fs"), database_name="main")
        self.addCleanup(root.close)
        mounted = ZODB.DB(
            zoe.Storage(f"{directory}/zoe"),
            databases=root.databases,
            database_name="mounted",
        )
        self.addCleanup(mounted.close)
        subscribe(self)
        switch_on(self)
        connection = root.open()
        try:
            app = Application()
            connection.root()["Application"] = app
            app.acl_users._doAddUser(SITE_OWNER_NAME, PASSWORD, ["Manager"], [])
            folder = Folder("mounted")
            connection.get_connection("mounted").add(folder)
            app._setObject("mounted", folder)
            transaction.commit()
        finally:
            transaction.abort()
            connection.close()
        with opened(root, ("mounted",)) as mount:
            mount.manage_setLocalRoles(GROUPED, ["Reader"])
            site = add_site(mount, "site")
            make_content(site)
            transaction.commit()
        path = ("mounted", "site")
        with opened(root, path) as site:
            self.assertIsNotNone(security.security_index(site.portal_catalog))
        dbtab = App.config.getConfiguration().dbtab
        with mock.patch.dict(dbtab.mount_paths, {"/mounted": "mounted"}):
            notify(DatabaseOpenedWithRoot(root))
            security._join()
        self.check(root, path, storage=mounted.storage, what="a mounted site")
        with opened(root, path) as site:
            site["private"].manage_setLocalRoles(MEMBER, ["Editor"])
            site["private"].reindexObjectSecurity()
            transition(site, site["public"], "retract")
            transaction.commit()
        self.check(root, path, storage=mounted.storage, what="changes")
