"""Write the test data of `tests/text_compat.rs`: the results of ZCTextIndex
with the lexicon `plone_lexicon` of Plone 6.1 on the corpus and the query
set of `zoe.plone.tests.text_corpus`.

    target/plone/zoe/parts/instance/bin/interpreter \\
        crates/zoe-catalog/tests/data/text_compat.py \\
        crates/zoe-catalog/tests/data/text_compat.json

Run it with the interpreter of the zoe scenario of `just plone` (`just
plone zoe build`): CPython 3.13, Products.ZCatalog 7.2 with
Products.ZCTextIndex, Products.CMFPlone 6.1 with its splitter, plone.i18n
and Unidecode 1.4.0, and zoedb, whose translator gives the plan of each
query.

The lexicon has the pipeline of `plone_lexicon`: the splitter "Unicode
Whitespace splitter" and the normalizer "Unicode Ignoring Accents Case
Normalizer" of Products.CMFPlone. Each text index of the corpus is an
`OkapiIndex` over that lexicon, as ZCTextIndex makes it for the type
"Okapi BM25 Rank". The script applies the changes of each state of the
corpus with `index_doc` and `unindex_doc`, and a clear with a new
`OkapiIndex`, as `ZCTextIndex.clear` does. For each state and each query,
it runs the parse tree of `QueryParser` on the index, as
`ZCTextIndex.query_index` does, and writes the hits in the order of
`Catalog.search`, `IIBucket.byValue(0)`: the score descending, then the rid
descending. An empty query gives no hit, as `query_index` gives `None` for
it. A query that the parser refuses gives the name of its error.

The JSON: `states` gives the operations of each state (`set`, `unindex`,
`clear`), with the state `staged` over `edits`; `queries` gives, for each
index, each query with its family, its plan (`null` for an error or an
empty query) and its hits at each state (`[[rid, score]...]`), or its
error.
"""

import importlib.util
import json
import pathlib
import random
import sys

from Products.CMFPlone.UnicodeSplitter.splitter import I18NNormalizer, Splitter
from Products.ZCTextIndex.Lexicon import Lexicon
from Products.ZCTextIndex.OkapiIndex import OkapiIndex
from Products.ZCTextIndex.ParseTree import ParseError, QueryError
from Products.ZCTextIndex.QueryParser import QueryParser

from zoe.plone.textindex import translate

HERE = pathlib.Path(__file__).resolve().parent
CORPUS = (
    HERE.parents[3]
    / "python"
    / "zoedb"
    / "src"
    / "zoe"
    / "plone"
    / "tests"
    / "text_corpus.py"
)


def load_corpus():
    spec = importlib.util.spec_from_file_location("text_corpus", CORPUS)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def results(lexicon, index, queries):
    """The hits of each query on `index`, or the name of its error."""
    out = []
    for query in queries:
        if not query:
            out.append([])
            continue
        try:
            tree = QueryParser(lexicon).parseQuery(query)
            found = tree.executeQuery(index)
        except (ParseError, QueryError) as e:
            out.append(type(e).__name__)
            continue
        if not found:
            out.append([])
            continue
        out.append([[rid, score] for score, rid in found.byValue(0)])
    return out


def main():
    (path,) = sys.argv[1:]
    corpus = load_corpus()
    # The ids of new words of the lexicon come from `random`.
    random.seed(297)
    states = corpus.states()
    lexicon = Lexicon(Splitter(), I18NNormalizer())
    indexes = {name: OkapiIndex(lexicon) for name in corpus.INDEXES}
    query_sets = {name: corpus.queries(name) for name in corpus.INDEXES}
    plans = {}
    for name, queries in query_sets.items():
        for _family, query in queries:
            if not query:
                plans[(name, query)] = None
                continue
            try:
                plans[(name, query)] = translate(lexicon, query)
            except (ParseError, QueryError):
                plans[(name, query)] = None
    hits = {name: {} for name in corpus.INDEXES}

    def apply(ops):
        for op in ops:
            if op[0] == "set":
                _, name, rid, text = op
                indexes[name].index_doc(rid, [text])
            elif op[0] == "unindex":
                _, name, rid = op
                if indexes[name].has_doc(rid):
                    indexes[name].unindex_doc(rid)
            else:
                _, name = op
                indexes[name] = OkapiIndex(lexicon)

    def record(state):
        for name, queries in query_sets.items():
            hits[name][state] = results(
                lexicon, indexes[name], [q for _f, q in queries]
            )

    operations = {}
    previous = {}
    for state in corpus.STATES:
        ops = []
        if state == "clear":
            ops.append(("clear", "Description"))
            before = dict(previous)
            before["Description"] = {}
        else:
            before = previous
        ops.extend(corpus.changes(before, states[state]))
        apply(ops)
        record(state)
        operations[state] = ops
        previous = states[state]
        if state == "edits":
            # The state `staged` over `edits`, in indexes of its own.
            saved = dict(indexes)
            staged_ops = corpus.changes(states["edits"], states["staged"])
            for name in corpus.INDEXES:
                fresh = OkapiIndex(lexicon)
                for rid, text in states["staged"][name].items():
                    fresh.index_doc(rid, [text])
                indexes[name] = fresh
            record("staged")
            operations["staged"] = staged_ops
            indexes.update(saved)
    out = {
        "states": {
            state: [list(op) for op in ops] for state, ops in operations.items()
        },
        "queries": {
            name: [
                {
                    "family": family,
                    "query": query,
                    "plan": plans[(name, query)],
                    "hits": {state: hits[name][state][i] for state in hits[name]},
                }
                for i, (family, query) in enumerate(queries)
            ]
            for name, queries in query_sets.items()
        },
    }
    with open(path, "w") as f:
        json.dump(out, f, ensure_ascii=False, separators=(",", ":"))
        f.write("\n")
    counts = {name: len(q) for name, q in query_sets.items()}
    print(f"{len(operations)} states, queries {counts}")


main()
