"""Acceptance test of concurrent creations in one folder.

A creation of a Document in a folder of plone.app.contenttypes writes the
folder too:

- `BTreeFolder2Base._setOb` inserts the id into `_tree`, adds 1 to
  `_count` and inserts the id into `_mt_index["Dexterity Item"]`. The
  key is the meta type of a Dexterity item, and a Document is one.
- `DefaultOrdering.notifyAdded` of plone.folder appends the id to the
  order, a `PersistentList` in the annotations of the folder, and inserts
  the id into the positions, the `OIBTree` `pos` beside it. `pos` gives
  the position of each id in the order.
- The reindex of the folder sets its `modification_date`, so the record of
  the `Folder` changes.

Tier 1 merges `_count`, and the inserts of different ids into `_tree`,
`_mt_index["Dexterity Item"]` and `pos`. Two creations from one snapshot
both set `modification_date`, so the record of the `Folder` conflicts.
Each one also appends to the order: the `PersistentList` has no
resolver, so it conflicts too. It must conflict as long as no merge
changes the order and `pos` together: both creations give their id the
same position in `pos`.

The test checks, on a Plone site of the functional layer of
plone.app.testing on a ZOE store, with queued indexing on:

- in each of `ROUNDS` rounds, `CREATORS` transactions that each create a
  versioned Document in one folder, from one snapshot, all commit at the
  first attempt;
- the order of the folder holds each item of the folder once, each new
  item among them; `pos` agrees with the order; `objectIds` of the folder
  gives the order; and the items of each round come after the items of
  the rounds before it;
- from another connection, each new item loads from the folder, and after
  the indexer runs, the catalog finds each new item in the folder;
- with the mechanism off, a round raises `ConflictError`, and a retry of
  each creation that failed commits and keeps the order and `pos` in
  agreement.

A site on a ZEO server, or on a ZOE store without the mechanism, keeps the
stock behaviour. From one snapshot, the first creation of a round commits.
Each other creation raises `ConflictError` on the `Folder` or on the
order, and the publisher of Zope retries the request: the creation runs
again from a new snapshot and then commits. The last test of the module
checks that behaviour on a ZOE store.

The rounds. The folder holds one Document before the rounds, as a folder
that is in use: the first item of a meta type inserts the key
`Dexterity Item` into `_mt_index`, and two such inserts of one key
conflict. Each round adds `CREATORS` items to the folder, so the trees of
the folder (`_tree`, `_mt_index["Dexterity Item"]` and `pos`) fill and
split during the rounds. The split merge of ADR 0048 merges these
splits, so the mechanism of the test switches it on, as it switches on
the merges of the trees of CMFEditions and of the intid utility.

The store. The layer keeps the site in a `DemoStorage`, which resolves
conflicts in Python, not with the tiers of the store. So each test copies
the committed records of the site to a `zoe.Storage` of its own, as the
acceptance test of concurrent creations in `test_concurrent_creations`
does, and the rounds run there. The merges of ZOE run in the core, so the
in-process `zoe.Storage` and a client of `zoe serve` get the same ones.

ADR 0052 names the mechanism and keeps the name of the switch. Only the
helpers `mechanism_on` and `mechanism_off` name it, through the constant
`SWITCH`.

The layers of plone.app.testing need the interpreter of a Plone instance.
So this module is in the package, with the other tests of `zoe.plone` that
need a Plone site: `bin/test -s zoe.plone` of the instance of the zoe
scenario runs it. `just plone zoe backends` runs it, and `just plone zoe
backends -t test_creations_in_one_folder` runs only this module.
"""

import shutil
import tempfile
import threading
import unittest

import transaction
from Acquisition import aq_base
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    PLONE_SITE_ID,
    TEST_USER_ID,
    TEST_USER_NAME,
    login,
    logout,
    setRoles,
)
from plone.folder.default import DefaultOrdering
from Testing.makerequest import makerequest
from ZODB.DB import DB
from ZODB.POSException import ConflictError
from zope.component.hooks import getSite, setSite
from zope.globalrequest import clearRequest, getRequest, setRequest

from zoe.plone.tests.test_concurrent_creations import (
    copy_records,
    describe,
    wire_ids,
)
from zoe.plone.tests.test_queued_indexing import drain, path_of, switch_on

# The transactions of a round, each in a thread of its own.
CREATORS = 8
# The rounds of a test. Each round adds `CREATORS` items to the folder, so
# the trees of the folder fill and split during the test.
ROUNDS = 10
# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60
# The workflow of the folder and the Documents.
WORKFLOW = "simple_publication_workflow"
# The id of the folder of the test.
FOLDER = "folder"
# The id of the Document that the folder holds before the rounds.
FIRST = "first"
# The switch of the merge policy for the mechanism of ADR 0052.
SWITCH = "folder_orders"
# The tier 2 entry of the class of the folder (ADR 0052).
FOLDER_ENTRY = {
    "tier2": "on",
    "default": "strict",
    "fields": {"modification_date": "max"},
}


def mechanism_on(test):
    """Switch on the mechanism of concurrent creations in one folder, until
    the end of `test`. Return the keyword arguments of `zoe.Storage` for
    the store of the test.

    The test calls it once, before it copies the site of the layer to its
    store. So the copy takes a change that the helper commits to the site
    of the layer.

    The helper writes a merge policy into the site of the layer and gives
    no keyword argument, as ADR 0052 says. The policy switches on the split
    merge of ADR 0048 (`split_merges`), because the trees of the folder and
    of CMFEditions split during the rounds. It gives the class of the
    folder the tier 2 entry `FOLDER_ENTRY`, which merges
    `modification_date` with `max`. It also switches on `SWITCH`, when
    `zoe.policy` knows that switch. Until the build adds it, the policy
    holds no `SWITCH`.

    The human approves a change of these helpers with the ADR.
    """
    import zoe.policy

    policy = {
        "classes": {"plone.app.contenttypes.content.Folder": FOLDER_ENTRY},
        "split_merges": "on",
    }
    if SWITCH in zoe.policy.SWITCHES:
        policy[SWITCH] = "on"
    zoe.policy.install(test.layer["portal"]._p_jar, policy)
    transaction.commit()
    return {}


def mechanism_off(test):
    """Switch off the mechanism of concurrent creations in one folder,
    until the end of `test`: the folder and its order keep the stock
    behaviour. Return the keyword arguments of `zoe.Storage` for the store
    of the test.

    The test calls it as it calls `mechanism_on`. The helper writes the
    policy of `mechanism_on` without `SWITCH`, and gives no keyword
    argument, as ADR 0052 says. So the trees merge their splits, the
    record of the folder merges `modification_date`, and only the order of
    the folder conflicts. The human approves a change of this helper with
    the ADR.
    """
    import zoe.policy

    zoe.policy.install(
        test.layer["portal"]._p_jar,
        {
            "classes": {"plone.app.contenttypes.content.Folder": FOLDER_ENTRY},
            "split_merges": "on",
        },
    )
    transaction.commit()
    return {}


def document_id(number, turn):
    """Return the id of the Document of the creator `turn` in round
    `number`."""
    return f"document-{number}-{turn}"


def create(folder, number, turn):
    """Create the Document of the creator `turn` of round `number` in
    `folder`."""
    folder.invokeFactory(
        "Document",
        document_id(number, turn),
        title=f"Document {number} of creator {turn}",
        description=f"Round {number}, creator {turn}",
    )


def ordering_of(folder):
    """Return the order of `folder` as a list, and its positions as a
    dict."""
    ordering = folder.getOrdering()
    return list(ordering._order()), dict(ordering._pos().items())


class TestCreationsInOneFolder(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    # A failure shows the error of each turn of each round.
    maxDiff = None

    def setUp(self):
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        switch_on(self)
        wire_ids(self)
        portal.portal_workflow.setChainForPortalTypes(
            ("Folder", "Document"), (WORKFLOW,)
        )
        portal.invokeFactory("Folder", FOLDER, title="Folder")
        portal[FOLDER].invokeFactory("Document", FIRST, title="First Document")
        transaction.commit()
        # The test reads the order of the default ordering of plone.folder.
        self.assertIsInstance(portal[FOLDER].getOrdering(), DefaultOrdering)
        # The copy takes the records, not the keys of the queue. So the
        # indexer applies the queue before the copy.
        drain(portal)

    def open_site(self, options):
        """Copy the site of the layer to a new `zoe.Storage`, opened with
        the keyword arguments `options`. Return the site on that store, on
        a connection of this thread, until the end of the test."""
        import zoe

        directory = tempfile.mkdtemp(prefix="zoe-one-folder-")
        self.addCleanup(shutil.rmtree, directory, ignore_errors=True)
        storage = zoe.Storage(f"{directory}/zoe", **options)
        try:
            copy_records(self.layer["zodbDB"].storage, storage)
        except BaseException:
            storage.close()
            raise
        self.db = DB(storage, pool_size=CREATORS + 2)
        self.addCleanup(self.db.close)
        connection = self.db.open()
        self.addCleanup(connection.close)
        self.addCleanup(setRequest, getRequest())
        self.addCleanup(setSite, getSite())
        self.addCleanup(transaction.abort)
        app = makerequest(connection.root()["Application"])
        setRequest(app.REQUEST)
        portal = app[PLONE_SITE_ID]
        setSite(portal)
        login(portal, TEST_USER_NAME)
        self.portal = portal
        return portal

    def in_connection(self, work):
        """Call `work(site)` with the site on a new connection, in a thread
        of its own, and commit. Return the error that it raised, or
        `None`."""
        errors = []

        def run():
            connection = self.db.open()
            try:
                app = makerequest(connection.root()["Application"])
                setRequest(app.REQUEST)
                site = app[PLONE_SITE_ID]
                setSite(site)
                login(site, TEST_USER_NAME)
                work(site)
                transaction.commit()
            except BaseException as error:
                errors.append(error)
            finally:
                transaction.abort()
                logout()
                setSite(None)
                clearRequest()
                connection.close()

        thread = threading.Thread(target=run)
        thread.start()
        thread.join(TIMEOUT)
        self.assertFalse(thread.is_alive())
        return errors[0] if errors else None

    def create_round(self, number):
        """Run round `number`: `CREATORS` threads, each with a connection of
        its own, create one Document each in the folder, from one snapshot.
        Then they commit, one after the other, in the order of their turns.
        Return the error of each turn that raised, by turn.
        """
        db = self.db
        changed = threading.Barrier(CREATORS, timeout=TIMEOUT)
        done = [threading.Event() for _ in range(CREATORS)]
        errors = {}

        def creator(turn):
            connection = db.open()
            try:
                app = makerequest(connection.root()["Application"])
                setRequest(app.REQUEST)
                site = app[PLONE_SITE_ID]
                setSite(site)
                login(site, TEST_USER_NAME)
                create(site[FOLDER], number, turn)
                # Each creator changes the site from the snapshot of its
                # connection. Then the creators commit.
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

        threads = [
            threading.Thread(target=creator, args=(turn,)) for turn in range(CREATORS)
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive())
        # The connection of this thread sees the commits of the round.
        transaction.begin()
        return errors

    def create_rounds(self):
        """Run each round. Return the errors of each round that had one, as
        text, by round and turn."""
        failed = {}
        for number in range(ROUNDS):
            errors = self.create_round(number)
            if errors:
                failed[number] = {
                    turn: describe(error) for turn, error in sorted(errors.items())
                }
        return failed

    def new_ids(self):
        """Return the ids of the Documents of the rounds, by round."""
        return [
            {document_id(number, turn) for turn in range(CREATORS)}
            for number in range(ROUNDS)
        ]

    def assert_order_agrees(self, folder):
        """Check that the order of `folder` holds each item of the folder
        once, that `pos` agrees with the order, and that `objectIds` gives
        the order. Return the order."""
        order, pos = ordering_of(folder)
        self.assertEqual(len(order), len(set(order)), order)
        self.assertEqual(set(order), set(folder.objectIds(ordered=False)))
        self.assertEqual(pos, {item: index for index, item in enumerate(order)})
        self.assertEqual(list(folder.objectIds()), order)
        self.assertEqual(len(folder), len(order))
        return order

    def test_the_creations_of_each_round_commit_at_the_first_attempt(self):
        self.open_site(mechanism_on(self))
        self.assertEqual(self.create_rounds(), {})
        folder = self.portal[FOLDER]
        expected = set().union(*self.new_ids())
        self.assertLessEqual(expected, set(folder.objectIds(ordered=False)))

    def test_the_order_holds_each_new_item_once_and_pos_agrees(self):
        self.open_site(mechanism_on(self))
        self.create_rounds()
        order = self.assert_order_agrees(self.portal[FOLDER])
        # The item of the folder before the rounds keeps its position.
        self.assertEqual(order[0], FIRST)
        # The items of each round come after the items of the rounds
        # before it.
        start = 1
        for number, ids in enumerate(self.new_ids()):
            self.assertEqual(
                set(order[start : start + CREATORS]), ids, f"round {number}"
            )
            start += CREATORS
        self.assertEqual(len(order), start)
        self.assertEqual(
            set(self.portal[FOLDER]._mt_index["Dexterity Item"].keys()), set(order)
        )

    def test_each_new_item_is_found_from_another_connection(self):
        portal = self.open_site(mechanism_on(self))
        self.create_rounds()
        drain(portal)
        expected = set().union(*self.new_ids())
        found = {}

        def look(site):
            folder = site[FOLDER]
            found["order"] = self.assert_order_agrees(folder)
            items = set(folder.objectIds(ordered=False))
            found["loaded"] = {
                item: aq_base(folder[item]).portal_type for item in expected & items
            }
            found["catalog"] = {
                brain.getPath()
                for brain in site.portal_catalog.unrestrictedSearchResults(
                    path={"query": path_of(folder), "depth": 1}
                )
            }
            found["folder"] = path_of(folder)
            transaction.abort()

        self.assertIsNone(self.in_connection(look))
        self.assertEqual(found["loaded"], dict.fromkeys(expected, "Document"))
        self.assertLessEqual(expected, set(found["order"]))
        paths = {f"{found['folder']}/{item}" for item in expected}
        self.assertLessEqual(paths, found["catalog"])

    def test_with_the_mechanism_off_a_round_raises_conflict_error(self):
        self.open_site(mechanism_off(self))
        errors = self.create_round(0)
        conflicts = {
            turn: error
            for turn, error in errors.items()
            if isinstance(error, ConflictError)
        }
        others = {
            turn: describe(error)
            for turn, error in errors.items()
            if not isinstance(error, ConflictError)
        }
        self.assertEqual(others, {})
        self.assertTrue(conflicts)
        # The stock behaviour: each creation that failed runs again from a
        # new snapshot, as the publisher of Zope retries a request, and
        # then commits.
        for turn in sorted(conflicts):
            error = self.in_connection(
                lambda site, turn=turn: create(site[FOLDER], 0, turn)
            )
            self.assertIsNone(error, f"retry of turn {turn}: {error!r}")
        transaction.begin()
        order = self.assert_order_agrees(self.portal[FOLDER])
        self.assertEqual(order[0], FIRST)
        self.assertEqual(set(order[1:]), self.new_ids()[0])
