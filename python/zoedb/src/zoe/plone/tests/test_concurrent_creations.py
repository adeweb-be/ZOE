"""Acceptance test of concurrent creations.

Each creation of a versioned Document inserts keys into trees that all the
creations of a site share:

- CMFEditions inserts the history id of the Document into
  `_shadowStorage._storage` of `portal_historiesstorage`. The id comes
  from `portal_uidgenerator`, so it grows.
- CMFEditions inserts a random key into `zvc_repo._histories` of
  `portal_historiesstorage`.
- The intid utility inserts the key reference of the Document into `ids`.
  The key reference sorts by oid, so it grows. It inserts the intid into
  `refs`.

A key that grows goes into the bucket at the right end of its tree. When
the insert of one creation splits a bucket, the store refuses each other
creation that changed the same node of the tree from the same snapshot,
and that creation raises `ConflictError`.

The test checks, on a Plone site of the functional layer of
plone.app.testing on a ZOE store, with queued indexing on:

- in each of `ROUNDS` rounds, `CREATORS` transactions that each create a
  versioned Document, from one snapshot, all commit at the first attempt;
- the history ids stay unique, and so do the intids;
- each version of each Document loads through `portal_repository`;
- after the indexer runs, the catalog equals a reindex from scratch;
- with the mechanism off, the same rounds raise `ConflictError`.

The rounds. A creation conflicts on a tree only when an insert splits a
bucket. From one snapshot, each creation of a round finds the same bucket
at the right end of a tree that grows, so either each of their inserts
splits it, or none does. Each round adds its Documents to the trees. So
the buckets fill and split during the rounds, and the test does not read
the buckets to make that happen.

The store. The layer keeps the site in a `DemoStorage`. A `DemoStorage`
gives no sequences, so `portal_uidgenerator` gives each creation of a
round the same history id. And it resolves conflicts in Python with the
resolver of ZODB, not with the tiers of the store. So each test copies the
committed records of the site to a `zoe.Storage` of its own, and the
rounds run there.

The folders. Each creator creates its Documents in a folder of its own,
as each user of the scenario `create-spread` of B5 does in the folders of
its own section. A creation reindexes its container in full, and that
sets `modification_date` of the container. In one folder, the record of
the folder then conflicts in every round.

The ids. `zoe.plone` gives `portal_uidgenerator` its ids from a sequence
of the store when Zope opens its database, on every ZOE stack of B5. The
layers of plone.testing do not send that event, so the test wires the ids
itself.

The ADR of the card names the mechanism. Only the helpers `mechanism_on`
and `mechanism_off` name it. The helpers `switch_on` and `drain` of the
acceptance test of queued indexing switch on queued indexing and run its
indexer, as ADR 0047 names them.

The layers of plone.app.testing need the interpreter of a Plone instance.
So this module is in the package, with the other tests of `zoe.plone` that
need a Plone site: `bin/test -s zoe.plone` of the instance of the zoe
scenario runs it. `just plone zoe backends` runs it, and `just plone zoe
backends -t test_concurrent_creations` runs only this module.
"""

import unittest

import transaction
from Acquisition import aq_base
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    TEST_USER_ID,
    TEST_USER_NAME,
    setRoles,
)
from ZODB.POSException import ConflictError

from zoe.plone.tests import helpers
from zoe.plone.tests.test_queued_indexing import (
    catalog_state,
    drain,
    path_of,
    reindexed_state,
    switch_on,
)

# The transactions of a round, each in a thread of its own.
CREATORS = 8
# The rounds of a test. Each round adds `CREATORS` entries to each tree, so
# the buckets of each tree fill and split during the test.
ROUNDS = 16
# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60
# The workflow of the folders and the Documents.
WORKFLOW = "simple_publication_workflow"


def mechanism_on(test):
    """Switch on the mechanism of concurrent creations, until the end of
    `test`. Return the keyword arguments of `zoe.Storage` for the store of
    the test.

    The test calls it once, before it copies the site of the layer to its
    store. So the copy takes a change that the helper commits to the site
    of the layer. `test.addCleanup` undoes a change of the process.

    The card does not name the mechanism. The ADR of the card names it:
    the split merge of ADR 0048. The helper writes the switch
    `split_merges` into the merge policy of the site of the layer, which
    the test then copies to its store, and gives no keyword argument.
    """
    import zoe.policy

    zoe.policy.install(
        test.layer["portal"]._p_jar, {"classes": {}, "split_merges": "on"}
    )
    transaction.commit()
    return {}


def mechanism_off(test):
    """Switch off the mechanism of concurrent creations, until the end of
    `test`: the site and its store keep the stock behaviour. Return the
    keyword arguments of `zoe.Storage` for the store of the test.

    The test calls it as it calls `mechanism_on`. Without a merge policy,
    the store keeps the stock behaviour, so the helper does nothing and
    gives no keyword argument (ADR 0048).
    """
    return {}


def folder_id(turn):
    """Return the id of the folder of the creator `turn`."""
    return f"folder-{turn}"


class TestConcurrentCreations(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    # A failure shows the error of each turn of each round.
    maxDiff = None

    def setUp(self):
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        switch_on(self)
        helpers.wire_ids(self)
        portal.portal_workflow.setChainForPortalTypes(
            ("Folder", "Document"), (WORKFLOW,)
        )
        for turn in range(CREATORS):
            portal.invokeFactory("Folder", folder_id(turn), title=f"Folder {turn}")
        # The first version of a site makes the storages of
        # `portal_historiesstorage`. This Document makes them before the
        # rounds, as on a site that is in use.
        portal.invokeFactory("Document", "first", title="First Document")
        transaction.commit()
        # The copy takes the records, not the keys of the queue. So the
        # indexer applies the queue before the copy.
        drain(portal)

    def open_site(self, options):
        """Copy the site of the layer to a new `zoe.Storage`, opened with
        the keyword arguments `options`. Return the site on that store, on
        a connection of this thread, until the end of the test."""
        return helpers.open_site(
            self, options, "zoe-creations-", CREATORS + 2, user=TEST_USER_NAME
        )

    def create_round(self, number):
        """Run round `number`: `CREATORS` threads, each with a connection of
        its own, create one Document each in the folder of their turn, from
        one snapshot. Then they commit, one after the other, in the order
        of their turns. Return the error of each turn that raised, by turn.
        """

        def create(site, turn):
            site[folder_id(turn)].invokeFactory(
                "Document",
                f"document-{number}",
                title=f"Document {number} of creator {turn}",
                description=f"Round {number}, creator {turn}",
            )

        return helpers.run_round(self, CREATORS, create, TIMEOUT)

    def create_rounds(self):
        """Run each round. Return the errors of each round that had one, as
        text, by round and turn."""
        return helpers.round_errors(self.create_round, ROUNDS)

    def documents(self):
        """Return the Documents of the folders, by path."""
        found = {}
        for turn in range(CREATORS):
            for document in self.portal[folder_id(turn)].objectValues():
                if document.portal_type == "Document":
                    found[path_of(document)] = document
        return found

    def test_the_creations_of_each_round_commit_at_the_first_attempt(self):
        self.open_site(mechanism_on(self))
        self.assertEqual(self.create_rounds(), {})
        self.assertEqual(len(self.documents()), ROUNDS * CREATORS)

    def test_the_history_ids_stay_unique(self):
        self.open_site(mechanism_on(self))
        self.create_rounds()
        handler = self.portal.portal_historyidhandler
        ids = {
            path: handler.queryUid(document)
            for path, document in self.documents().items()
        }
        self.assertTrue(ids)
        self.assertNotIn(None, ids.values())
        self.assertEqual(len(set(ids.values())), len(ids), ids)

    def test_the_intids_stay_unique(self):
        from zope.component import getUtility
        from zope.intid.interfaces import IIntIds

        self.open_site(mechanism_on(self))
        self.create_rounds()
        intids = getUtility(IIntIds)
        documents = self.documents()
        ids = {path: intids.queryId(document) for path, document in documents.items()}
        self.assertTrue(ids)
        self.assertNotIn(None, ids.values())
        self.assertEqual(len(set(ids.values())), len(ids), ids)
        for path, document in documents.items():
            found = intids.getObject(ids[path])
            self.assertIs(aq_base(found), aq_base(document), path)

    def test_each_version_of_each_document_loads(self):
        self.open_site(mechanism_on(self))
        self.create_rounds()
        repository = self.portal.portal_repository
        documents = self.documents()
        self.assertTrue(documents)
        for path, document in documents.items():
            history = repository.getHistoryMetadata(document)
            self.assertTrue(history, path)
            versions = history.getLength(countPurged=True)
            self.assertGreaterEqual(versions, 1, path)
            for version in range(versions):
                retrieved = repository.retrieve(document, version).object
                self.assertEqual(retrieved.getId(), document.getId(), path)
                self.assertEqual(retrieved.Title(), document.Title(), path)

    def test_after_the_indexer_the_catalog_is_a_reindex_from_scratch(self):
        portal = self.open_site(mechanism_on(self))
        self.create_rounds()
        drain(portal)
        documents = self.documents()
        self.assertTrue(documents)
        state = catalog_state(portal)
        self.assertLessEqual(set(documents), set(state["objects"]))
        self.assertEqual(state, reindexed_state(portal))

    def test_with_the_mechanism_off_the_creations_raise_conflict_error(self):
        self.open_site(mechanism_off(self))
        conflicts = []
        others = []
        for number in range(ROUNDS):
            for error in self.create_round(number).values():
                if isinstance(error, ConflictError):
                    conflicts.append(helpers.describe(error))
                else:
                    others.append(helpers.describe(error))
            if conflicts or others:
                break
        self.assertEqual(others, [])
        self.assertTrue(conflicts)
