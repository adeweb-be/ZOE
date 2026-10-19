"""Tests of the properties of the members and groups of `zoe.plone` in a
Plone site (ADR 0046, section 9).

The import of the profile `zoe.plone:default` gives each value of
`_storage` of the mutable property sheet of PAS the class
`zoe.plone.properties.PrincipalProperties`, groups included, and marks the
sheet. A write of the properties of a new principal then stores the
class, a read writes nothing, and the code of Plone that reads the
properties works as with dicts. The import of `zoe.plone:uninstall` gives
the dicts back.

The layer is the one of `test_profile_one_folder`: it loads the ZCML of
`zoe.plone` and imports `zoe.plone:default` on the site of
`PLONE_FIXTURE`.

`just plone zoe backends -t test_principal_properties` runs this module.
"""

import unittest

import transaction
from plone import api
from plone.app.testing import TEST_USER_ID, applyProfile, setRoles

from zoe.plone import properties
from zoe.plone.properties import PrincipalProperties
from zoe.plone.tests import test_profile_one_folder as alone

MEMBER = "member-one"
GROUP = "group-one"


class TestPrincipalProperties(unittest.TestCase):
    layer = alone.PROFILE_FUNCTIONAL_TESTING

    def setUp(self):
        self.portal = self.layer["portal"]
        setRoles(self.portal, TEST_USER_ID, ["Manager"])
        self.addCleanup(transaction.abort)
        self.sheet = self.portal.acl_users.mutable_properties
        self.storage = self.sheet._storage

    def add_principals(self):
        api.user.create(
            email=f"{MEMBER}@example.invalid",
            username=MEMBER,
            password="a-password-of-the-test",
            properties={"fullname": "Member One"},
        )
        api.group.create(groupname=GROUP, title="Group One")
        transaction.commit()

    def kinds(self):
        return {key: type(value) for key, value in self.storage.items()}

    def test_the_profile_moves_every_value_groups_included(self):
        self.assertTrue(properties.marked(self.sheet))
        kinds = self.kinds()
        self.assertTrue(kinds)
        self.assertEqual(set(kinds.values()), {PrincipalProperties})
        self.assertTrue(any(value.get("isGroup") for value in self.storage.values()))

    def test_a_new_member_and_a_new_group_get_the_class(self):
        self.add_principals()
        self.assertIs(type(self.storage[MEMBER]), PrincipalProperties)
        self.assertIs(type(self.storage[GROUP]), PrincipalProperties)
        self.assertEqual(self.storage[MEMBER]["fullname"], "Member One")
        self.assertEqual(self.storage[GROUP]["title"], "Group One")
        member = api.user.get(username=MEMBER)
        member.setMemberProperties({"fullname": "Member Renamed"})
        transaction.commit()
        self.assertEqual(
            api.user.get(username=MEMBER).getProperty("fullname"), "Member Renamed"
        )

    def test_a_read_of_the_properties_writes_nothing(self):
        self.add_principals()
        # Properties without the defaults, as a site with an older schema
        # keeps them.
        self.storage[MEMBER] = PrincipalProperties({"fullname": "Member One"})
        transaction.commit()
        storage = self.layer["zodbDB"].storage
        before = storage.lastTransaction()
        member = api.user.get(username=MEMBER)
        self.assertEqual(member.getProperty("fullname"), "Member One")
        sheet = self.sheet.getPropertiesForUser(member.getUser())
        self.assertEqual(sheet.getProperty("email"), "")
        self.assertFalse(self.storage[MEMBER]._p_changed)
        transaction.commit()
        self.assertEqual(storage.lastTransaction(), before)
        self.assertEqual(dict(self.storage[MEMBER]), {"fullname": "Member One"})

    def test_the_searches_and_the_members_read_the_properties(self):
        self.add_principals()
        found = self.portal.acl_users.searchUsers(fullname="Member One")
        self.assertIn(MEMBER, [entry["userid"] for entry in found])
        found = self.portal.portal_membership.searchForMembers(name="Member One")
        self.assertIn(MEMBER, [member.getId() for member in found])
        self.assertEqual(api.group.get(groupname=GROUP).getProperty("title"), "Group One")
        # plone.restapi and plone.app.users read each property through
        # `getProperty` of the member.
        user = api.user.get(username=MEMBER)
        self.assertEqual(user.getProperty("fullname"), "Member One")
        self.assertEqual(user.getProperty("email"), f"{MEMBER}@example.invalid")

    def test_a_login_writes_the_times_into_the_record_of_the_member(self):
        from DateTime import DateTime
        from plone.app.testing import login, logout

        self.add_principals()
        record = self.storage[MEMBER]
        login(self.portal, MEMBER)
        try:
            self.portal.portal_membership.setLoginTimes()
        finally:
            logout()
        self.assertTrue(record._p_changed)
        self.assertIs(self.storage[MEMBER], record)
        transaction.commit()
        self.assertGreater(record["login_time"], DateTime("2000/01/02"))

    def test_the_uninstall_gives_the_dicts_back(self):
        self.add_principals()
        applyProfile(self.portal, "zoe.plone:uninstall")
        transaction.commit()
        self.assertFalse(properties.marked(self.sheet))
        self.assertEqual(set(self.kinds().values()), {dict})
        self.assertEqual(self.storage[MEMBER]["fullname"], "Member One")
        # A new member keeps the stock dict.
        api.user.create(
            email="member-two@example.invalid",
            username="member-two",
            password="a-password-of-the-test",
        )
        transaction.commit()
        self.assertIs(type(self.storage["member-two"]), dict)
        # A new import of the profile moves them again.
        applyProfile(self.portal, alone.PROFILE)
        transaction.commit()
        self.assertTrue(properties.marked(self.sheet))
        self.assertEqual(set(self.kinds().values()), {PrincipalProperties})

    def test_the_policy_of_the_database_holds_the_entry(self):
        import zoe.policy

        entry = zoe.policy.read(self.portal._p_jar)["classes"][
            "zoe.plone.properties.PrincipalProperties"
        ]
        self.assertEqual(entry["tier2"], "on")
        self.assertEqual(entry["default"], "strict")
        self.assertEqual(entry["fields"], properties.ENTRY["fields"])
