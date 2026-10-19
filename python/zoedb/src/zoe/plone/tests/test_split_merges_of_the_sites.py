"""Tests of the switch `split_merges` of the sites of one database.

The profile `zoe.plone:default` switches `split_merges` on, as it switches
`folder_orders` on. So `update_policy` composes both switches in the same
way: the database keeps the latest mode that each site asked for, and the
policy takes the lowest of them (ADR 0052, section 6). The database keeps
the modes of `split_merges` in `root['zoe.plone.split_merges']`.

The tests are those of `test_folder_orders_of_the_sites`, on the switch
`split_merges`. Each test replaces the switch of that module, and reads
the modes of `split_merges`.

`just plone zoe backends -t test_split_merges_of_the_sites` runs this
module.
"""

from unittest import mock

from zoe.plone.tests import test_folder_orders_of_the_sites as sites

# The switch of the tests.
SWITCH = "split_merges"


class TestSplitMergesOfTheSites(sites.TestFolderOrdersOfTheSites):
    def setUp(self):
        patcher = mock.patch.object(sites, "SWITCH", SWITCH)
        patcher.start()
        self.addCleanup(patcher.stop)
        super().setUp()

    def modes(self):
        """Return the mapping `root['zoe.plone.split_merges']`."""
        from zoe.plone import merges

        return self.portal._p_jar.root()[merges.SITE_SWITCHES[SWITCH]]
