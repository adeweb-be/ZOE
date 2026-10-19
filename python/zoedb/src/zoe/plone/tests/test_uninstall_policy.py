"""Tests of the merge policy of a site that uninstalls `zoe.plone`.

The import of the profile `zoe.plone:uninstall` switches off, for its
site, what the import of `zoe.plone:default` switched on: `folder_orders`,
`split_merges` and the `tier2` of the stock container classes. The site
asks for `off`, so the policy of the database takes `off` while the
database keeps the mode of that site, whatever the other sites ask for.

The layer is the one of `test_profile_one_folder`: it loads the ZCML of
`zoe.plone` and imports `zoe.plone:default` on the site of
`PLONE_FIXTURE`. The tests import the profiles with GenericSetup.

`just plone zoe backends -t test_uninstall_policy` runs this module.
"""

import unittest

import transaction
from plone.app.testing import TEST_USER_ID, applyProfile, setRoles
from zope.component.hooks import getSite, setSite

from zoe.plone.tests import test_profile_one_folder as alone

SWITCHES = ("folder_orders", "split_merges")
UNINSTALL = "zoe.plone:uninstall"


class TestUninstallPolicy(unittest.TestCase):
    layer = alone.PROFILE_FUNCTIONAL_TESTING
    maxDiff = None

    def setUp(self):
        self.app = self.layer["app"]
        self.portal = self.layer["portal"]
        setRoles(self.portal, TEST_USER_ID, ["Manager"])
        self.addCleanup(setSite, getSite())
        self.addCleanup(transaction.abort)

    def modes(self):
        """Return the switches and the `tier2` of the container classes in
        the policy of the database."""
        import zoe.policy

        policy = zoe.policy.read(self.portal._p_jar)
        found = {switch: policy.get(switch) for switch in SWITCHES}
        for name in alone.CONTAINERS:
            found[name] = policy["classes"][name]["tier2"]
        return found

    def expect(self, mode):
        return dict.fromkeys((*SWITCHES, *alone.CONTAINERS), mode)

    def apply(self, site, profile):
        setSite(site)
        try:
            applyProfile(site, profile)
        finally:
            setSite(self.portal)
        transaction.commit()

    def add_site(self, site_id="other"):
        from Products.CMFPlone.factory import addPloneSite

        site = getSite()
        try:
            addPloneSite(
                self.app, site_id, extension_ids=("plone.app.contenttypes:default",)
            )
        finally:
            setSite(site)
        transaction.commit()
        return self.app[site_id]

    def test_an_uninstall_switches_off_what_the_profile_switched_on(self):
        from zoe.plone import merges

        self.assertEqual(self.modes(), self.expect("on"))
        self.apply(self.portal, UNINSTALL)
        self.assertEqual(self.modes(), self.expect("off"))
        self.assertEqual(list(merges.site_overrides(self.portal)), [UNINSTALL])
        # The other classes keep their derived mode.
        import zoe.policy

        classes = zoe.policy.read(self.portal._p_jar)["classes"]
        self.assertEqual(classes[alone.DOCUMENT]["tier2"], "shadow")

    def test_another_site_keeps_off_while_the_uninstalled_site_asks_for_it(self):
        from zoe.plone import merges

        self.apply(self.portal, UNINSTALL)
        other = self.add_site()
        self.apply(other, alone.PROFILE)
        self.assertEqual(self.modes(), self.expect("off"))
        root = self.portal._p_jar.root()
        path = "/".join(self.portal.getPhysicalPath())
        for name in (*merges.SITE_SWITCHES.values(), *merges.SITE_CLASSES.values()):
            self.assertEqual(root[name][path], "off", name)
        # The uninstalled site installs the profile again: the policy rises.
        self.apply(self.portal, alone.PROFILE)
        self.assertEqual(self.modes(), self.expect("on"))
        self.assertEqual(list(merges.site_overrides(self.portal)), [alone.PROFILE])
