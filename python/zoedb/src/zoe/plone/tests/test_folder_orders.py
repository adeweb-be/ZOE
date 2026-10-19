"""Tests of the order merge of ADR 0052 on a Plone site.

They reuse the site and the rounds of the acceptance test of concurrent
creations in one folder (`test_creations_in_one_folder`), with the merge
on:

- with queued indexing, after the indexer ran, a search of the catalog
  sorted on `getObjPositionInParent` gives the order of the folder;
- with the stock indexing, a creation writes the catalog in its own
  transaction; the creations of a round commit, or raise `ConflictError`
  and commit at their retry, and the order, `pos` and the catalog agree;
- the profile `zoe.plone:default` switches `folder_orders` on, and an
  override of `off` in a later profile wins.

`bin/test -s zoe.plone -t test_folder_orders` of the instance of the zoe
scenario runs them.
"""

import json
import os

import transaction
from plone.app.testing import TEST_USER_ID, setRoles
from plone.folder.default import DefaultOrdering
from ZODB.POSException import ConflictError

from zoe.plone.tests import test_creations_in_one_folder as accept
from zoe.plone.tests.test_concurrent_creations import wire_ids
from zoe.plone.tests.test_queued_indexing import drain, path_of


def catalog_order(portal, folder):
    """Return the ids of the items of `folder` that the catalog finds,
    sorted on `getObjPositionInParent`.

    The index of Plone 6.1 of that name is a `GopipIndex` of plone.folder
    (`plone/folder/nogopip.py`): it stores no value, and computes the
    positions from the order of the folder when it sorts."""
    catalog = portal.portal_catalog
    brains = catalog.unrestrictedSearchResults(
        path={"query": path_of(folder), "depth": 1},
        sort_on="getObjPositionInParent",
    )
    return [brain.getId for brain in brains]


def stored_positions(portal):
    """Return the positions that the catalog of `portal` stores: none, as
    its index of positions stores no value."""
    index = portal.portal_catalog._catalog.getIndex("getObjPositionInParent")
    return index.numObjects()


class TestWithQueuedIndexing(accept.TestCreationsInOneFolder):
    def test_the_catalog_gives_the_positions_of_pos_after_the_indexer(self):
        portal = self.open_site(accept.mechanism_on(self))
        self.assertEqual(self.create_rounds(), {})
        drain(portal)
        folder = portal[accept.FOLDER]
        order = self.assert_order_agrees(folder)
        self.assertEqual(catalog_order(portal, folder), order)
        self.assertEqual(stored_positions(portal), 0)

    # The tests of the acceptance module run in their own module.
    test_the_creations_of_each_round_commit_at_the_first_attempt = None
    test_the_order_holds_each_new_item_once_and_pos_agrees = None
    test_each_new_item_is_found_from_another_connection = None
    test_with_the_mechanism_off_a_round_raises_conflict_error = None


class TestWithTheStockIndexing(accept.TestCreationsInOneFolder):
    def setUp(self):
        # The setup of the acceptance test, without queued indexing.
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        wire_ids(self)
        portal.portal_workflow.setChainForPortalTypes(
            ("Folder", "Document"), (accept.WORKFLOW,)
        )
        portal.invokeFactory("Folder", accept.FOLDER, title="Folder")
        portal[accept.FOLDER].invokeFactory(
            "Document", accept.FIRST, title="First Document"
        )
        transaction.commit()
        self.assertIsInstance(portal[accept.FOLDER].getOrdering(), DefaultOrdering)

    def test_the_order_pos_and_the_catalog_agree(self):
        portal = self.open_site(accept.mechanism_on(self))
        errors = self.create_round(0)
        others = {
            turn: accept.describe(error)
            for turn, error in errors.items()
            if not isinstance(error, ConflictError)
        }
        self.assertEqual(others, {})
        for turn in sorted(errors):
            error = self.in_connection(
                lambda site, turn=turn: accept.create(site[accept.FOLDER], 0, turn)
            )
            self.assertIsNone(error, f"retry of turn {turn}: {error!r}")
        transaction.begin()
        folder = portal[accept.FOLDER]
        order = self.assert_order_agrees(folder)
        self.assertEqual(catalog_order(portal, folder), order)
        self.assertEqual(stored_positions(portal), 0)

    test_the_creations_of_each_round_commit_at_the_first_attempt = None
    test_the_order_holds_each_new_item_once_and_pos_agrees = None
    test_each_new_item_is_found_from_another_connection = None
    test_with_the_mechanism_off_a_round_raises_conflict_error = None


class TestTheProfile(accept.TestCreationsInOneFolder):
    def setUp(self):
        setRoles(self.layer["portal"], TEST_USER_ID, ["Manager"])

    def test_the_profile_switches_folder_orders_on_and_an_override_wins(self):
        import zoe.plone
        import zoe.policy
        from zoe.plone import merges

        portal = self.layer["portal"]
        path = os.path.join(
            os.path.dirname(zoe.plone.__file__), "profiles", "default", merges.OVERRIDES
        )
        with open(path) as file:
            overrides = json.load(file)
        self.assertEqual(overrides.get("folder_orders"), "on")
        kept = merges.site_overrides(portal, create=True)
        kept["profile-zoe.plone:default"] = overrides
        merges.update_policy(portal)
        self.assertEqual(zoe.policy.read(portal._p_jar).get("folder_orders"), "on")
        # A later profile of the site switches it off.
        kept["profile-site:default"] = {"folder_orders": "off"}
        merges.update_policy(portal)
        self.assertEqual(zoe.policy.read(portal._p_jar).get("folder_orders"), "off")
        transaction.abort()

    test_the_creations_of_each_round_commit_at_the_first_attempt = None
    test_the_order_holds_each_new_item_once_and_pos_agrees = None
    test_each_new_item_is_found_from_another_connection = None
    test_with_the_mechanism_off_a_round_raises_conflict_error = None
