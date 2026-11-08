"""Acceptance tests of the query translator of `ZoeTextIndex` (ADR 0060,
section 12).

`zoe.plone.textindex.translate(lexicon, query)` parses a query with the
`QueryParser` and the pipeline of a lexicon of ZCTextIndex, and gives the
nodes of the text clause. The tests check, on the lexicon of ZCTextIndex
with its stop words and on the lexicon of Plone (`plone_lexicon`):

- each construct of the grammar of ZCTextIndex gives its node, with the
  words after the lexicon;
- a glob gives the pattern as the user wrote it;
- a glob that starts with `*` or `?` raises `QueryError`;
- each query that the parser of ZCTextIndex refuses raises its error.

`bin/test -s zoe.plone` of the instance of the zoe scenario runs them.
"""

import unittest

from Products.CMFPlone.UnicodeSplitter.splitter import I18NNormalizer
from Products.CMFPlone.UnicodeSplitter.splitter import Splitter as PloneSplitter
from Products.ZCTextIndex.Lexicon import (
    CaseNormalizer,
    Lexicon,
    Splitter,
    StopWordRemover,
)
from Products.ZCTextIndex.ParseTree import ParseError, QueryError
from Products.ZCTextIndex.QueryParser import QueryParser

from zoe.plone.textindex import translate


def atom(*words):
    return {"n": "atom", "w": list(words)}


def phrase(*words):
    return {"n": "phrase", "w": list(words)}


def glob(pattern):
    return {"n": "glob", "p": pattern}


def and_(items, nots=()):
    return {"n": "and", "items": list(items), "nots": list(nots)}


def or_(*items):
    return {"n": "or", "items": list(items)}


class WhitespaceSplitter:
    """A splitter that keeps each run of characters between spaces, so a
    glob can start with `*` or `?`."""

    def process(self, lst):
        return [w for s in lst for w in s.split()]

    processGlob = process


class TestTheTranslator(unittest.TestCase):
    def setUp(self):
        # The lexicon of ZCTextIndex, with its stop words.
        self.stock = Lexicon(Splitter(), CaseNormalizer(), StopWordRemover())
        # The lexicon of Plone.
        self.plone = Lexicon(PloneSplitter(), I18NNormalizer())

    def test_each_construct_gives_its_node_with_the_words_after_the_lexicon(self):
        cases = [
            ("Coffee", atom("coffee")),
            ("coffee tea", and_([atom("coffee"), atom("tea")])),
            ("coffee AND tea", and_([atom("coffee"), atom("tea")])),
            ("coffee and tea", and_([atom("coffee"), atom("tea")])),
            ("coffee OR tea", or_(atom("coffee"), atom("tea"))),
            ("coffee AND NOT tea", and_([atom("coffee")], [atom("tea")])),
            ("coffee -tea", and_([atom("coffee")], [atom("tea")])),
            ('"night sky"', phrase("night", "sky")),
            ("night-sky", phrase("night", "sky")),
            ('coffee -"night sky"', and_([atom("coffee")], [phrase("night", "sky")])),
            (
                "(coffee OR tea) AND beans",
                and_([or_(atom("coffee"), atom("tea")), atom("beans")]),
            ),
            (
                "coffee OR tea AND NOT milk",
                or_(atom("coffee"), and_([atom("tea")], [atom("milk")])),
            ),
            # A stop word leaves the query, as in ZCTextIndex.
            ("the coffee", atom("coffee")),
            ('"the night sky"', phrase("night", "sky")),
        ]
        for query, node in cases:
            self.assertEqual(translate(self.stock, query), node, query)

    def test_the_words_come_from_the_pipeline_of_the_lexicon_of_plone(self):
        cases = [
            # The normalizer of Plone puts the words in lower case and
            # folds the accents of the words in Latin letters.
            ("Café", atom("cafe")),
            ("Élan vital", and_([atom("elan"), atom("vital")])),
            ('"Crème brûlée"', phrase("creme", "brulee")),
        ]
        for query, node in cases:
            self.assertEqual(translate(self.plone, query), node, query)

    def test_a_glob_gives_the_pattern_as_written(self):
        cases = [
            (self.stock, "Coff*", glob("Coff*")),
            (self.stock, "co?fee", glob("co?fee")),
            (self.plone, "Café*", glob("Café*")),
            (self.plone, '"Run*"', glob("Run*")),
        ]
        for lexicon, query, node in cases:
            self.assertEqual(translate(lexicon, query), node, query)
        self.assertEqual(
            translate(self.stock, "beans -Coff*"),
            and_([atom("beans")], [glob("Coff*")]),
        )
        self.assertEqual(
            translate(self.plone, "beans AND Café*"),
            and_([atom("beans"), glob("Café*")]),
        )

    def test_a_glob_that_starts_with_a_wildcard_raises_query_error(self):
        lexicon = Lexicon(WhitespaceSplitter(), CaseNormalizer())
        for query in ("*offee", "?offee", "beans AND *offee", "beans -?offee"):
            with self.assertRaises(QueryError, msg=query):
                translate(lexicon, query)

    def test_each_query_that_the_parser_refuses_raises_its_error(self):
        queries = [
            "",
            "AND",
            "coffee AND",
            "OR coffee",
            "(coffee",
            "coffee)",
            "-coffee",
            "NOT coffee",
            "the",
            "the AND an",
        ]
        for query in queries:
            with self.assertRaises(ParseError, msg=query) as stock:
                QueryParser(self.stock).parseQuery(query)
            with self.assertRaises(ParseError, msg=query) as ours:
                translate(self.stock, query)
            self.assertEqual(str(ours.exception), str(stock.exception), query)
