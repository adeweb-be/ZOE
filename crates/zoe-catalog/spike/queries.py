"""The query set of the measures on catalog-1m, as plans.

Usage: python queries.py VENDOR PLAN CORPUS OUT

The text terms of the plan of the fixture (four bands of document
frequency), and synthetic queries made of the words of the corpus: AND,
OR, AND NOT, phrases and prefixes. Each line holds the plan that
`zoe.plone.textquery` gives for the parse tree of ZCTextIndex.
"""

import itertools
import json
import random
import sys

sys.argv, args = sys.argv[:2], sys.argv[2:]
import zctext  # noqa: E402  (installs the lexicon of plone_lexicon)

from Products.ZCTextIndex.Lexicon import Lexicon  # noqa: E402
from Products.ZCTextIndex.QueryParser import QueryParser  # noqa: E402


def main():
    plan_path, corpus, out = args
    rng = random.Random(6)
    lexicon = Lexicon(zctext.splitter.Splitter(), zctext.splitter.I18NNormalizer())
    plan = json.load(open(plan_path))
    bands = {}
    for q in plan["queries"]:
        if q["kind"] == "text":
            bands.setdefault(q["band"], []).append(q["query"]["SearchableText"])
    lines = [json.loads(line) for line in itertools.islice(open(corpus), 0, 20000, 7)]
    texts = [d["SearchableText"].split() for d in lines if len(d["SearchableText"].split()) > 20]
    vocab = sorted({w.strip(".").lower() for t in texts for w in t if w.strip(".").isalpha()})
    queries = []

    def add(kind, index, q):
        tree = QueryParser(lexicon).parseQuery(q)
        queries.append({"kind": kind, "index": index, "query": q, "tree": zctext.textquery.plan(tree, lexicon)})

    for band, terms in bands.items():
        for t in terms:
            add(f"word-{band}", "SearchableText", t)
    for _ in range(20):
        a, b = rng.sample(vocab, 2)
        add("and", "SearchableText", f"{a} {b}")
    for _ in range(10):
        a, b = rng.sample(vocab, 2)
        add("or", "SearchableText", f"{a} OR {b}")
    for _ in range(5):
        a, b = rng.sample(vocab, 2)
        add("and-not", "SearchableText", f"{a} AND NOT {b}")
    for t in rng.sample(texts, 10):
        i = rng.randrange(len(t) - 2)
        add("phrase", "SearchableText", '"' + " ".join(w.strip(".") for w in t[i : i + 2]) + '"')
    for w in rng.sample(vocab, 10):
        add("prefix", "SearchableText", w[:3] + "*")
    # The search form of Plone: `munge_search_term` joins the words with
    # AND and adds `*` to the last one.
    for k in (1, 2, 3):
        for _ in range(8):
            ws = rng.sample(vocab, k)
            add(f"plone-{k}", "SearchableText", " AND ".join(ws) + "*")
    for index in ("Title", "Description"):
        for w in rng.sample(vocab, 10):
            add(f"word-{index}", index, w)
    with open(out, "w") as f:
        for q in queries:
            f.write(json.dumps(q) + "\n")
    print(len(queries), "queries")


if __name__ == "__main__":
    main()
