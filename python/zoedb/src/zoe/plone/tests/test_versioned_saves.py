"""Acceptance test of two concurrent saves of one versioned Document.

A save of a Document sets a field, then sends `ObjectModifiedEvent`, as the
edit form and the PATCH of plone.restapi do. plone.app.versioningbehavior
then saves a version: CMFEditions writes the new version into
`portal_historiesstorage` and sets `version_id` of the Document. Two saves
from one snapshot set two different fields. Tier 2 merges the fields of
the Document, but both saves also wrote `version_id`, to different values,
and both wrote a version into the history of the Document.

The test checks, on a Plone site of the functional layer of
plone.app.testing on a ZOE store, with the merge policy of `zoe.plone` in
mode `on` and queued indexing on:

- two saves of different fields of one Document, from one snapshot, both
  commit at the first attempt, and the Document holds both changes;
- the repository of CMFEditions then holds what the same two saves give
  one after the other: the same versions, each with the same version id,
  title and description, and the Document has the `version_id` of the last
  of them;
- two saves of one field from one snapshot still raise `ConflictError`:
  the fields stay strict (a guard; it passes today);
- without a merge policy, two saves of different fields raise
  `ConflictError` (a guard: the saves conflict).

The store. The layer keeps the site in a `DemoStorage`, which resolves
conflicts in Python with the resolver of ZODB, not with the tiers of the
store. So each run copies the committed records of the site to a
`zoe.Storage` of its own, as the acceptance test of concurrent creations
does, and the saves run there.

The catalog. A save reindexes the Document. Both saves change its entry in
the catalog, and the catalog refuses that. Queued indexing (ADR 0047)
takes the catalog out of the save, as on the stack of B5. So the test
switches it on with the helper of its acceptance test.

The merge policy. The test writes the policy of `zoe.plone` as a site with
a profile with overrides gets it (ADR 0046, section 9): the derived
entries, with `{"tier2": "on"}` as the overrides of one profile. So a
change of the derived policy reaches the test.

The layers of plone.app.testing need the interpreter of a Plone instance.
So this module is in the package, with the other tests of `zoe.plone` that
need a Plone site: `just plone zoe backends -t test_versioned_saves` runs
only this module.
"""

import shutil
import tempfile
import threading
import unittest

import transaction
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    PLONE_SITE_ID,
    TEST_USER_ID,
    TEST_USER_NAME,
    login,
    logout,
    setRoles,
)
from Testing.makerequest import makerequest
from ZODB.DB import DB
from ZODB.POSException import ConflictError
from zope.component.hooks import getSite, setSite
from zope.event import notify
from zope.globalrequest import clearRequest, getRequest, setRequest
from zope.lifecycleevent import Attributes, ObjectModifiedEvent

from zoe.plone.tests.test_concurrent_creations import copy_records, describe
from zoe.plone.tests.test_queued_indexing import drain, switch_on

# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60
# The id of the Document of the test.
DOCUMENT = "versioned"
# The saves of the tests: the field that each save sets, and its value.
TITLE = ("title", "The title of the first save")
DESCRIPTION = ("description", "The description of the second save")
OTHER_TITLE = ("title", "The title of the second save")


def merges_on(portal):
    """Write the merge policy of `zoe.plone` into the database of `portal`,
    with the derived entries in mode `on`: the site keeps the overrides
    `{"tier2": "on"}` as the overrides of one profile, and the policy is
    derived again. The caller commits."""
    from zoe.plone import merges

    merges.site_overrides(portal, create=True)["zoe.plone.tests"] = {"tier2": "on"}
    merges.update_policy(portal)


def save(document, field, value):
    """Save `field` of `document` with `value`, as the PATCH of plone.restapi
    does: set the field, then send `ObjectModifiedEvent` with it."""
    from plone.app.dexterity.behaviors.metadata import IBasic

    setattr(document, field, value)
    notify(ObjectModifiedEvent(document, Attributes(IBasic, f"IBasic.{field}")))


class TestVersionedSaves(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    maxDiff = None

    def setUp(self):
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        switch_on(self)
        # The creation saves the first version of the Document.
        portal.invokeFactory(
            "Document",
            DOCUMENT,
            title="The first title",
            description="The first description",
        )
        transaction.commit()
        history = portal.portal_repository.getHistoryMetadata(portal[DOCUMENT])
        self.assertTrue(history, "the Document has no version")
        # The copy takes the records, not the keys of the queue. So the
        # indexer applies the queue before the copy.
        drain(portal)

    def open_db(self):
        """Copy the site of the layer to a new `zoe.Storage`. Return a
        database on it, until the end of the test."""
        import zoe

        directory = tempfile.mkdtemp(prefix="zoe-versioned-saves-")
        self.addCleanup(shutil.rmtree, directory, ignore_errors=True)
        storage = zoe.Storage(f"{directory}/zoe")
        try:
            copy_records(self.layer["zodbDB"].storage, storage)
        except BaseException:
            storage.close()
            raise
        db = DB(storage, pool_size=4)
        self.addCleanup(db.close)
        return db

    def in_site(self, db, work):
        """Run `work(site)` on a connection of `db` in this thread, as the
        test user, and return what it returns. The transaction is aborted
        at the end, and the thread gets back its site and its request."""
        site_before, request_before = getSite(), getRequest()
        connection = db.open()
        try:
            app = makerequest(connection.root()["Application"])
            setRequest(app.REQUEST)
            site = app[PLONE_SITE_ID]
            setSite(site)
            login(site, TEST_USER_NAME)
            transaction.begin()
            return work(site)
        finally:
            transaction.abort()
            logout()
            clearRequest()
            connection.close()
            setSite(site_before)
            if request_before is not None:
                setRequest(request_before)

    def concurrent_saves(self, db, saves):
        """Run each save of `saves`, a `(field, value)` pair, in a thread of
        its own with a connection of its own, from one snapshot. Then the
        threads commit, one after the other, in the order of `saves`.
        Return the error of each save that raised, by its index."""
        changed = threading.Barrier(len(saves), timeout=TIMEOUT)
        done = [threading.Event() for _ in saves]
        errors = {}

        def run(turn, field, value):
            def work(site):
                save(site[DOCUMENT], field, value)
                # Each save changes the site from the snapshot of its
                # connection. Then the saves commit.
                changed.wait()
                if turn:
                    done[turn - 1].wait(TIMEOUT)
                transaction.commit()

            try:
                self.in_site(db, work)
            except BaseException as error:
                errors[turn] = error
            finally:
                done[turn].set()

        threads = [
            threading.Thread(target=run, args=(turn, field, value))
            for turn, (field, value) in enumerate(saves)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive())
        return errors

    def sequential_saves(self, db, saves):
        """Run each save of `saves` in a transaction of its own, one after
        the other."""
        for field, value in saves:

            def work(site, field=field, value=value):
                save(site[DOCUMENT], field, value)
                transaction.commit()

            self.in_site(db, work)

    def state(self, db):
        """Return the state of the Document of the site of `db`: its title,
        its description and its `version_id`, and each version of its
        history with its version id, title and description."""

        def work(site):
            document = site[DOCUMENT]
            repository = site.portal_repository
            history = repository.getHistoryMetadata(document)
            versions = []
            for selector in range(history.getLength(countPurged=True)):
                version = repository.retrieve(document, selector).object
                versions.append(
                    (
                        getattr(version, "version_id", None),
                        version.Title(),
                        version.Description(),
                    )
                )
            return {
                "document": (
                    document.Title(),
                    document.Description(),
                    getattr(document, "version_id", None),
                ),
                "versions": versions,
            }

        return self.in_site(db, work)

    def errors_text(self, errors):
        return {turn: describe(error) for turn, error in sorted(errors.items())}

    def test_two_saves_of_different_fields_commit_at_the_first_attempt(self):
        merges_on(self.layer["portal"])
        transaction.commit()
        db = self.open_db()
        errors = self.concurrent_saves(db, [TITLE, DESCRIPTION])
        self.assertEqual(self.errors_text(errors), {})
        title, description, _ = self.state(db)["document"]
        self.assertEqual((title, description), (TITLE[1], DESCRIPTION[1]))

    def test_the_repository_holds_what_the_saves_give_one_after_the_other(self):
        merges_on(self.layer["portal"])
        transaction.commit()
        reference = self.open_db()
        self.sequential_saves(reference, [TITLE, DESCRIPTION])
        expected = self.state(reference)
        # The reference: one version for each save, and the Document has
        # the version id of the last one.
        self.assertEqual(len(expected["versions"]), 3, expected)
        self.assertEqual(expected["document"][2], expected["versions"][-1][0])

        db = self.open_db()
        errors = self.concurrent_saves(db, [TITLE, DESCRIPTION])
        self.assertEqual(self.errors_text(errors), {})
        self.assertEqual(self.state(db), expected)

    def test_two_saves_of_one_field_raise_conflict_error(self):
        merges_on(self.layer["portal"])
        transaction.commit()
        db = self.open_db()
        errors = self.concurrent_saves(db, [TITLE, OTHER_TITLE])
        self.assertEqual(list(errors), [1], self.errors_text(errors))
        self.assertIsInstance(errors[1], ConflictError)
        self.assertEqual(self.state(db)["document"][0], TITLE[1])

    def test_without_a_merge_policy_the_saves_raise_conflict_error(self):
        db = self.open_db()
        errors = self.concurrent_saves(db, [TITLE, DESCRIPTION])
        self.assertEqual(list(errors), [1], self.errors_text(errors))
        self.assertIsInstance(errors[1], ConflictError)
