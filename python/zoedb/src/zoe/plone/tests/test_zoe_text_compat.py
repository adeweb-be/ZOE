"""Acceptance test of the mode `compat` of `ZoeTextIndex` (ADR 0060,
section 11).

The test runs on `ZOE_TEXT_SITE`: a Plone site in a store of its own, with
the backend `catalog` of `zoe.plone`. Its catalog gets, for each index of
the corpus of `text_corpus` (`SearchableText`, `Title` and `Description`),
a ZCTextIndex of the type "Okapi BM25 Rank" on the lexicon `plone_lexicon`
of the site (`zc_<id>`) and a `ZoeTextIndex` in the mode `compat` on the
same attribute and lexicon (`compat_<id>`). Both get the same documents:
objects whose attributes hold the texts of the corpus, catalogued with
`catalogObject`, changed, uncatalogued, and a clear of both `Description`
indexes, state by state as `text_corpus.states()` gives them.

At each state, each query of the set of `text_compat.rs`
(`text_corpus.queries`) gives on the `ZoeTextIndex` the rids, the order,
the counts, the scores and the normalized scores of the ZCTextIndex, or
the same error: through `catalog_query`, with the hook of the backend,
and through `text_query`, on the Python path. Some queries also run with
`b_start` and `b_size`. The state `staged` runs in a transaction that
changes the documents of the state `edits` and does not commit, so the
`ZoeTextIndex` sends the staged texts with each query.

`just plone zoe backends` runs this test.
"""

import unittest

import transaction
from Products.ZCatalog.Catalog import Catalog
from Products.ZCTextIndex.ZCTextIndex import ZCTextIndex

from zoe.plone import catalog, textindex
from zoe.plone import testing as zoe_testing
from zoe.plone.tests import text_corpus
from zoe.plone.tests.test_catalog_mirror import key_of, opened, wait_ready
from zoe.plone.tests.test_zoe_text_index import python_path, wait_text

#: The id of the ZCTextIndex of an index of the corpus.
ZC = "zc_{}"
#: The id of the `ZoeTextIndex` of an index of the corpus.
ZOE = "compat_{}"
#: The lexicon of the Plone site.
LEXICON = "plone_lexicon"


class _Extra:
    """The extra record of an index."""


class Doc:
    """A catalogued object: its texts as attributes, None for no text."""

    def __init__(self, texts):
        for name in text_corpus.INDEXES:
            setattr(self, name, texts.get(name))


def native_count():
    """The searches that the hook answered natively."""
    return catalog._counts_now()["queries"].get("native", 0)


def uid_of(rid):
    """The path of the document of the corpus `rid` in the catalog."""
    return f"/plone/compat-text/{rid}"


def answer(search, catalog, query):
    """The rid, the score and the normalized score of each brain of the
    result of `search(catalog, query)`, its `len()` and its
    `actual_result_count`; or the name of the error that it raises."""
    try:
        result = search(catalog, query)
        found = [
            (b.getRID(), b.data_record_score_, b.data_record_normalized_score_)
            for b in result
        ]
    except Exception as e:
        return type(e).__name__
    return found, len(result), getattr(result, "actual_result_count", len(result))


class TestTheCompatibilityMode(unittest.TestCase):
    layer = zoe_testing.ZOE_TEXT_SITE
    maxDiff = None

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        with opened(self.db) as site:
            pc = site.portal_catalog
            for name in text_corpus.INDEXES:
                extra = _Extra()
                extra.doc_attr = name
                extra.lexicon_id = LEXICON
                extra.index_type = "Okapi BM25 Rank"
                pc._catalog.addIndex(
                    ZC.format(name),
                    ZCTextIndex(ZC.format(name), extra=extra, caller=pc),
                )
                extra = _Extra()
                extra.indexed_attrs = name
                extra.lexicon_id = LEXICON
                extra.mode = "compat"
                pc._catalog.addIndex(
                    ZOE.format(name),
                    textindex.ZoeTextIndex(ZOE.format(name), extra=extra, caller=pc),
                )
            transaction.commit()
            self.key = key_of(site)
        wait_ready(self.storage, self.key)
        wait_text(self.storage, self.key)
        catalog._counts.clear()

    def tearDown(self):
        with opened(self.db) as site:
            pc = site.portal_catalog._catalog
            for uid in [u for u in pc.uids if u.startswith(uid_of(""))]:
                pc.uncatalogObject(uid)
            for name in text_corpus.INDEXES:
                pc.delIndex(ZC.format(name))
                pc.delIndex(ZOE.format(name))
            transaction.commit()
        wait_text(self.storage, self.key)

    def ids(self):
        return [f.format(n) for n in text_corpus.INDEXES for f in (ZC, ZOE)]

    def apply(self, site, before, after, clear=False):
        """Catalog the documents of the texts `after` over the texts
        `before`: a new or changed document, and an uncatalogued one.
        `clear` clears both `Description` indexes first."""
        pc = site.portal_catalog._catalog
        if clear:
            pc.getIndex(ZC.format("Description")).clear()
            pc.getIndex(ZOE.format("Description")).clear()
        docs = {}
        for name, texts in after.items():
            for rid, text in texts.items():
                docs.setdefault(rid, {})[name] = text
        old = {rid for texts in before.values() for rid in texts}
        for rid in sorted(old - set(docs)):
            pc.uncatalogObject(uid_of(rid))
        for rid, texts in sorted(docs.items()):
            changed = clear and "Description" in texts
            for name in text_corpus.INDEXES:
                if before.get(name, {}).get(rid) != texts.get(name):
                    changed = True
            if changed:
                pc.catalogObject(Doc(texts), uid_of(rid), idxs=self.ids())

    def check(self, site, state):
        """Each query of each index gives the answer of the ZCTextIndex on
        the `ZoeTextIndex`, through `catalog_query` and through
        `text_query`. Return the number of the native answers."""
        pc = site.portal_catalog._catalog
        stock = catalog._installed["stock"]
        native = 0
        for name in text_corpus.INDEXES:
            for i, (family, query) in enumerate(text_corpus.queries(name)):
                options = [{}]
                if i % 7 == 0:
                    options.append({"b_start": 1, "b_size": 3})
                for extra in options:
                    what = f"{name} {family} {query!r} {extra} at {state}"
                    zc = {ZC.format(name): query, **extra}
                    zoe = {ZOE.format(name): query, **extra}
                    expected = answer(stock, pc, zc)
                    before = native_count()
                    found = answer(Catalog.search, pc, zoe)
                    native += native_count() - before
                    self.assertEqual(found, expected, f"catalog_query: {what}")
                    with python_path():
                        found = answer(Catalog.search, pc, zoe)
                    self.assertEqual(found, expected, f"text_query: {what}")
        return native

    def test_each_query_gives_the_results_of_zctextindex(self):
        states = text_corpus.states()
        previous = {}
        native = 0
        for state in text_corpus.STATES:
            clear = state == "clear"
            with opened(self.db) as site:
                self.apply(site, previous, states[state], clear=clear)
                transaction.commit()
            wait_text(self.storage, self.key)
            with opened(self.db) as site:
                native += self.check(site, state)
            if state == "edits":
                # A transaction that stages the texts of `staged` and does
                # not commit.
                with opened(self.db) as site:
                    self.apply(site, states["edits"], states["staged"])
                    self.check(site, "staged")
            previous = states[state]
        # The mirror answered most queries natively.
        self.assertGreater(native, 800)

    def test_the_hook_sends_the_text_clause_of_the_mode_compat(self):
        with opened(self.db) as site:
            pc = site.portal_catalog._catalog
            index = pc.getIndex(ZOE.format("SearchableText"))
            self.assertEqual(index.getSettings()["mode"], "compat")
            # A query in the mode `compat` reads the index alone, and no
            # field of a language.
            self.assertEqual(index._reads(), [ZOE.format("SearchableText")])
            plan, reason = catalog.plan_of(pc, {ZOE.format("SearchableText"): "kalo"})
            self.assertIsNone(reason)
            (clause,) = plan["c"]
            self.assertEqual(clause["k"], "t")
            self.assertEqual(clause["q"], {"n": "atom", "w": ["kalo"]})
            self.assertIsNone(clause["lg"])
