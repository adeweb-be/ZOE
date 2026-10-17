"""Acceptance test of queued indexing.

With queued indexing on, a save of content writes no object of the
catalog. The indexing queue of Products.CMFCore still collects the catalog
operations of a transaction: index, reindex and unindex. These operations
go to the durable namespace of the store (ADR 0043) in the transaction of
the save, not to the catalog. One indexer applies them to the catalog in
transactions of its own.

The test checks, on a Plone site of the functional layer of
plone.app.testing, with the option on:

- a save of a Document, an edit or an add, writes no record of the
  catalog: the catalog tool and each persistent object that it reaches
  keep their serials, and the catalog reaches no new record;
- after the indexer runs, the catalog equals a reindex from scratch: the
  same paths, the same entry of each index for each path, the same
  metadata;
- two saves of different Documents from one snapshot both commit at the
  first attempt, and the indexer applies both;
- an edit, a workflow transition, a move and a delete, each in a
  transaction of its own, reach the catalog in order, also when a new
  Document takes the path of a deleted one;
- a crash of the indexer between two batches loses no operation, and a
  second run applies each one once.

The ADR of queued indexing names the option, the indexer and its batches.
Only the helpers `switch_on`, `drain` and `crash_after_first_batch` use
these names. The human approves a change of these helpers with the ADR.

The layers of plone.app.testing need the interpreter of a Plone instance.
So this module is in the package, with the other tests of `zoe.plone` that
need a Plone site: `bin/test -s zoe.plone` of the instance of the zoe
scenario runs it, on the layer of `zoe.plone.testing`, whose base storage
is a `zoe.Storage`. `just plone zoe backends` runs it.
"""

import collections
import itertools
import threading
import unittest

import Missing
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
from Products.ZCatalog.Catalog import Catalog
from Testing.makerequest import makerequest
from ZODB.serialize import referencesf
from ZODB.utils import get_pickle_metadata, load_current
from zope.component.hooks import setSite
from zope.event import notify
from zope.globalrequest import clearRequest, setRequest
from zope.lifecycleevent import ObjectModifiedEvent

# The longest time that a test waits for a thread, in seconds.
TIMEOUT = 60
# The Documents of the site at the start of each test.
DOCUMENTS = 5
# The workflow of the Documents.
WORKFLOW = "simple_publication_workflow"
# The operations in a batch of the indexer when a test simulates a crash.
BATCH = 2
# The missing value of ZCatalog, as `plain` gives it.
MISSING = "Missing.Value"


def switch_on(test):
    """Switch queued indexing on for the site of the layer of `test`, until
    the end of the test.

    While the option is on in a test, no indexer runs by itself: the test
    runs it with `drain` or `crash_after_first_batch`.

    The card does not name the option. The ADR of queued indexing names it,
    and how a site or a process switches it on. The human approves a
    change of this helper with the ADR. Until then, the helper calls
    `install()` of the module `zoe.plone.indexing`, and `uninstall()` at
    the end of the test.
    """
    from zoe.plone import indexing

    indexing.install()
    test.addCleanup(indexing.uninstall)


def drain(portal):
    """Run the indexer of the site `portal` once, in this process, until it
    applied each queued operation. Then begin a new transaction, so that
    the test sees the commits of the indexer.

    Call it when the current transaction of the test holds no change.

    The card does not name the indexer. The ADR of queued indexing names it,
    and how a test runs it. The human approves a change of this helper with
    the ADR. Until then, the helper calls `run()` of
    `zoe.plone.indexing.Indexer(portal)`.
    """
    from zoe.plone import indexing

    try:
        indexing.Indexer(portal).run()
    finally:
        transaction.begin()


class _Crash(BaseException):
    """Stops the indexer at once, as a crash of its process does."""


def crash_after_first_batch(portal, batch_size):
    """Run the indexer of the site `portal`, in this process, with batches
    of `batch_size` operations. Stop it as a crash does, after the commit
    of its first batch and before its second batch begins. Then begin a new
    transaction.

    After the call, the store holds what such a crash leaves. A new indexer
    can then run at once, as after the lease of the crashed indexer
    expired.

    The card does not name the batches of the indexer, nor how a test
    stops it. The ADR of queued indexing names them. The human approves a
    change of this helper with the ADR. Until then, the helper gives
    `zoe.plone.indexing.Indexer` the keyword `batch_size`, and raises
    `_Crash` from its attribute `after_batch`, which the indexer calls after
    the commit of each batch.
    """
    from zoe.plone import indexing

    indexer = indexing.Indexer(portal, batch_size=batch_size)

    def crash(*args, **kwargs):
        raise _Crash

    indexer.after_batch = crash
    try:
        indexer.run()
    except _Crash:
        pass
    else:
        raise AssertionError("the indexer ended without the commit of a batch")
    finally:
        transaction.begin()


def edit(document, title):
    """Save `document` with the title `title`, as the edit form of Plone
    does: set the field, then notify `ObjectModifiedEvent`."""
    document.title = title
    notify(ObjectModifiedEvent(document))


def path_of(obj):
    """Return the path of `obj`, the key of the catalog."""
    return "/".join(obj.getPhysicalPath())


def catalog_records(portal):
    """Return the records of the catalog of `portal` as the store holds
    them: the record of the catalog tool and of each persistent object
    that it reaches, as a dict of oid to `(serial, class name)`."""
    catalog = aq_base(portal.portal_catalog)
    storage = catalog._p_jar.db().storage
    records = {}
    todo = [catalog._p_oid]
    while todo:
        oid = todo.pop()
        if oid in records:
            continue
        data, serial = load_current(storage, oid)
        records[oid] = (serial, ".".join(get_pickle_metadata(data)))
        todo.extend(referencesf(data))
    return records


def written(before, after):
    """Return the sorted class names of the records that changed from
    `before` to `after`, two results of `catalog_records`: a new record, a
    record with another serial, and a record that the catalog no longer
    reaches."""
    names = [
        name
        for oid, (serial, name) in after.items()
        if before.get(oid, (None, None))[0] != serial
    ]
    names.extend(name for oid, (_, name) in before.items() if oid not in after)
    return sorted(names)


def plain(value):
    """Return `value` as values that compare by value: a mapping as a dict,
    a set of BTrees as a tuple of its keys, a list as a tuple, and the
    missing value of ZCatalog as `MISSING`."""
    if isinstance(value, type(Missing.Value)):
        return MISSING
    if isinstance(value, (str, bytes)):
        return value
    if isinstance(value, (list, tuple)):
        return tuple(plain(item) for item in value)
    if hasattr(value, "items"):
        return {key: plain(item) for key, item in value.items()}
    if hasattr(value, "keys"):
        return tuple(value.keys())
    return value


def entry(value):
    """Return the entry of an index for one object as a value that compares
    by value. The order of a sequence does not count: a keyword index keeps
    its keywords in the order of a set."""
    value = plain(value)
    if isinstance(value, tuple):
        return tuple(sorted(value, key=repr))
    return value


def catalog_state(portal):
    """Return the content of the catalog of `portal`: for each path, the
    entry of each index and the metadata; the paths of the catalog by rid
    and by uid; the size of each index; and the length of the catalog."""
    catalog = portal.portal_catalog._catalog
    objects = {}
    for rid, path in catalog.paths.items():
        indexes = {
            name: entry(catalog.getIndex(name).getEntryForObject(rid, None))
            for name in catalog.indexes
        }
        metadata = plain(catalog.getMetadataForRID(rid))
        objects[path] = {"indexes": indexes, "metadata": metadata}
    sizes = {}
    for name in catalog.indexes:
        index = catalog.getIndex(name)
        sizes[name] = tuple(
            getattr(index, size)()
            for size in ("numObjects", "indexSize")
            if hasattr(index, size)
        )
    return {
        "objects": objects,
        "uids": sorted(catalog.uids.keys()),
        "sizes": sizes,
        "length": len(catalog),
    }


def reindexed_state(portal):
    """Return the content of the catalog of `portal` after a reindex from
    scratch, and leave the store as it is.

    The reindex indexes what `clearFindAndRebuild` of Plone indexes: it
    empties the catalog, then indexes the site and each object below it
    that has `reindexObject`, except the catalog. It calls `catalog_object`
    of the catalog for each object: the stock path of ZCatalog, without the
    indexing queue. Then the transaction aborts, so the reindex reaches no
    store.
    """
    catalog = portal.portal_catalog
    objects = [portal]

    def collect(obj, path):
        base = aq_base(obj)
        if base is not aq_base(catalog) and callable(
            getattr(base, "reindexObject", None)
        ):
            objects.append(obj)

    try:
        portal.ZopeFindAndApply(portal, search_sub=True, apply_func=collect)
        catalog.manage_catalogClear()
        for obj in objects:
            catalog.catalog_object(obj, path_of(obj))
        return catalog_state(portal)
    finally:
        transaction.abort()


class CatalogWrites:
    """Count the writes of a catalog that commit, by path.

    ZCatalog writes the entries of an object through
    `Catalog.catalogObject`, and removes them through
    `Catalog.uncatalogObject`. While the test runs, each call counts in
    `indexed` or in `unindexed` when its transaction commits, in any thread
    and with any transaction manager.
    """

    def __init__(self, test):
        self.indexed = collections.Counter()
        self.unindexed = collections.Counter()
        self._lock = threading.Lock()
        # id of a transaction -> (the transaction, [(counter, path)])
        self._pending = {}
        stock_catalog = Catalog.catalogObject
        stock_uncatalog = Catalog.uncatalogObject
        writes = self

        def catalogObject(catalog, obj, uid, *args, **kwargs):
            writes._record(catalog, writes.indexed, uid)
            return stock_catalog(catalog, obj, uid, *args, **kwargs)

        def uncatalogObject(catalog, uid):
            writes._record(catalog, writes.unindexed, uid)
            return stock_uncatalog(catalog, uid)

        Catalog.catalogObject = catalogObject
        Catalog.uncatalogObject = uncatalogObject
        test.addCleanup(setattr, Catalog, "catalogObject", stock_catalog)
        test.addCleanup(setattr, Catalog, "uncatalogObject", stock_uncatalog)

    def _record(self, catalog, counter, path):
        jar = catalog._p_jar
        txn = transaction.get() if jar is None else jar.transaction_manager.get()
        with self._lock:
            pending = self._pending.get(id(txn))
            if pending is None:
                pending = self._pending[id(txn)] = (txn, [])
                txn.addAfterCommitHook(self._end, (id(txn),))
            pending[1].append((counter, path))

    def _end(self, committed, key):
        with self._lock:
            _txn, calls = self._pending.pop(key)
            if committed:
                for counter, path in calls:
                    counter[path] += 1


class TestQueuedIndexing(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        self.portal = portal = self.layer["portal"]
        self.tokens = itertools.count()
        setRoles(portal, TEST_USER_ID, ["Manager"])
        switch_on(self)
        portal.portal_workflow.setChainForPortalTypes(("Document",), (WORKFLOW,))
        portal.invokeFactory("Folder", "folder", title="Folder")
        for n in range(DOCUMENTS):
            portal.invokeFactory(
                "Document",
                f"doc-{n}",
                title=f"Document {self.token()}",
                description=f"Description {self.token()}",
            )
        transaction.commit()
        drain(portal)

    def token(self):
        """Return a word that no other text of the test holds. So a save
        with it adds a word to the text indexes."""
        return f"token{next(self.tokens)}x"

    def search(self, **query):
        """Return the paths of the catalog for `query`."""
        brains = self.portal.portal_catalog.unrestrictedSearchResults(**query)
        return sorted(brain.getPath() for brain in brains)

    def assertCatalogIsAReindexFromScratch(self):
        state = catalog_state(self.portal)
        self.assertIn(path_of(self.portal["doc-0"]), state["objects"])
        self.assertEqual(state, reindexed_state(self.portal))

    def test_an_edit_writes_no_object_of_the_catalog(self):
        document = self.portal["doc-0"]
        before = catalog_records(self.portal)
        serial = document._p_serial
        edit(document, f"Edited {self.token()}")
        transaction.commit()
        # The save committed the Document, and no record of the catalog.
        self.assertNotEqual(document._p_serial, serial)
        self.assertEqual(written(before, catalog_records(self.portal)), [])

    def test_an_add_writes_no_object_of_the_catalog(self):
        before = catalog_records(self.portal)
        self.portal.invokeFactory("Document", "added", title=f"Added {self.token()}")
        transaction.commit()
        self.assertIn("added", self.portal.objectIds())
        self.assertEqual(written(before, catalog_records(self.portal)), [])

    def test_after_the_indexer_the_catalog_is_a_reindex_from_scratch(self):
        portal = self.portal
        edited = self.token()
        edit(portal["doc-1"], f"Edited {edited}")
        transaction.commit()
        added = self.token()
        portal["folder"].invokeFactory("Document", "added", title=f"Added {added}")
        transaction.commit()
        drain(portal)
        self.assertEqual(self.search(SearchableText=edited), [path_of(portal["doc-1"])])
        self.assertEqual(
            self.search(SearchableText=added), [path_of(portal["folder"]["added"])]
        )
        self.assertCatalogIsAReindexFromScratch()

    def test_two_saves_from_one_snapshot_both_commit_at_the_first_attempt(self):
        db = self.portal._p_jar.db()
        saves = {"doc-1": self.token(), "doc-2": self.token()}
        changed = threading.Barrier(len(saves), timeout=TIMEOUT)
        first_committed = threading.Event()
        errors = {}

        def save(turn, document_id, word):
            connection = db.open()
            try:
                app = makerequest(connection.root()["Application"])
                setRequest(app.REQUEST)
                site = app[PLONE_SITE_ID]
                setSite(site)
                login(site, TEST_USER_NAME)
                edit(site[document_id], f"Saved {word}")
                # Both saves change their Document from one snapshot. Then
                # they commit, one after the other.
                changed.wait()
                if turn:
                    first_committed.wait(TIMEOUT)
                transaction.commit()
            except BaseException as error:
                errors[document_id] = f"{type(error).__name__}: {error}"
            finally:
                if not turn:
                    first_committed.set()
                transaction.abort()
                logout()
                setSite(None)
                clearRequest()
                connection.close()

        threads = [
            threading.Thread(target=save, args=(turn, document_id, word))
            for turn, (document_id, word) in enumerate(saves.items())
        ]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join(TIMEOUT)
            self.assertFalse(thread.is_alive())
        self.assertEqual(errors, {})
        transaction.begin()
        drain(self.portal)
        for document_id, word in saves.items():
            self.assertEqual(
                self.search(SearchableText=word),
                [path_of(self.portal[document_id])],
            )
        self.assertCatalogIsAReindexFromScratch()

    def test_an_edit_a_transition_a_move_and_a_delete_reach_the_catalog_in_order(
        self,
    ):
        portal = self.portal
        workflow = portal.portal_workflow
        # One Document: an edit, a transition, a second edit, a move.
        moved = portal["doc-1"]
        moved_uid = moved.UID()
        moved_from = path_of(moved)
        first, second = self.token(), self.token()
        edit(moved, f"Moved {first}")
        transaction.commit()
        workflow.doActionFor(moved, "publish")
        transaction.commit()
        edit(moved, f"Moved {second}")
        transaction.commit()
        portal["folder"].manage_pasteObjects(portal.manage_cutObjects(["doc-1"]))
        transaction.commit()
        moved = portal["folder"]["doc-1"]
        # Another Document: an edit, a delete, then a new Document on its
        # path.
        deleted = portal["doc-2"]
        deleted_uid = deleted.UID()
        deleted_path = path_of(deleted)
        edit(deleted, f"Deleted {self.token()}")
        transaction.commit()
        portal.manage_delObjects(["doc-2"])
        transaction.commit()
        replacement = self.token()
        portal.invokeFactory("Document", "doc-2", title=f"Replacement {replacement}")
        transaction.commit()
        replacement_uid = portal["doc-2"].UID()

        drain(portal)

        # The move, the transition and the second edit reached the catalog.
        self.assertEqual(self.search(UID=moved_uid), [path_of(moved)])
        (brain,) = portal.portal_catalog.unrestrictedSearchResults(UID=moved_uid)
        self.assertEqual(brain.review_state, "published")
        self.assertEqual(brain.Title, f"Moved {second}")
        self.assertEqual(self.search(path={"query": moved_from, "depth": 0}), [])
        self.assertEqual(self.search(SearchableText=first), [])
        # The delete reached the catalog before the add on the same path.
        self.assertEqual(self.search(UID=deleted_uid), [])
        self.assertEqual(self.search(UID=replacement_uid), [deleted_path])
        self.assertEqual(
            self.search(path={"query": deleted_path, "depth": 0}), [deleted_path]
        )
        self.assertEqual(self.search(SearchableText=replacement), [deleted_path])
        self.assertCatalogIsAReindexFromScratch()

    def test_a_crash_between_two_batches_loses_no_operation(self):
        portal = self.portal
        words = {}
        # One save for each Document, each in a transaction of its own: more
        # operations than one batch holds.
        for n in range(DOCUMENTS):
            document = portal[f"doc-{n}"]
            words[path_of(document)] = word = self.token()
            edit(document, f"Saved {word}")
            transaction.commit()
        writes = CatalogWrites(self)

        crash_after_first_batch(portal, BATCH)

        # The first batch reached the catalog, and only the first batch.
        self.assertEqual(sum(writes.indexed.values()), BATCH)
        self.assertLessEqual(set(writes.indexed), set(words))
        self.assertEqual(writes.unindexed, collections.Counter())

        drain(portal)

        # The second run applied the other operations. Each operation
        # reached the catalog once, in a transaction that committed.
        self.assertEqual(writes.indexed, collections.Counter(words.keys()))
        self.assertEqual(writes.unindexed, collections.Counter())
        for path, word in words.items():
            self.assertEqual(self.search(SearchableText=word), [path])
        self.assertCatalogIsAReindexFromScratch()
