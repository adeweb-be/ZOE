"""Acceptance tests of `ZoeTextIndex` (ADR 0060, sections 8, 12 and 14).

The tests run on `ZOE_TEXT_SITE`: the site of `ZOE_SITE` in a store of its
own, with the backend `catalog` of `zoe.plone`, after the profile step
`zoe.plone:text`. `SearchableText`, `Title` and `Description` are
`ZoeTextIndex` indexes, and the native text index of the store answers
them. They check:

- a save writes no record of a text index, only its deltas;
- `SearchableText` with a word, a stemmed form in the site language, an
  accented form, a phrase, a prefix, `AND`, `OR` and `NOT`, alone and with
  `portal_type` and `path`, gives the hits of `EXPECTED`, in the order of
  the score, and the hook counts the query as native; the same queries on
  the Python path give the same rids in the same order, through
  `text_query`;
- in a transaction that creates and edits Documents, a search after each
  change finds the new texts and no longer finds the old ones, natively;
  after a savepoint and its rollback, the rolled-back texts are gone;
- a query at a snapshot outside the mirror window gives the matched set
  of the texts at that snapshot;
- two saves of different Documents from one snapshot commit at the first
  attempt; two texts of one Document from one snapshot raise
  `ConflictError` for the second;
- `clearFindAndRebuild`, `manage_clearIndex`, and `delIndex` then
  `addIndex` with the same id give an index that holds only the documents
  indexed after the clear;
- a change of the boosts applies to the next query;
- `numObjects`, `indexSize` and `getEntryForObject` give the values of
  `text_info`;
- `zoe.plone:text-uninstall` gives ZCTextIndex indexes that hold every
  catalogued object;
- on a storage without the extension `text`, the profile step raises
  `Unsupported` and changes nothing, and `index_object` of an installed
  `ZoeTextIndex` raises `Unsupported`;
- `uninstall()` of the backend refuses while the catalog holds a
  `ZoeTextIndex`.

The search form of Plone sends `munge_search_term` of the words of the
user: each word joined with `AND`, the last one with `*`.
`test_the_search_form_in_the_default_mode` records its hits in the
enhanced mode, where the glob of the last word also matches the stem of
the word: `running*` finds the words of the stem `run` and the words that
start with it, such as `runway`.

`just plone zoe backends` runs these tests.
"""

import contextlib
import time
import types
import unittest
from unittest import mock

import transaction
from plone.app.textfield.value import RichTextValue
from Products.CMFCore.indexing import processQueue
from Products.CMFPlone.browser.search import munge_search_term
from Products.ZCTextIndex.ZCTextIndex import ZCTextIndex
from ZODB.POSException import ConflictError, Unsupported
from zope.component.hooks import getSite, setSite

from zoe.plone import catalog, textindex
from zoe.plone import testing as zoe_testing
from zoe.plone.tests.test_catalog_mirror import key_of, opened, wait_ready

# The longest wait for the text index, in seconds.
TIMEOUT = 30

# The ids, types, folders, titles, descriptions and texts of the corpus.
CORPUS = {
    "n1": (
        "Document",
        "",
        "Garden tools",
        "Tools for the garden",
        "The gardener waters the plants with running water.",
    ),
    "n2": (
        "Document",
        "",
        "Runner guide",
        "A guide for runners",
        "Runners run every morning in new running shoes.",
    ),
    "n3": (
        "Document",
        "",
        "Runway lights",
        "Lights along a runway",
        "The runway lights shine at night.",
    ),
    "n4": (
        "Document",
        "",
        "Café notes",
        "Notes from the café",
        "The café serves coffee and tea.",
    ),
    "n5": (
        "Document",
        "",
        "Coffee beans",
        "Beans and roasting",
        "Fresh coffee beans roast slowly.",
    ),
    "n6": (
        "Document",
        "box",
        "Night sky",
        "Stars at night",
        "Stars shine in the night sky over the garden.",
    ),
    "n7": (
        "News Item",
        "box",
        "Coffee news",
        "News about coffee",
        "Coffee prices rise. The garden café opens.",
    ),
}

# The folder of the corpus.
FOLDER = "box"

# The text queries of the tests.
QUERIES = [
    "coffee",
    "gardens",
    "cafe",
    "Café",
    '"night sky"',
    "run*",
    "coffee AND tea",
    "coffee OR stars",
    "coffee AND NOT tea",
    "garden -night",
]

# The filters of each query.
FILTERS = {
    "alone": {},
    "type": {"portal_type": "Document"},
    "path": {"path": "/plone/box"},
}

# The hits of each query with each filter: the ids, in groups of equal
# score, from the highest score. `gardens` finds the stem `garden`; `cafe`
# and `Café` find `café`; `run*` finds the words of the stem `run` and
# `runway`, with the constant score of a glob in each field.
EXPECTED = {
    ('"night sky"', "alone"): [{"n6"}],
    ('"night sky"', "path"): [{"n6"}],
    ('"night sky"', "type"): [{"n6"}],
    ("Café", "alone"): [{"n4"}, {"n7"}],
    ("Café", "path"): [{"n7"}],
    ("Café", "type"): [{"n4"}],
    ("cafe", "alone"): [{"n4"}, {"n7"}],
    ("cafe", "path"): [{"n7"}],
    ("cafe", "type"): [{"n4"}],
    ("coffee", "alone"): [{"n7"}, {"n5"}, {"n4"}],
    ("coffee", "path"): [{"n7"}],
    ("coffee", "type"): [{"n5"}, {"n4"}],
    ("coffee AND NOT tea", "alone"): [{"n7"}, {"n5"}],
    ("coffee AND NOT tea", "path"): [{"n7"}],
    ("coffee AND NOT tea", "type"): [{"n5"}],
    ("coffee AND tea", "alone"): [{"n4"}],
    ("coffee AND tea", "path"): [],
    ("coffee AND tea", "type"): [{"n4"}],
    ("coffee OR stars", "alone"): [{"n6"}, {"n7"}, {"n5"}, {"n4"}],
    ("coffee OR stars", "path"): [{"n6"}, {"n7"}],
    ("coffee OR stars", "type"): [{"n6"}, {"n5"}, {"n4"}],
    ("garden -night", "alone"): [{"n1"}, {"n7"}],
    ("garden -night", "path"): [{"n7"}],
    ("garden -night", "type"): [{"n1"}],
    ("gardens", "alone"): [{"n1"}, {"n6"}, {"n7"}],
    ("gardens", "path"): [{"n6"}, {"n7"}],
    ("gardens", "type"): [{"n1"}, {"n6"}],
    ("run*", "alone"): [{"n2", "n3"}, {"n1"}],
    ("run*", "path"): [],
    ("run*", "type"): [{"n2", "n3"}, {"n1"}],
}

# The hits of the search form of Plone in the default mode, as `EXPECTED`.
# `running` becomes `running*`, which finds `runway` (n3) with the same
# score as the runners of n2; ZCTextIndex finds the words that start with
# `running` only, in n1 and n2.
SEARCH_FORM = {
    "café notes": [{"n4"}],
    "coff": [{"n7"}, {"n5"}, {"n4"}],
    "garden": [{"n1"}, {"n6", "n7"}],
    "running": [{"n2", "n3"}, {"n1"}],
}

# The texts of the search form.
SEARCH_FORM_WORDS = ["running", "garden", "café notes", "coff"]


def text_of(html):
    return RichTextValue(html, "text/html", "text/x-html-safe")


def make_corpus(site):
    """Add the folder and the objects of the corpus, once."""
    if FOLDER not in site.objectIds():
        site.invokeFactory("Folder", FOLDER, title="Box")
    for oid, (kind, folder, title, description, text) in CORPUS.items():
        container = site[folder] if folder else site
        if oid in container.objectIds():
            continue
        container.invokeFactory(
            kind,
            oid,
            title=title,
            description=description,
            text=text_of(f"<p>{text}</p>"),
        )


def hits(result):
    """The ids of a result, in groups of equal score."""
    groups = []
    last = object()
    for brain in result:
        score = brain.data_record_score_
        if score != last:
            groups.append(set())
            last = score
        groups[-1].add(brain.getId)
    return groups


def rids(result):
    return [brain.getRID() for brain in result]


def search(site, text, **query):
    return site.portal_catalog.unrestrictedSearchResults(SearchableText=text, **query)


def counts():
    queries = catalog._counts_now()["queries"]
    return queries.get("native", 0), queries.get("python", 0)


def wait_text(storage, key, timeout=TIMEOUT):
    """Wait until the text index of the catalog `key` is ready and holds
    every commit."""
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        found = storage._catalog_counts()["texts"].get(key)
        if found and found["ready"] and found["zoe_text_commit_lag_seconds"] == 0:
            return found
        time.sleep(0.05)
    raise AssertionError(f"the text index is not ready: {found}")


@contextlib.contextmanager
def text_queries(storage):
    """Yield the list of the calls of `text_query` of `storage`."""
    calls = []
    original = storage.text_query

    def spy(*args, **kwargs):
        calls.append(args)
        return original(*args, **kwargs)

    with mock.patch.object(storage, "text_query", spy):
        yield calls


@contextlib.contextmanager
def python_path():
    """Make the hook refuse the text index, so each text query takes the
    Python path."""
    with mock.patch.dict(catalog._CLAUSES):
        del catalog._CLAUSES[catalog._TEXT]
        yield


class TextTests(unittest.TestCase):
    layer = zoe_testing.ZOE_TEXT_SITE
    maxDiff = None

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        with opened(self.db) as site:
            make_corpus(site)
            transaction.commit()
            self.key = key_of(site)
        wait_ready(self.storage, self.key)
        wait_text(self.storage, self.key)
        catalog._counts.clear()

    def index(self, site, name="SearchableText"):
        return site.portal_catalog._catalog.getIndex(name)


class TestTheIndex(TextTests):
    def test_the_profile_gives_text_indexes_with_their_settings(self):
        with opened(self.db) as site:
            for name in textindex.TEXT_INDEXES:
                index = self.index(site, name)
                self.assertIsInstance(index.aq_base, textindex.ZoeTextIndex)
                settings = index.getSettings()
                self.assertEqual(settings["mode"], "enhanced")
                self.assertEqual(settings["language"], ["en"])
                self.assertEqual(settings["indexed_attrs"], [name])
            self.assertEqual(self.index(site).boosts, textindex.BOOSTS)
        value = self.storage.kv_get("durable", textindex.NAMESPACE, self.key)
        import json

        indexes = json.loads(value)["indexes"]
        self.assertEqual(sorted(indexes), sorted(textindex.TEXT_INDEXES))
        self.assertEqual(indexes["SearchableText"]["boosts"], textindex.BOOSTS)
        self.assertEqual(indexes["Title"]["fuzzy"], 0)

    def test_a_save_writes_only_deltas_of_the_text_indexes(self):
        with opened(self.db) as site:
            doc = site["n1"]
            doc.setTitle("Garden tools changed")
            doc.reindexObject()
            processQueue()
            jar = site._p_jar
            written = {type(obj).__name__ for obj in jar._registered_objects}
            text_oids = set()
            for name in textindex.TEXT_INDEXES:
                index = self.index(site, name).aq_base
                text_oids.add(index._p_oid)
            changed = {obj._p_oid for obj in jar._registered_objects}
            self.assertFalse(changed & text_oids, written)
            deltas = textindex._deltas(jar, create=False)
            self.assertIsNotNone(deltas)
            rid = site.portal_catalog.getrid("/plone/n1")
            self.assertIn((self.key, "Title", rid), deltas.texts)
            self.assertEqual(
                deltas.texts[(self.key, "Title", rid)][1], "Garden tools changed"
            )
            # No index of the profile keeps a tree of its own.
            for name in textindex.TEXT_INDEXES:
                index = self.index(site, name).aq_base
                self.assertFalse(
                    [v for v in vars(index).values() if hasattr(v, "_p_oid")]
                )

    def test_the_queries_give_the_expected_hits_natively_and_on_the_python_path(
        self,
    ):
        got = {}
        with opened(self.db) as site:
            for text in QUERIES:
                for name, query in FILTERS.items():
                    before = counts()
                    native = search(site, text, **query)
                    self.assertEqual(counts(), (before[0] + 1, before[1]), (text, name))
                    got[(text, name)] = hits(native)
                    with python_path(), text_queries(self.storage) as calls:
                        stock = search(site, text, **query)
                        self.assertEqual(rids(stock), rids(native), (text, name))
                        self.assertTrue(calls, (text, name))
        self.assertEqual(got, EXPECTED)

    def test_the_search_form_in_the_default_mode(self):
        got = {}
        with opened(self.db) as site:
            for words in SEARCH_FORM_WORDS:
                text = munge_search_term(words)
                got[words] = hits(search(site, text, b_start=0, b_size=10))
        self.assertEqual(got, SEARCH_FORM)

    def test_a_transaction_sees_its_own_texts_natively(self):
        with opened(self.db) as site:
            before = counts()
            site.invokeFactory("Document", "t-new", title="Zebra crossing")
            self.assertEqual([b.getId for b in search(site, "zebra")], ["t-new"])
            site["t-new"].setTitle("Giraffe crossing")
            site["t-new"].reindexObject()
            self.assertEqual(list(search(site, "zebra")), [])
            self.assertEqual([b.getId for b in search(site, "giraffe")], ["t-new"])
            site["n5"].text = text_of("<p>Walnut cake</p>")
            site["n5"].reindexObject()
            self.assertEqual({b.getId for b in search(site, "walnut")}, {"n5"})
            self.assertEqual({b.getId for b in search(site, "slowly")}, set())
            point = transaction.savepoint()
            site["n4"].text = text_of("<p>Hazelnut cake</p>")
            site["n4"].reindexObject()
            site.invokeFactory("Document", "t-later", title="Hazelnut tart")
            self.assertEqual(
                {b.getId for b in search(site, "hazelnut")}, {"n4", "t-later"}
            )
            point.rollback()
            self.assertEqual({b.getId for b in search(site, "hazelnut")}, set())
            self.assertEqual({b.getId for b in search(site, "tea")}, {"n4"})
            self.assertEqual({b.getId for b in search(site, "walnut")}, {"n5"})
            # Each search of the test, and those of the creations, are
            # native.
            self.assertEqual(counts()[1], before[1])
            self.assertGreaterEqual(counts()[0], before[0] + 9)

    def test_a_snapshot_outside_the_mirror_window_gives_its_own_texts(self):
        old = self.db.open(transaction_manager=transaction.TransactionManager())
        try:
            from Testing.makerequest import makerequest

            app = makerequest(old.root()["Application"])
            # Commits past the window, each with a new text.
            for k in range(zoe_testing.TEXT_WINDOW_TIDS + 2):
                with opened(self.db) as site:
                    site["n5"].text = text_of(f"<p>Hazelnut {k}</p>")
                    site["n5"].reindexObject()
                    transaction.commit()
            catalog._counts.clear()
            found = app.plone.portal_catalog.unrestrictedSearchResults(
                SearchableText="slowly"
            )
            self.assertEqual({b.getId for b in found}, {"n5"})
            again = app.plone.portal_catalog.unrestrictedSearchResults(
                SearchableText="hazelnut"
            )
            self.assertEqual(list(again), [])
            counted = catalog._counts_now()
            self.assertEqual(counted["queries"].get("native", 0), 0)
            self.assertEqual(
                counted["fallbacks"].get(("outside_window", "server")), 2, counted
            )
        finally:
            old.transaction_manager.abort()
            old.close()
            with opened(self.db) as site:
                site["n5"].text = text_of(f"<p>{CORPUS['n5'][4]}</p>")
                site["n5"].reindexObject()
                transaction.commit()

    def test_saves_of_different_documents_commit_and_of_one_document_conflict(self):
        first = transaction.TransactionManager()
        second = transaction.TransactionManager()
        c1 = self.db.open(transaction_manager=first)
        c2 = self.db.open(transaction_manager=second)
        try:
            from Testing.makerequest import makerequest

            s1 = makerequest(c1.root()["Application"]).plone
            s2 = makerequest(c2.root()["Application"]).plone
            # The queue of CMFCore follows the transaction of the thread:
            # each save indexes at once, in the transaction of its
            # Connection.
            # The indexer of the body text of Plone needs the current site.
            idxs = list(textindex.TEXT_INDEXES)
            old_site = getSite()
            try:
                setSite(s1)
                s1["n1"].text = text_of("<p>Maple leaves</p>")
                s1.portal_catalog._reindexObject(s1["n1"], idxs=idxs)
                setSite(s2)
                s2["n2"].text = text_of("<p>Birch leaves</p>")
                s2.portal_catalog._reindexObject(s2["n2"], idxs=idxs)
            finally:
                setSite(old_site)
            first.commit()
            second.commit()
            # Two texts of one rid, from one snapshot, with no record.
            first.begin()
            second.begin()
            rid = s1.portal_catalog.getrid("/plone/n3")
            index1 = s1.portal_catalog._catalog.getIndex("Description")
            index2 = s2.portal_catalog._catalog.getIndex("Description")
            index1.index_object(rid, types.SimpleNamespace(Description="Oak"))
            index2.index_object(rid, types.SimpleNamespace(Description="Elm"))
            first.commit()
            with self.assertRaises(ConflictError):
                second.commit()
        finally:
            first.abort()
            second.abort()
            c1.close()
            c2.close()
        with opened(self.db) as site:
            self.assertEqual({b.getId for b in search(site, "maple")}, {"n1"})
            self.assertEqual({b.getId for b in search(site, "birch")}, {"n2"})
            for oid in ("n1", "n2", "n3"):
                doc = site[oid]
                doc.text = text_of(f"<p>{CORPUS[oid][4]}</p>")
                doc.reindexObject()
            transaction.commit()

    def check_only(self, site, expected, text="coffee"):
        # `Title` has no boosts: the query reads that index alone.
        found = site.portal_catalog.unrestrictedSearchResults(Title=text)
        self.assertEqual({b.getId for b in found}, expected)

    def test_a_clear_gives_an_index_of_the_documents_indexed_after_it(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            sizes = {
                name: pc._catalog.getIndex(name).indexSize()
                for name in textindex.TEXT_INDEXES
            }
            pc.manage_clearIndex(["Title"])
            pc.reindexObject(site["n5"], idxs=["Title"])
            self.check_only(site, {"n5"})
            transaction.commit()
        wait_text(self.storage, self.key)
        with opened(self.db) as site:
            self.check_only(site, {"n5"})
            pc = site.portal_catalog
            index = pc._catalog.getIndex("Title")
            extra = types.SimpleNamespace(
                indexed_attrs="Title", lexicon_id=index.lexicon_id, language="en"
            )
            pc.delIndex("Title")
            pc.addIndex(
                "Title", textindex.ZoeTextIndex("Title", extra=extra, caller=pc)
            )
            pc.reindexObject(site["n4"], idxs=["Title"])
            self.check_only(site, set(), "coffee")
            self.check_only(site, {"n4"}, "cafe")
            transaction.commit()
        wait_text(self.storage, self.key)
        with opened(self.db) as site:
            self.check_only(site, {"n4"}, "cafe")
            self.check_only(site, set(), "coffee")
            site.portal_catalog.clearFindAndRebuild()
            self.check_only(site, {"n5", "n7"})
            transaction.commit()
        wait_text(self.storage, self.key)
        with opened(self.db) as site:
            self.check_only(site, {"n5", "n7"})
            pc = site.portal_catalog
            # The rebuild gave each entry a new rid: no delta of an old rid
            # counts.
            for name in textindex.TEXT_INDEXES:
                index = pc._catalog.getIndex(name)
                self.assertEqual(index.indexSize(), sizes[name], name)

    def test_a_change_of_the_boosts_applies_to_the_next_query(self):
        def scored(site):
            return [(b.getId, b.data_record_score_) for b in search(site, "coffee")]

        with opened(self.db) as site:
            first = scored(site)
            self.index(site).setSettings(
                boosts={"Title": 0.1, "Description": 0.1, "SearchableText": 1.0}
            )
            transaction.commit()
        try:
            deadline = time.monotonic() + TIMEOUT
            while True:
                with opened(self.db) as site:
                    now = scored(site)
                if now != first or time.monotonic() > deadline:
                    break
                time.sleep(0.05)
            self.assertNotEqual(now, first)
        finally:
            with opened(self.db) as site:
                self.index(site).setSettings(boosts=textindex.BOOSTS)
                transaction.commit()

    def test_the_counts_give_the_values_of_text_info(self):
        with opened(self.db) as site:
            jar = site._p_jar
            tid = catalog._start_of(jar)
            for name in textindex.TEXT_INDEXES:
                index = self.index(site, name)
                info = self.storage.text_info(self.key, name, tid)
                self.assertEqual(index.numObjects(), info["words"], name)
                self.assertEqual(index.indexSize(), info["docs"], name)
                rid = site.portal_catalog.getrid("/plone/n4")
                entry = self.storage.text_info(self.key, name, tid, rid)
                words = index.getEntryForObject(rid)
                self.assertEqual(words, index._split(entry["text"]), name)
            self.assertEqual(
                self.index(site, "Title").getEntryForObject(
                    site.portal_catalog.getrid("/plone/n4")
                ),
                ["cafe", "notes"],
            )
            self.assertIsNone(self.index(site).getEntryForObject(2**31 - 1))

    def test_the_uninstall_step_gives_full_zctextindexes_and_the_backend_refuses(
        self,
    ):
        with self.assertRaises(Unsupported):
            catalog.uninstall()
        self.assertTrue(catalog.installed())
        with opened(self.db) as site:
            site.portal_setup.runAllImportStepsFromProfile(
                "profile-zoe.plone:text-uninstall"
            )
            transaction.commit()
        try:
            with opened(self.db) as site:
                pc = site.portal_catalog
                entries = len(pc._catalog.uids)
                for name in textindex.TEXT_INDEXES:
                    index = pc._catalog.getIndex(name)
                    self.assertIsInstance(index.aq_base, ZCTextIndex)
                    self.assertEqual(index.indexSize(), entries, name)
                self.assertIsNone(
                    self.storage.kv_get("durable", textindex.NAMESPACE, self.key)
                )
                # A storage without the extension text: the profile step
                # refuses and changes nothing.
                with mock.patch.object(self.storage, "text_supported", lambda: False):
                    with self.assertRaises(Unsupported):
                        site.portal_setup.runAllImportStepsFromProfile(
                            "profile-zoe.plone:text"
                        )
                    for name in textindex.TEXT_INDEXES:
                        self.assertIsInstance(
                            pc._catalog.getIndex(name).aq_base, ZCTextIndex
                        )
                transaction.abort()
        finally:
            with opened(self.db) as site:
                site.portal_setup.runAllImportStepsFromProfile("profile-zoe.plone:text")
                transaction.commit()
            wait_text(self.storage, self.key)

    def test_an_index_on_a_storage_without_text_raises_unsupported(self):
        with opened(self.db) as site:
            index = self.index(site)
            with mock.patch.object(self.storage, "text_supported", lambda: False):
                with self.assertRaises(Unsupported):
                    index.index_object(1, types.SimpleNamespace(SearchableText="x"))
                with self.assertRaises(Unsupported):
                    search(site, "coffee")
            with mock.patch.dict(catalog._installed, {}, clear=True):
                with self.assertRaises(Unsupported):
                    index.index_object(1, types.SimpleNamespace(SearchableText="x"))


def scored(result):
    """The rid, the score and the normalized score of each brain."""
    return [
        (b.getRID(), b.data_record_score_, b.data_record_normalized_score_)
        for b in result
    ]


class TestTheOptions(TextTests):
    """The options of a query with a text clause, natively and on the
    Python path, and the other ways to change an index."""

    def same_on_both_paths(self, site, text, **query):
        before = counts()
        native = scored(search(site, text, **query))
        self.assertEqual(counts(), (before[0] + 1, before[1]), query)
        with python_path():
            stock = scored(search(site, text, **query))
        self.assertEqual(native, stock, query)
        return native

    def test_b_start_sort_limit_sort_on_and_normalized_scores(self):
        with opened(self.db) as site:
            whole = self.same_on_both_paths(site, "coffee OR stars")
            self.assertEqual(len(whole), 4)
            self.assertEqual(whole[0][2], 100)
            self.assertTrue(all(0 < norm <= 100 for _r, _s, norm in whole))
            page = self.same_on_both_paths(site, "coffee OR stars", b_start=1, b_size=2)
            # A page after the first keeps the scores of the whole result.
            self.assertEqual(page, whole[1:3])
            self.same_on_both_paths(site, "coffee OR stars", b_start=2)
            self.assertEqual(
                self.same_on_both_paths(site, "coffee OR stars", sort_limit=2),
                whole[:2],
            )
            self.same_on_both_paths(site, "coffee OR stars", sort_on="sortable_title")
            self.same_on_both_paths(
                site, "coffee OR stars", sort_on="sortable_title", sort_limit=2
            )
            self.same_on_both_paths(
                site,
                "coffee OR stars",
                sort_on="sortable_title",
                sort_order="reverse",
                b_start=1,
                b_size=2,
            )

    def test_a_language_clause_without_a_language_index(self):
        with opened(self.db) as site:
            self.assertNotIn("Language", site.portal_catalog._catalog.indexes)
            for language in ("fr", "en-us", ["en", ""]):
                found = self.same_on_both_paths(site, "coffee", Language=language)
                self.assertEqual(len(found), 3, language)

    def test_a_language_clause_with_a_language_index(self):
        def ids_of(site, language):
            native = [b.getId for b in search(site, "coffee", Language=language)]
            with python_path():
                stock = [b.getId for b in search(site, "coffee", Language=language)]
            self.assertEqual(native, stock, language)
            return set(native)

        with opened(self.db) as site:
            pc = site.portal_catalog
            pc.addIndex("Language", "FieldIndex")
            for name in textindex.TEXT_INDEXES:
                self.index(site, name).setSettings(language=["en", "fr"])
            site["n5"].language = "fr"
            pc.reindexIndex("Language", None)
            site["n5"].reindexObject()
            transaction.commit()
        try:
            wait_text(self.storage, self.key)
            with opened(self.db) as site:
                index = self.index(site)
                cat = site.portal_catalog._catalog
                fields = {
                    "fr": ["fr"],
                    "fr-ca": ["fr"],
                    "": ["en"],
                    "de": ["en"],
                    ("fr", ""): None,
                }
                for language, field in fields.items():
                    query = {
                        "Language": list(language)
                        if isinstance(language, tuple)
                        else language
                    }
                    self.assertEqual(index._languages_of(cat, query), field, language)
                self.assertEqual(ids_of(site, "fr"), {"n5"})
                self.assertEqual(ids_of(site, "fr-ca"), set())
                # The neutral items are in the field of the first language.
                self.assertEqual(ids_of(site, ["fr", ""]), {"n4", "n5", "n7"})
                self.assertEqual(ids_of(site, ""), {"n4", "n7"})
        finally:
            with opened(self.db) as site:
                site.portal_catalog.delIndex("Language")
                for name in textindex.TEXT_INDEXES:
                    self.index(site, name).setSettings(language=["en"])
                site["n5"].language = ""
                site["n5"].reindexObject()
                transaction.commit()
            wait_text(self.storage, self.key)

    def test_an_empty_text_counts_as_a_document(self):
        with opened(self.db) as site:
            before = self.index(site, "Title").indexSize()
            site["n3"].setTitle("")
            site["n3"].reindexObject()
            transaction.commit()
        try:
            wait_text(self.storage, self.key)
            with opened(self.db) as site:
                index = self.index(site, "Title")
                self.assertEqual(index.indexSize(), before)
                rid = site.portal_catalog.getrid("/plone/n3")
                self.assertEqual(index.getEntryForObject(rid), [])
        finally:
            with opened(self.db) as site:
                site["n3"].setTitle(CORPUS["n3"][2])
                site["n3"].reindexObject()
                transaction.commit()

    def test_the_meta_type_adds_the_index(self):
        with opened(self.db) as site:
            pc = site.portal_catalog
            extra = types.SimpleNamespace(
                indexed_attrs="Title", lexicon_id="plone_lexicon", language="en"
            )
            pc.addIndex("zoe_probe", "ZoeTextIndex", extra)
            index = pc._catalog.getIndex("zoe_probe")
            self.assertIsInstance(index.aq_base, textindex.ZoeTextIndex)
            self.assertEqual(index.getSettings()["indexed_attrs"], ["Title"])
            self.assertEqual(index.getSettings()["language"], ["en"])

    def test_the_settings_follow_the_transaction_of_the_connection(self):
        manager = transaction.TransactionManager()
        connection = self.db.open(transaction_manager=manager)
        try:
            from Testing.makerequest import makerequest

            site = makerequest(connection.root()["Application"]).plone
            index = site.portal_catalog._catalog.getIndex("Description")
            index.setSettings(boosts={"Title": 2.0})
            manager.commit()
            value = self.storage.kv_get("durable", textindex.NAMESPACE, self.key)
            import json

            boosts = json.loads(value)["indexes"]["Description"]["boosts"]
            self.assertEqual(boosts, {"Title": 2.0})
            index.setSettings(boosts={})
            manager.commit()
        finally:
            manager.abort()
            connection.close()


class TestAStepThatFailsHalfway(TextTests):
    """A profile step that fails after some batches: the old index serves,
    and a second import finishes the work and loses no document."""

    def import_failing(self, profile, after):
        from Products.CMFPlone.CatalogTool import CatalogTool

        calls = []
        stock = CatalogTool.catalog_object

        def failing(tool, *args, **kwargs):
            calls.append(1)
            if len(calls) == after:
                raise ConflictError("a conflict of the test")
            return stock(tool, *args, **kwargs)

        with (
            mock.patch.object(textindex, "BATCH_SIZE", 2),
            mock.patch.object(CatalogTool, "catalog_object", failing),
            opened(self.db) as site,
        ):
            with self.assertRaises(ConflictError):
                site.portal_setup.runAllImportStepsFromProfile(profile)
            transaction.abort()

    def import_again(self, profile):
        with mock.patch.object(textindex, "BATCH_SIZE", 2), opened(self.db) as site:
            site.portal_setup.runAllImportStepsFromProfile(profile)
            transaction.commit()

    def check(self, kind, pending=False):
        """Check that the indexes of `kind` serve and hold every object.
        With `pending`, the pending indexes of a failed step are there."""
        with opened(self.db) as site:
            pc = site.portal_catalog
            entries = len(pc._catalog.uids)
            keys = [k for k in pc._catalog.indexes if k.startswith(textindex.PENDING)]
            expected = [textindex.PENDING + n for n in textindex.TEXT_INDEXES]
            self.assertEqual(sorted(keys), sorted(expected) if pending else [])
            for name in textindex.TEXT_INDEXES:
                index = pc._catalog.getIndex(name)
                self.assertIsInstance(index.aq_base, kind, name)
                self.assertEqual(index.getId(), name)
                self.assertEqual(index.indexSize(), entries, name)
            self.assertEqual({b.getId for b in search(site, "stars")}, {"n6"})

    def test_both_steps_resume_after_a_failure(self):
        self.import_failing("profile-zoe.plone:text-uninstall", after=5)
        # The text index still serves, and the batches before the failure
        # are committed in the pending indexes.
        self.check(textindex.ZoeTextIndex, pending=True)
        self.import_again("profile-zoe.plone:text-uninstall")
        self.check(ZCTextIndex)
        self.import_failing("profile-zoe.plone:text", after=5)
        self.check(ZCTextIndex, pending=True)
        self.import_again("profile-zoe.plone:text")
        wait_text(self.storage, self.key)
        self.check(textindex.ZoeTextIndex)
