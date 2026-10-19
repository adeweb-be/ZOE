"""Acceptance tests of the switch `folder_orders` of the sites of one
database (ADR 0052, section 6).

The database keeps the latest mode of `folder_orders` that each of its
sites asked for, in `root['zoe.plone.folder_orders']`, by the path of the
site. `zoe.plone.merges.update_policy` gives the policy the lowest of these
modes. The tests check, on Plone sites of the functional layer of
plone.app.testing:

- an `off` that `zoe.policy.install` writes, and that no site asks for, is
  replaced at the next `update_policy` of a site, after an edit of a
  Dexterity type; the update logs a warning that names the switch and the
  stored mode; an update that does not lower the stored mode logs no
  warning about the switch;
- a removed site, or a renamed site, leaves no entry under its old path
  after the next `update_policy`; so the policy rises when no other site
  asks for less, and no warning names the old path;
- two sites in one database compose to the lowest latest mode, through
  `update_policy`;
- an `update_policy` that changes no mode does not write the record of
  `root['zoe.plone.folder_orders']`.

The layers of plone.app.testing load no ZCML of `zoe.plone`. So each test
registers the subscribers of `zoe.plone.merges` that the ZCML files of
`zoe.plone` name, as Zope does when it loads them (`merges_subscribers`).

Each test gives the overrides of the profiles of a site directly, under
the profile id `PROFILE`, as the import step keeps them. So the tests do
not read the overrides file of the profile `zoe.plone:default`.

`just plone zoe backends -t test_folder_orders_of_the_sites` runs this
module.
"""

import importlib
import logging
import pathlib
import re
import unittest
import xml.etree.ElementTree as ElementTree

import transaction
from AccessControl.SecurityManagement import noSecurityManager
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    SITE_OWNER_NAME,
    TEST_USER_ID,
    setRoles,
)
from plone.testing import zope as zope_testing
from ZODB.utils import load_current
from zope.component import getGlobalSiteManager
from zope.component.hooks import getSite, setSite
from zope.event import notify
from zope.lifecycleevent import ObjectModifiedEvent

# The switch of the tests.
SWITCH = "folder_orders"
# The id of the second site of the database.
OTHER = "other"
# The new id of the second site, after a rename.
RENAMED = "renamed"
# The profile id under which a test gives the overrides of a site.
PROFILE = "profile-site:default"


def resolve(name):
    """Return the object of the dotted name `name`, relative to
    `zoe.plone` when it starts with a dot."""
    if name.startswith("."):
        name = "zoe.plone" + name
    module, _, attribute = name.rpartition(".")
    return getattr(importlib.import_module(module), attribute)


def merges_subscribers(test):
    """Register each subscriber that the ZCML files of `zoe.plone` name
    with a handler of `zoe.plone.merges`, until the end of `test`, as Zope
    does when it loads them. Return their number."""
    import zoe.plone

    registry = getGlobalSiteManager()
    found = 0
    for path in sorted(pathlib.Path(zoe.plone.__file__).parent.glob("*.zcml")):
        for element in ElementTree.parse(path).iter():
            if not element.tag.endswith("subscriber"):
                continue
            name = element.get("handler") or ""
            if not name.lstrip(".").startswith(("merges.", "zoe.plone.merges.")):
                continue
            handler = resolve(name)
            # Without `for`, the handler declares what it adapts.
            required = element.get("for")
            if required is not None:
                required = tuple(resolve(item) for item in required.split())
            registry.registerHandler(handler, required)
            test.addCleanup(registry.unregisterHandler, handler, required)
            found += 1
    return found


def warnings_about(records, word):
    """Return the messages of `records` at WARNING or above that name
    `word`."""
    return [
        record.getMessage()
        for record in records
        if record.levelno >= logging.WARNING and word in record.getMessage()
    ]


class TestFolderOrdersOfTheSites(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    maxDiff = None

    def setUp(self):
        self.app = self.layer["app"]
        self.portal = self.layer["portal"]
        setRoles(self.portal, TEST_USER_ID, ["Manager"])
        self.addCleanup(setSite, getSite())
        self.addCleanup(transaction.abort)
        setSite(self.portal)
        self.assertTrue(
            merges_subscribers(self), "zoe.plone.merges subscribes to no event"
        )

    def ask(self, site, mode):
        """Give `site` the overrides of `PROFILE` with `mode` for `SWITCH`,
        and update the policy of the database from it. Commit."""
        from zoe.plone import merges

        merges.site_overrides(site, create=True)[PROFILE] = {SWITCH: mode}
        merges.update_policy(site)
        transaction.commit()

    def stored_mode(self):
        """Return the mode of `SWITCH` of the policy of the database."""
        import zoe.policy

        return zoe.policy.read(self.portal._p_jar).get(SWITCH)

    def modes(self):
        """Return the mapping `root['zoe.plone.folder_orders']`."""
        from zoe.plone import merges

        return self.portal._p_jar.root()[merges.SITE_MODES]

    def path_of(self, site):
        return "/".join(site.getPhysicalPath())

    def add_site(self, site_id=OTHER):
        """Add a second Plone site to the Zope root, and commit. Return
        it."""
        from Products.CMFPlone.factory import addPloneSite

        site = getSite()
        try:
            addPloneSite(self.app, site_id)
        finally:
            setSite(site)
        transaction.commit()
        return self.app[site_id]

    def edit_type(self, site):
        """Change the title of the type Document of `site`, and notify the
        change, as an edit of the type in Plone does."""
        setSite(site)
        try:
            fti = site.portal_types["Document"]
            fti.title = fti.title + " (edited)"
            notify(ObjectModifiedEvent(fti))
        finally:
            setSite(self.portal)

    def test_an_off_of_install_that_an_update_replaces_logs_a_warning(self):
        import zoe.policy

        self.ask(self.portal, "on")
        self.assertEqual(self.stored_mode(), "on")
        # The operator switches the order merge off for the database, with
        # `zoe.policy.install`, and no site asks for it.
        policy = zoe.policy.read(self.portal._p_jar)
        del policy["version"]
        policy[SWITCH] = "off"
        zoe.policy.install(self.portal._p_jar, policy)
        transaction.commit()
        self.assertEqual(self.stored_mode(), "off")
        # An edit of a Dexterity type updates the policy from the site,
        # which asks for on. The update replaces the off, with a warning
        # that names the switch and the stored mode.
        with self.assertLogs("zoe.plone", logging.INFO) as logs:
            self.edit_type(self.portal)
        transaction.commit()
        self.assertEqual(self.stored_mode(), "on")
        found = warnings_about(logs.records, SWITCH)
        self.assertTrue(
            any(re.search(r"\boff\b", message) for message in found),
            f"no warning names {SWITCH} and the stored mode off: {found}",
        )
        # The stored mode is on now: the next update does not lower it and
        # logs no warning about the switch.
        with self.assertLogs("zoe.plone", logging.INFO) as logs:
            self.edit_type(self.portal)
        transaction.commit()
        self.assertEqual(warnings_about(logs.records, SWITCH), [])

    def test_a_removed_site_leaves_no_mode(self):
        from zoe.plone import merges

        other = self.add_site()
        gone = self.path_of(other)
        self.ask(other, "off")
        self.ask(self.portal, "on")
        self.assertEqual(self.stored_mode(), "off")
        self.app._delObject(OTHER)
        transaction.commit()
        with self.assertLogs("zoe.plone", logging.INFO) as logs:
            merges.update_policy(self.portal)
        transaction.commit()
        self.assertEqual(dict(self.modes()), {self.path_of(self.portal): "on"})
        self.assertEqual(self.stored_mode(), "on")
        self.assertEqual(warnings_about(logs.records, gone), [])

    def test_a_renamed_site_leaves_no_mode_under_its_old_path(self):
        from zoe.plone import merges

        other = self.add_site()
        old = self.path_of(other)
        self.ask(other, "off")
        self.ask(self.portal, "on")
        zope_testing.login(self.app["acl_users"], SITE_OWNER_NAME)
        self.addCleanup(noSecurityManager)
        self.app.manage_renameObject(OTHER, RENAMED)
        transaction.commit()
        renamed = self.app[RENAMED]
        new = self.path_of(renamed)
        with self.assertLogs("zoe.plone", logging.INFO) as logs:
            merges.update_policy(self.portal)
        transaction.commit()
        self.assertNotIn(old, self.modes())
        self.assertEqual(warnings_about(logs.records, old), [])
        # The renamed site keeps its overrides: its next update gives its
        # mode under its new path, and the policy takes it.
        merges.update_policy(renamed)
        transaction.commit()
        self.assertEqual(
            dict(self.modes()), {self.path_of(self.portal): "on", new: "off"}
        )
        self.assertEqual(self.stored_mode(), "off")

    def test_two_sites_compose_to_the_lowest_latest_mode(self):
        portal = self.path_of(self.portal)
        other = self.add_site()
        path = self.path_of(other)
        self.ask(self.portal, "on")
        self.assertEqual(self.stored_mode(), "on")
        # The other site asks for less: the policy takes it.
        self.ask(other, "shadow")
        self.assertEqual(self.stored_mode(), "shadow")
        # An update of the first site keeps the lower mode of the other,
        # with a warning that names the other site.
        with self.assertLogs("zoe.plone", logging.WARNING) as logs:
            self.edit_type(self.portal)
        transaction.commit()
        self.assertEqual(self.stored_mode(), "shadow")
        self.assertTrue(warnings_about(logs.records, path), logs.output)
        # The first site switches off: the lowest mode is off.
        self.ask(self.portal, "off")
        self.assertEqual(self.stored_mode(), "off")
        self.assertEqual(dict(self.modes()), {portal: "off", path: "shadow"})
        # Both sites raise their modes: the policy rises with the last one.
        self.ask(other, "on")
        self.assertEqual(self.stored_mode(), "off")
        self.ask(self.portal, "on")
        self.assertEqual(self.stored_mode(), "on")
        self.assertEqual(dict(self.modes()), {portal: "on", path: "on"})
        # The classes of the policy are the classes of both sites.
        import zoe.policy

        classes = zoe.policy.read(self.portal._p_jar)["classes"]
        self.assertIn("plone.app.contenttypes.content.Folder", classes)

    def test_an_update_that_changes_no_mode_does_not_write_the_modes(self):
        from zoe.plone import merges

        self.ask(self.portal, "on")
        modes = self.modes()
        serial = modes._p_serial
        storage = self.portal._p_jar.db().storage
        merges.update_policy(self.portal)
        self.assertFalse(modes._p_changed)
        transaction.commit()
        self.assertEqual(modes._p_serial, serial)
        self.assertEqual(load_current(storage, modes._p_oid)[1], serial)
        # An edit of a type, with the same overrides, does not write it
        # either.
        self.edit_type(self.portal)
        self.assertFalse(modes._p_changed)
        transaction.commit()
        self.assertEqual(load_current(storage, modes._p_oid)[1], serial)
        # A change of the mode writes it.
        self.ask(self.portal, "shadow")
        self.assertNotEqual(load_current(storage, modes._p_oid)[1], serial)
        self.assertEqual(dict(self.modes()), {self.path_of(self.portal): "shadow"})
