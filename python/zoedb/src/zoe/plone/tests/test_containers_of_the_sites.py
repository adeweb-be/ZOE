"""Tests of the `tier2` of the stock container classes of the sites of one
database.

The profile `zoe.plone:default` switches on the entries of the stock
container classes. So `update_policy` composes their `tier2` as it
composes `folder_orders` and `split_merges`: the database keeps the latest
mode that each site asked for, and the policy takes the lowest of them.
So an import of the profile on one site does not raise a container class
over the lower mode of another site.

The tests use the helpers of `test_folder_orders_of_the_sites`. Each site
gets the overrides of the file of the profile under `PROFILE`, and the
overrides of a policy package of its own under `LOCAL`.

`just plone zoe backends -t test_containers_of_the_sites` runs this
module.
"""

import json
import logging

import transaction

from zoe.plone.tests import helpers
from zoe.plone.tests import test_folder_orders_of_the_sites as sites

# The profile id of the overrides of the profile of `zoe.plone`.
PROFILE = "zoe.plone:default"
# The profile id of the overrides of a policy package of a site.
LOCAL = "profile-site:default"
FOLDER = "plone.app.contenttypes.content.Folder"
SITE_ROOT = "Products.CMFPlone.Portal.PloneSite"


def profile_overrides():
    """Return the overrides of the file of the profile `PROFILE`."""
    import importlib.resources

    from zoe.plone import merges

    path = importlib.resources.files("zoe.plone") / "profiles" / "default"
    return json.loads((path / merges.OVERRIDES).read_text())


class TestContainersOfTheSites(sites.TestFolderOrdersOfTheSites):
    # The tests of the switch `folder_orders` run in their own module.
    test_an_off_of_install_that_an_update_replaces_logs_a_warning = None
    test_a_removed_site_leaves_no_mode = None
    test_a_renamed_site_leaves_no_mode_under_its_old_path = None
    test_two_sites_compose_to_the_lowest_latest_mode = None
    test_an_update_that_changes_no_mode_does_not_write_the_modes = None

    def add_site(self, site_id=sites.OTHER):
        """Add a second Plone site with the types of plone.app.contenttypes
        to the Zope root, as the site of the layer has them, and commit.
        Return it."""
        return helpers.add_site(
            self.app, site_id, extension_ids=("plone.app.contenttypes:default",)
        )

    def give(self, site, classes=None):
        """Give `site` the overrides of the profile, and the entries
        `classes` of a policy package of its own when not None. Update the
        policy of the database from it. Commit."""
        from zoe.plone import merges

        kept = merges.site_overrides(site, create=True)
        kept[PROFILE] = profile_overrides()
        if classes is None:
            kept.pop(LOCAL, None)
        else:
            kept[LOCAL] = {"classes": classes}
        merges.update_policy(site)
        transaction.commit()

    def tier2(self, name):
        """Return the `tier2` of the class `name` in the policy of the
        database."""
        import zoe.policy

        return zoe.policy.read(self.portal._p_jar)["classes"][name]["tier2"]

    def class_modes(self, name):
        """Return the modes that the sites asked for the class `name`."""
        from zoe.plone import merges

        return dict(self.portal._p_jar.root()[merges.SITE_CLASSES[name]])

    def test_an_import_on_another_site_keeps_the_lower_container(self):
        portal = self.path_of(self.portal)
        other = self.add_site()
        path = self.path_of(other)
        self.assertIn("Folder", other.portal_types)
        # The first site lowers the folders in its policy package.
        self.give(self.portal, {FOLDER: {"tier2": "shadow"}})
        self.assertEqual(self.tier2(FOLDER), "shadow")
        self.assertEqual(self.tier2(SITE_ROOT), "on")
        # The other site imports the profile alone: the folders stay in
        # shadow mode, with a warning that names the first site.
        with self.assertLogs("zoe.plone", logging.WARNING) as logs:
            self.give(other)
        self.assertEqual(self.tier2(FOLDER), "shadow")
        self.assertEqual(self.tier2(SITE_ROOT), "on")
        self.assertTrue(sites.warnings_about(logs.records, portal), logs.output)
        self.assertEqual(self.class_modes(FOLDER), {portal: "shadow", path: "on"})
        # An edit of a type of the other site keeps them too.
        self.edit_type(other)
        transaction.commit()
        self.assertEqual(self.tier2(FOLDER), "shadow")
        # The first site drops its lower mode: the folders merge.
        self.give(self.portal)
        self.assertEqual(self.tier2(FOLDER), "on")
        self.assertEqual(self.class_modes(FOLDER), {portal: "on", path: "on"})

    def test_a_site_that_lowers_a_container_lowers_the_database(self):
        other = self.add_site()
        self.give(self.portal)
        self.give(other)
        self.assertEqual(self.tier2(SITE_ROOT), "on")
        # A later import on the other site lowers the class for the
        # database, and keeps the derived fields.
        self.give(other, {SITE_ROOT: {"tier2": "off"}})
        self.assertEqual(self.tier2(SITE_ROOT), "off")
        self.give(self.portal)
        self.assertEqual(self.tier2(SITE_ROOT), "off")
        import zoe.policy

        entry = zoe.policy.read(self.portal._p_jar)["classes"][SITE_ROOT]
        self.assertEqual(entry["fields"]["modification_date"], "max")
