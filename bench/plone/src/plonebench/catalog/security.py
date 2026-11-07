"""The check of the security index of `zoe.plone` (ADR 0061): the gate
item G5, "security bitmaps identical to reindexObjectSecurity on the
corpus".

    run.py ZOPE_CONF /abs/path/security.py OUT_JSON [--synthetic] [--opaque]

`inzope/run.py` runs it inside Zope, with the storage of ZOPE_CONF: an
in-process `zoe.Storage`, which holds the catalog mirror. `--synthetic`
first adds a small Plone site with the cases that a corpus of B5 does not
hold: an Owner local role, a sequence setting below the site root, a
block, a group, and a comment below its conversation. Without it, the
site is the site `Plone` of the store, such as a fixture of B5.

The check adds to the catalog a stock `KeywordIndex` of the values of the
indexer of `allowedRolesAndUsers`, which it indexes again in full at each
step: it holds the value that a stock catalog holds after
`reindexObjectSecurity`. At each step it compares, for each entry, the
resolved value of the `ZoeSecurityIndex` on the Python path with the value
of the stock index, and, for each token of the stock index and for the
tokens of anonymous and of two members, the rids of a search of the mirror
and of the Python path with the rids of the stock index.

The steps: the stock catalog against the indexer, the install of the
index, a share on the largest folder (Reader for a member), the transition
of that folder, a block of the local roles of a leaf folder, a local role
on a private Document, and the deletion of the local roles of the member
on the whole site, as the deletion of a user does. `--opaque` adds, before
the deletion, a local role provider for every Folder and a setting that is
a str on a leaf folder. The share and the transition also count the
records that they write.

OUT_JSON gets the report. The script exits with 1 when a step differs.
"""

import json
import sys
import time

STOCK = "stockAllowedRolesAndUsers"
INDEX = "allowedRolesAndUsers"
SITE_ID = "Plone"
# The longest wait for the mirror, in seconds.
TIMEOUT = 600
PASSWORD = "security-check-pw"


def _commit(note):
    import transaction

    txn = transaction.get()
    txn.note(note)
    transaction.commit()


def _transition(site, obj, name=None):
    """Run the transition `name`, or the first transition of `obj`. Return
    its name, or None."""
    workflow = site.portal_workflow
    names = sorted(t["id"] for t in workflow.getTransitionsFor(obj))
    if name is None:
        name = next(iter(names), None)
    if name is None or name not in names:
        return None
    workflow.doActionFor(obj, name)
    return name


def build_synthetic(app):
    """Add the site `Plone` with the cases that a corpus does not hold."""
    from plone.app.discussion.interfaces import IConversation
    from plone.testing import zope as zope_testing
    from Products.CMFPlone.factory import addPloneSite
    from zope.component import createObject
    from zope.component.hooks import setSite

    addPloneSite(
        app,
        SITE_ID,
        extension_ids=(
            "plone.app.contenttypes:default",
            "plone.app.discussion:default",
        ),
    )
    site = app[SITE_ID]
    setSite(site)
    users = site.acl_users
    for name, roles in (
        ("member-000", ["Member"]),
        ("member-001", ["Member"]),
        ("author", ["Manager"]),
    ):
        users.userFolderAddUser(name, PASSWORD, roles, [])
    site.portal_groups.addGroup("editors")
    site.portal_groups.addPrincipalToGroup("member-001", "editors")
    zope_testing.login(users, "author")
    site.invokeFactory("Folder", "section", title="Section")
    section = site["section"]
    _transition(site, section, "publish")
    section.manage_setLocalRoles("editors", ["Editor", "Reviewer"])
    for i in range(3):
        section.invokeFactory("Folder", f"unit-{i}", title=f"Unit {i}")
        unit = section[f"unit-{i}"]
        _transition(site, unit, "publish")
        for j in range(12):
            unit.invokeFactory("Document", f"doc-{j}", title=f"Doc {j}")
            doc = unit[f"doc-{j}"]
            if j % 3 == 1:
                _transition(site, doc, "publish")
            elif j % 3 == 2:
                _transition(site, doc, "submit")
        unit.invokeFactory("File", "file", title="File")
        unit.invokeFactory("Folder", "leaf", title="Leaf")
        unit["leaf"].invokeFactory("Document", "inner", title="Inner")
    section["unit-1"].__ac_local_roles_block__ = True
    section["unit-1"].reindexObjectSecurity()
    site.invokeFactory("Folder", "listed", title="Listed")
    listed = site["listed"]
    listed.manage_permission(
        "Access contents information", ["Reviewer", "Manager"], acquire=1
    )
    listed.reindexObjectSecurity()
    listed.invokeFactory("Document", "item", title="Item")
    reply = createObject("plone.Comment")
    reply.text = "A comment"
    reply.author_username = "author"
    IConversation(section["unit-0"]["doc-0"]).addComment(reply)
    zope_testing.logout()
    _commit("security check: the synthetic site")
    return site


def _stock_search(site, query):
    from unittest import mock

    from Products.ZCatalog.Catalog import Catalog
    from zoe.plone import catalog

    stock = catalog._installed.get("stock", Catalog.search)
    with mock.patch.object(Catalog, "search", stock):
        found = site.portal_catalog.unrestrictedSearchResults(**query)
        return sorted(brain.getRID() for brain in found)


def _wait_applied(storage, key):
    deadline = time.monotonic() + TIMEOUT
    while time.monotonic() < deadline:
        status = storage._catalog_status(key)
        last = int.from_bytes(storage.lastTransaction(), "big")
        if (
            status is not None
            and status["state"] == "ready"
            and (status["applied"] or 0) >= last
        ):
            return status
        time.sleep(0.1)
    raise RuntimeError(f"the mirror lags: {storage._catalog_status(key)}")


class Check:
    """The steps of the check on one site."""

    def __init__(self, app, site, members):
        from Acquisition import aq_base

        self.app = app
        self.site = site
        self.members = members
        self.storage = aq_base(site.portal_catalog)._p_jar.db().storage
        self.key = aq_base(site.portal_catalog)._catalog._p_oid
        self.steps = []

    def tokens(self):
        pc = self.site.portal_catalog
        users = self.site.acl_users
        out = [["Anonymous"]]
        for name in self.members:
            user = users.getUserById(name)
            if user is not None:
                out.append(pc._listAllowedRolesAndUsers(user))
        return out

    def index_stock(self):
        self.site.portal_catalog.reindexIndex(STOCK, None)
        _commit("security check: the stock index")

    def compare(self, name, extra=None):
        """Compare each entry and each search, and keep the step."""
        from zoe.plone import catalog, security

        started = time.monotonic()
        self.index_stock()
        pc = self.site.portal_catalog
        index = security.security_index(pc)
        stock = pc._catalog.getIndex(STOCK)
        entries = 0
        differ = []
        for rid in pc._catalog.paths.keys():
            entries += 1
            got = index.getEntryForObject(rid, [])
            want = sorted(stock.getEntryForObject(rid, []))
            if got != want:
                differ.append(pc._catalog.paths[rid])
        searches = [[token] for token in stock.uniqueValues()] + self.tokens()
        _wait_applied(self.storage, self.key)
        catalog._counts.clear()
        queries = []
        for tokens in searches:
            want = _stock_search(self.site, {STOCK: tokens})
            python = _stock_search(self.site, {INDEX: tokens})
            found = pc.unrestrictedSearchResults(**{INDEX: tokens})
            native = sorted(brain.getRID() for brain in found)
            if python != want or native != want:
                queries.append(
                    {
                        "tokens": tokens,
                        "stock": len(want),
                        "python": len(python),
                        "mirror": len(native),
                    }
                )
        counts = catalog._counts_now()["queries"]
        step = {
            "step": name,
            "entries": entries,
            "differ": len(differ),
            "differing": differ[:20],
            "searches": len(searches),
            "searches_differ": queries,
            "native": counts.get("native", 0),
            "python": counts.get("python", 0),
            "seconds": round(time.monotonic() - started, 3),
        }
        if extra:
            step.update(extra)
        self.steps.append(step)
        print(
            f"security check: {name}: {entries} entries, {len(differ)} differ,"
            f" {len(searches)} searches, {len(queries)} differ,"
            f" {step['native']} native",
            flush=True,
        )
        return step

    def written(self, change):
        """Run `change`, then count the records of its transaction, and
        commit it."""
        from Products.CMFCore.indexing import processQueue

        started = time.monotonic()
        change()
        processQueue()
        jar = self.site.portal_catalog._p_jar
        records = len(jar._registered_objects)
        _commit("security check: a change")
        return {
            "records": records,
            "change_seconds": round(time.monotonic() - started, 3),
        }


def largest_folder(site):
    """The Folder with the most catalog entries below it, and their
    number."""
    pc = site.portal_catalog
    paths = sorted(
        b.getPath() for b in pc.unrestrictedSearchResults(portal_type="Folder")
    )
    every = sorted(pc._catalog.uids.keys())
    best, most = None, -1
    import bisect

    for path in paths:
        low = bisect.bisect_left(every, path + "/")
        high = bisect.bisect_left(every, path + "0")
        if high - low > most:
            best, most = path, high - low
    return best, most


def leaf_folders(site):
    pc = site.portal_catalog
    folders = sorted(
        b.getPath() for b in pc.unrestrictedSearchResults(portal_type="Folder")
    )
    return [p for p in folders if not any(f.startswith(p + "/") for f in folders)]


def run(app, out, synthetic=False, opaque=False):
    """Run the check. Return the report."""
    import transaction
    from AccessControl.SecurityManagement import newSecurityManager
    from AccessControl.SpecialUsers import system
    from zoe.plone import catalog, security
    from zope.component.hooks import setSite

    newSecurityManager(None, system)
    if synthetic:
        build_synthetic(app)
        members = ["member-000", "member-001"]
    else:
        members = []
    site = app[SITE_ID]
    setSite(site)
    pc = site.portal_catalog
    if not members:
        members = sorted(site.portal_membership.listMemberIds())[:2]
    # The mirror of the store.
    catalog.install()
    db = site._p_jar.db()
    catalog.register(db)
    # The stock catalog against the indexer.
    if STOCK not in pc.indexes():
        pc.addIndex(STOCK, "KeywordIndex", extra={"indexed_attrs": INDEX})
    _commit("security check: the stock index")
    check = Check(app, site, members)
    check.index_stock()
    stock = pc._catalog.getIndex(STOCK)
    current = pc._catalog.getIndex(INDEX)
    stale = [
        pc._catalog.paths[rid]
        for rid in pc._catalog.paths.keys()
        if sorted(current.getEntryForObject(rid, []) or [])
        != sorted(stock.getEntryForObject(rid, []) or [])
    ]
    report = {
        "site": "/".join(site.getPhysicalPath()),
        "members": members,
        "stock_stale": len(stale),
        "steps": check.steps,
    }
    security.install()
    started = time.monotonic()
    security.replace_index(pc)
    _commit("security check: the index")
    check.compare("install", {"install_seconds": round(time.monotonic() - started, 3)})
    folder_path, below = largest_folder(site)
    report["largest"] = {"path": folder_path, "descendants": below}
    folder = app.unrestrictedTraverse(folder_path)
    member = members[0]

    def share():
        folder.manage_setLocalRoles(member, ["Reader"])
        folder.reindexObjectSecurity()

    check.compare("share on the largest folder", check.written(share))
    moved = {}

    def transition():
        moved["name"] = _transition(site, folder, "retract") or _transition(
            site, folder
        )

    extra = check.written(transition)
    extra["transition"] = moved.get("name")
    check.compare("transition of the largest folder", extra)
    leaves = leaf_folders(site)
    if leaves:
        leaf = app.unrestrictedTraverse(leaves[0])
        leaf.__ac_local_roles_block__ = True
        leaf.reindexObjectSecurity()
        _commit("security check: a block")
        check.compare("block of a leaf folder", {"folder": leaves[0]})
    private = pc.unrestrictedSearchResults(
        portal_type="Document", review_state="private"
    )
    if private:
        doc = private[0]._unrestrictedGetObject()
        doc.manage_setLocalRoles(members[-1], ["Reader"])
        doc.reindexObjectSecurity()
        _commit("security check: a local role on a private Document")
        check.compare("local role on a private Document")
    if opaque and len(leaves) > 2:
        _opaque_steps(app, site, check, members, leaves)
    started = time.monotonic()
    site.portal_membership.deleteLocalRoles(site, [member], reindex=1, recursive=1)
    _commit("security check: the deletion of the local roles of a member")
    check.compare(
        "deletion of the local roles of a member",
        {"change_seconds": round(time.monotonic() - started, 3)},
    )
    report["differ"] = sum(
        s["differ"] + len(s["searches_differ"]) + (s["searches"] - s["native"])
        for s in check.steps
    )
    with open(out, "w") as file:
        json.dump(report, file, indent=1)
    transaction.abort()
    return report


def _opaque_steps(app, site, check, members, leaves):
    """A local role provider for every Folder, which gives Reader to the
    second member on one leaf folder, and a setting that is a str on
    another leaf folder."""
    from borg.localrole.interfaces import ILocalRoleProvider
    from plone.app.contenttypes.interfaces import IFolder
    from zope.component import getGlobalSiteManager
    from zope.interface import implementer

    chosen = leaves[1]
    other = members[-1]

    @implementer(ILocalRoleProvider)
    class Provider:
        def __init__(self, context):
            self.context = context

        def _mine(self):
            return "/".join(self.context.getPhysicalPath()) == chosen

        def getRoles(self, principal):
            return ["Reader"] if self._mine() and principal == other else []

        def getAllRoles(self):
            if self._mine():
                yield other, ["Reader"]

    getGlobalSiteManager().registerAdapter(
        Provider, (IFolder,), ILocalRoleProvider, "security-check"
    )
    folder = app.unrestrictedTraverse(chosen)
    folder.reindexObjectSecurity()
    _commit("security check: a provider")
    check.compare("a local role provider for every Folder", {"folder": chosen})
    named = app.unrestrictedTraverse(leaves[2])
    named._Access_contents_information_Permission = "View"
    named.reindexObjectSecurity()
    _commit("security check: a str setting")
    check.compare("a setting that is a str", {"folder": leaves[2]})


def main(app, argv):
    out, *options = argv
    report = run(
        app, out, synthetic="--synthetic" in options, opaque="--opaque" in options
    )
    print(f"security check: {report['differ']} differences", flush=True)
    return 1 if report["differ"] else 0


if __name__ == "__main__":
    sys.exit(main(app, sys.argv[1:]))  # noqa: F821 - `run.py` gives `app`.
