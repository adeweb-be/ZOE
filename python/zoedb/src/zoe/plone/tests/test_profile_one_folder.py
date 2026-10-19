"""Acceptance test of the order merge of a folder from the profile of
`zoe.plone` alone.

The acceptance test of concurrent creations in one folder
(`test_creations_in_one_folder`) writes its merge policy with
`zoe.policy.install`. This test writes none. The merge policy of its site
comes only from the import of the profile `zoe.plone:default`, as on a
site that installs `zoe.plone` and gives no overrides of its own.

The layer of the test loads the ZCML of `zoe.plone` and imports the
profile on the site of `PLONE_FIXTURE`. The layers of plone.app.testing
do not load the ZCML of an add-on, so the layer does it. The test gives no
keyword argument to `zoe.Storage`.

The test checks, on that site:

- the rounds and the checks of the acceptance test of concurrent
  creations in one folder: in each round, `CREATORS` creations of a
  Document in one folder, from one snapshot, all commit at the first
  attempt; the order holds each new item once, and `pos` agrees with the
  order; from another connection, each new item loads from the folder,
  and the catalog finds it;
- the policy that the profile writes: `split_merges` and `folder_orders`
  are `on`, and the tier 2 entry of each container class merges, with
  `modification_date` as `max`;
- the entry of each other content class keeps the mode that the
  derivation gives it: a Document merges in shadow mode only, so the
  record of a Document still conflicts in tier 2.

The container classes are the Dexterity container classes of the types of
the site: `plone.app.contenttypes.content.Folder` and the class of the
site root, `Products.CMFPlone.Portal.PloneSite`. The test checks that the
site has no other.

The tests of the rounds are those of the module
`test_creations_in_one_folder`. They call its helper `mechanism_on`. Each
test of this module replaces that helper with `profile_only`, which
writes no policy. So the rounds and the checks run unchanged, on the
policy of the profile.

`bin/test -s zoe.plone` of the instance of the zoe scenario runs it.
`just plone zoe backends -t test_profile_one_folder` runs only this
module.
"""

from unittest import mock

from plone.app.testing import (
    PLONE_FIXTURE,
    FunctionalTesting,
    PloneSandboxLayer,
    applyProfile,
)

from zoe.plone.tests import test_creations_in_one_folder as accept

# The profile of the test.
PROFILE = "zoe.plone:default"
# The Dexterity container classes of the types of a stock Plone 6.1 site.
CONTAINERS = (
    "plone.app.contenttypes.content.Folder",
    "Products.CMFPlone.Portal.PloneSite",
)
# The switches of tier 1 that the profile switches on.
SWITCHES = ("split_merges", "folder_orders")
# The class of a Document.
DOCUMENT = "plone.app.contenttypes.content.Document"


class ProfileFixture(PloneSandboxLayer):
    """The site of `PLONE_FIXTURE`, with the ZCML of `zoe.plone` loaded and
    the profile `PROFILE` imported."""

    defaultBases = (PLONE_FIXTURE,)

    def setUpZope(self, app, configurationContext):
        import zoe.plone

        self.loadZCML(package=zoe.plone)

    def setUpPloneSite(self, portal):
        applyProfile(portal, PROFILE)


PROFILE_FIXTURE = ProfileFixture()
PROFILE_FUNCTIONAL_TESTING = FunctionalTesting(
    bases=(PROFILE_FIXTURE,), name="zoe.plone:ProfileFunctional"
)


def profile_only(test):
    """Return the keyword arguments of `zoe.Storage` for the store of
    `test`: none. Write no merge policy: the policy of the site is the one
    that the import of the profile wrote."""
    return {}


def container_classes(portal):
    """Return the names of the classes of the Dexterity types of `portal`
    that are Dexterity containers."""
    from plone.dexterity.content import Container
    from plone.dexterity.interfaces import IDexterityFTI
    from plone.dexterity.utils import resolveDottedName

    found = set()
    for fti in portal.portal_types.objectValues():
        if not IDexterityFTI.providedBy(fti):
            continue
        klass = resolveDottedName(fti.klass)
        if issubclass(klass, Container):
            found.add(f"{klass.__module__}.{klass.__qualname__}")
    return found


class TestTheProfileAlone(accept.TestCreationsInOneFolder):
    layer = PROFILE_FUNCTIONAL_TESTING

    def setUp(self):
        from zoe.plone import merges

        patcher = mock.patch.object(accept, "mechanism_on", profile_only)
        patcher.start()
        self.addCleanup(patcher.stop)
        super().setUp()
        # The profile ran on the site, and its overrides are the only ones.
        kept = merges.site_overrides(self.layer["portal"])
        self.assertIsNotNone(kept)
        self.assertEqual(list(kept), [PROFILE])

    def policy(self):
        """Return the merge policy of the database of the site of the
        layer, and the entries that the derivation gives its classes."""
        import zoe.policy
        from zoe.plone import merges

        portal = self.layer["portal"]
        return zoe.policy.read(portal._p_jar), merges.derive(portal)["classes"]

    def test_the_container_classes_are_the_folder_and_the_site_root(self):
        self.assertEqual(container_classes(self.layer["portal"]), set(CONTAINERS))

    def test_the_profile_switches_on_the_container_classes_and_the_split_merge(
        self,
    ):
        policy, derived = self.policy()
        self.assertIsNotNone(policy)
        # One comparison, so a failure shows each switch and each entry.
        found = {switch: policy.get(switch) for switch in SWITCHES}
        expected = dict.fromkeys(SWITCHES, "on")
        for name in CONTAINERS:
            entry = policy["classes"].get(name, {})
            found[name] = (entry.get("tier2"), entry.get("fields", {}))
            if derived[name].get("blocked"):
                # A class that the derivation blocks stays blocked.
                expected[name] = ("off", derived[name].get("fields", {}))
            else:
                expected[name] = (
                    "on",
                    {**derived[name].get("fields", {}), "modification_date": "max"},
                )
        self.assertEqual(found, expected)

    def test_the_other_content_classes_keep_their_derived_mode(self):
        policy, derived = self.policy()
        self.assertIsNotNone(policy)
        found = {
            name: policy["classes"].get(name, {}).get("tier2")
            for name in derived
            if name not in CONTAINERS
        }
        expected = {
            name: entry["tier2"]
            for name, entry in derived.items()
            if name not in CONTAINERS
        }
        self.assertEqual(found, expected)
        self.assertEqual(found[DOCUMENT], "shadow")

    # The test of the acceptance module writes a policy without the order
    # merge. It is out of this module.
    test_with_the_mechanism_off_a_round_raises_conflict_error = None
