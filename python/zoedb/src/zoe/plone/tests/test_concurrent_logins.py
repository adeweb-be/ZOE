"""Acceptance test of concurrent logins on a ZOE store.

A login of a member in Plone 6.1 writes the times of the login. The login
form and the `@login` service of plone.restapi both call `loginUser` of
`portal_membership`, which calls `setLoginTimes`
(`Products/PlonePAS/tools/membership.py`). `setLoginTimes` sets the
properties `login_time` (now) and `last_login_time` (the `login_time`
before the login; now at the first login) of the member:

- The mutable property sheet of PAS (`acl_users/mutable_properties`, a
  `ZODBMutablePropertyProvider`) keeps the properties of each member in
  one plain dict, the value of the key of the member in its `OOBTree`
  `_storage`. `setPropertiesForUser` updates that dict and sets the key
  again, once for each of the two properties. So the login writes the
  bucket of `_storage` that holds the member, or the tree itself while it
  has one bucket.
- `notifyModified` of the member adapter then calls `registerMemberData`
  of `portal_memberdata`, which sets the key of the member in the
  `OOBTree` `_members` to its `MemberData` again. So the login writes the
  bucket of `_members` that holds the member too.

The record of `_members` stays the same, so two logins merge there. The
values of `_storage` are dicts, for each member and for each group. The
resolver of BTrees compares the values of each key with `<`, and a dict
does not support `<`. So it refuses any two changes of the bucket, also
two changes of different keys. ZOE gives the same answer. So with these
dicts, two logins from one snapshot raise `ConflictError` on `_storage`,
for one member and for two members, on ZOE as on ZEO.

The test checks, on a Plone site of the profile `zoe.plone:default` on a
ZOE store:

- two logins of one member from one snapshot both commit at the first
  attempt, for the first login of the member and for a later one;
- two logins of two members from one snapshot both commit at the first
  attempt, for their first logins and for later ones;
- after each round, a new connection reads the login times of each
  member: `login_time` is the `login_time` that the later login of the
  member wrote, and `last_login_time` is no later than it. For one
  member, `last_login_time` is a value that one of the two logins wrote,
  or the `login_time` of the earlier login, as two logins one after the
  other give. For two members, each member keeps the times that its own
  login wrote.

The keys of plone.protect. A login also runs `onUserLogsIn` of
plone.protect. It rotates each keyring of the site and of the Zope root
whose last rotation is older than a day (a week for `_system`), and it
makes the key manager of the Zope root when the root has none. Two such
logins from one snapshot conflict on the keyrings, which are
`PersistentList`s, and on the root. The CSRF keys stay in plone.keyring
(ADR 0043), so this test keeps the rotation out: a login of the test user
before the rounds rotates the keyrings, as on a site in use.

In each round, the logins call `setLoginTimes` in the order of their
turns, and then commit in the same order. So the later login also
commits later, as in a site, where the request that logs in later
commits later in almost every case.

The fix of the build. The store keeps the stock behaviour of a site on
ZEO until the build changes it. The build may still choose how:

- tier 2: the profile `zoe.plone:default` gives the class that holds the
  login times an entry, `max` for the two times and `strict` for the
  other properties. The dict of the properties of a member is no
  persistent object and tier 2 does not merge a class of tier 1, so this
  needs a persistent class of `zoe.plone` for the properties of a member
  or a group, as `zoe.plone.history.WorkflowHistory` is for a workflow
  history. While one value of the bucket stays a dict, the bucket does not
  merge;
- a KV namespace: a backend of `zoe.plone` keeps the login times in a
  durable namespace, where the later commit wins. A new namespace is a
  checkpoint for the human, as is a change of a public API.

The layer imports the profile, so the first choice needs no change of the
test. For the second choice, the build wires `mechanism_on`: it installs
the backend until the end of the test, and it returns the keyword
arguments of `zoe.Storage`, if the backend needs some. The human approves
a change of that helper.

The store. The layer keeps the site in a `DemoStorage`, which resolves
conflicts in Python, not with the tiers of the store. So each test copies
the committed records of the site to a `zoe.Storage` of its own, as the
acceptance test of concurrent creations in one folder does, and the
logins run there. The merges of ZOE run in the core, so the in-process
`zoe.Storage` and a client of `zoe serve` get the same ones.

`bin/test -s zoe.plone` of the instance of the zoe scenario runs it.
`just plone zoe backends -t test_concurrent_logins` runs only this module.
"""

import secrets
import threading
import unittest

import transaction
from plone import api
from plone.app.testing import (
    PLONE_SITE_ID,
    TEST_USER_ID,
    TEST_USER_NAME,
    login,
    logout,
    setRoles,
)
from Testing.makerequest import makerequest
from zope.component import getMultiAdapter
from zope.component.hooks import setSite
from zope.globalrequest import clearRequest, setRequest

from zoe.plone.tests import helpers
from zoe.plone.tests.test_profile_one_folder import PROFILE_FUNCTIONAL_TESTING

# The two members of the test.
MEMBERS = ("member-one", "member-two")
# The password of the members. Each run makes a new one.
PASSWORD = secrets.token_urlsafe(18)
# The rounds of a test: the first login of each member, then a later one.
ROUNDS = 2
# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60
# The connections of the database of a test: one for each login, one for
# the test, and one for the reads after a round.
POOL_SIZE = 4


def mechanism_on(test):
    """Switch on the mechanism of concurrent logins, until the end of
    `test`. Return the keyword arguments of `zoe.Storage` for the store of
    the test.

    The test calls it once, before it copies the site of the layer to its
    store. So the copy takes a change that the helper commits to the site
    of the layer.

    The layer imports the profile `zoe.plone:default`, so the helper does
    nothing until the build gives it a backend to install. The human
    approves a change of this helper.
    """
    return {}


def log_in(site, member):
    """Log `member` in on `site`, as the login form of Plone does after it
    checked the password: the view `login` runs its steps after a login,
    which call `loginUser` of `portal_membership`. Return the login times
    of the member after the login, as `(login_time, last_login_time)`."""
    login(site, member)
    view = getMultiAdapter((site, site.REQUEST), name="login")
    view._post_login()
    return times(site.portal_membership.getAuthenticatedMember())


def times(member):
    """Return the login times of the member data `member`, as
    `(login_time, last_login_time)`."""
    return member.getProperty("login_time"), member.getProperty("last_login_time")


def times_of(site, member):
    """Return the login times of the member with the id `member` on
    `site`."""
    return times(site.portal_membership.getMemberById(member))


class TestConcurrentLogins(unittest.TestCase):
    layer = PROFILE_FUNCTIONAL_TESTING
    # A failure shows the error of each turn of each round.
    maxDiff = None

    def setUp(self):
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        # The members of a site, as the fixture of the benchmark makes
        # them: an email, a password and a full name.
        for member in MEMBERS:
            api.user.create(
                email=f"{member}@example.invalid",
                username=member,
                password=PASSWORD,
                properties={"fullname": member},
            )
        transaction.commit()
        # A login of another user first, as on a site in use. It makes the
        # key manager of the Zope root and rotates the keyrings, so the
        # logins of the test do not.
        log_in(portal, TEST_USER_NAME)
        transaction.commit()
        logout()

    def open_site(self, options):
        """Copy the site of the layer to a new `zoe.Storage`, opened with
        the keyword arguments `options`. Return the site on that store, on
        a connection of this thread, until the end of the test."""
        return helpers.open_site(self, options, "zoe-logins-", POOL_SIZE)

    def login_round(self, accounts):
        """Run one round: one thread for each account of `accounts`, each
        with a connection of its own, logs that account in, from one
        snapshot. The threads log in in the order of their turns, then
        commit in the same order. Return the errors of the turns that
        raised, by turn, and the login times that each turn wrote, by
        turn."""
        db = self.db
        count = len(accounts)
        changed = threading.Barrier(count, timeout=TIMEOUT)
        logged_in = [threading.Event() for _ in range(count)]
        done = [threading.Event() for _ in range(count)]
        errors = {}
        written = {}

        def run(turn):
            connection = db.open()
            try:
                app = makerequest(connection.root()["Application"])
                setRequest(app.REQUEST)
                site = app[PLONE_SITE_ID]
                setSite(site)
                if turn:
                    logged_in[turn - 1].wait(TIMEOUT)
                try:
                    written[turn] = log_in(site, accounts[turn])
                finally:
                    logged_in[turn].set()
                # Each login changes the site from the snapshot of its
                # connection. Then the logins commit.
                changed.wait()
                if turn:
                    done[turn - 1].wait(TIMEOUT)
                transaction.commit()
            except BaseException as error:
                errors[turn] = error
            finally:
                done[turn].set()
                transaction.abort()
                logout()
                setSite(None)
                clearRequest()
                connection.close()

        threads = [threading.Thread(target=run, args=(turn,)) for turn in range(count)]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive())
        return errors, written

    def stored_times(self):
        """Return the login times of each member, by member, from a new
        connection."""
        found = {}

        def read():
            connection = self.db.open()
            try:
                site = connection.root()["Application"][PLONE_SITE_ID]
                setSite(site)
                for member in MEMBERS:
                    found[member] = times_of(site, member)
            finally:
                transaction.abort()
                setSite(None)
                connection.close()

        thread = threading.Thread(target=read)
        thread.start()
        thread.join(TIMEOUT)
        self.assertFalse(thread.is_alive())
        return found

    def rounds(self, accounts):
        """Run `ROUNDS` rounds of logins of `accounts`. Return the errors of
        each round that had one, as text, by round and turn, and the login
        times that each round wrote and then stored."""
        failed = {}
        history = []
        for number in range(ROUNDS):
            before = self.stored_times()
            errors, written = self.login_round(accounts)
            if errors:
                failed[number] = {
                    turn: helpers.describe(error)
                    for turn, error in sorted(errors.items())
                }
            history.append((before, written, self.stored_times()))
        return failed, history

    def test_two_logins_of_one_member_commit_at_the_first_attempt(self):
        self.open_site(mechanism_on(self))
        failed, _ = self.rounds((MEMBERS[0], MEMBERS[0]))
        self.assertEqual(failed, {})

    def test_two_logins_of_two_members_commit_at_the_first_attempt(self):
        self.open_site(mechanism_on(self))
        failed, _ = self.rounds(MEMBERS)
        self.assertEqual(failed, {})

    def test_one_member_keeps_the_time_of_the_later_login(self):
        self.open_site(mechanism_on(self))
        member = MEMBERS[0]
        failed, history = self.rounds((member, member))
        self.assertEqual(failed, {})
        for number, (_, written, stored) in enumerate(history):
            (first, first_last), (second, second_last) = written[0], written[1]
            self.assertLessEqual(first, second, f"round {number}")
            login_time, last_login_time = stored[member]
            self.assertEqual(login_time, second, f"round {number}")
            self.assertLessEqual(last_login_time, login_time, f"round {number}")
            # The value of a login, or the value that the second login
            # writes when it comes after the first.
            self.assertIn(
                last_login_time, (first_last, second_last, first), f"round {number}"
            )

    def test_two_members_keep_the_times_of_their_logins(self):
        self.open_site(mechanism_on(self))
        failed, history = self.rounds(MEMBERS)
        self.assertEqual(failed, {})
        for number, (_, written, stored) in enumerate(history):
            self.assertEqual(
                stored,
                {member: written[turn] for turn, member in enumerate(MEMBERS)},
                f"round {number}",
            )
