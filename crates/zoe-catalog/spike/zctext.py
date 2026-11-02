"""ZCTextIndex on a corpus and a query set: the expected results.

Usage: python zctext.py VENDOR CORPUS DOCS_OUT CASES_OUT

VENDOR holds the splitter and the normalizer of plone_lexicon, copied from
Products.CMFPlone 6.1.5 and plone.i18n 5.1.1. The script indexes the
corpus with an OkapiIndex and the lexicon of plone_lexicon, as ZCTextIndex
does (`index_object`), adds synthetic documents made of generated words of
many scripts, runs a synthetic query set through `QueryParser` and the
index, and writes, for each query, the plan of `zoe.plone.textquery` and
the result in the order of `Catalog.search`: `IIBucket.byValue(0)`.
"""

import importlib.util
import json
import os
import pathlib
import random
import sys
import types

VENDOR = pathlib.Path(sys.argv[1])
HERE = pathlib.Path(__file__).resolve().parent


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    mod = importlib.util.module_from_spec(spec)
    sys.modules[name] = mod
    spec.loader.exec_module(mod)
    return mod


def install_plone_lexicon():
    import Products  # noqa: F401

    for pkg in (
        "plone",
        "plone.i18n",
        "plone.i18n.normalizer",
        "Products.CMFPlone",
        "Products.CMFPlone.UnicodeSplitter",
    ):
        if pkg not in sys.modules:
            mod = types.ModuleType(pkg)
            mod.__path__ = []
            sys.modules[pkg] = mod
    load("plone.i18n.normalizer.base", VENDOR / "plone_i18n_base.py")
    load("Products.CMFPlone.UnicodeSplitter.config", VENDOR / "UnicodeSplitter" / "config.py")
    return load(
        "Products.CMFPlone.UnicodeSplitter.splitter",
        VENDOR / "UnicodeSplitter" / "splitter.py",
    )


splitter = install_plone_lexicon()
sys.path.insert(0, str(HERE.parents[2] / "python" / "zoedb" / "src" / "zoe" / "plone"))
import textquery  # noqa: E402
from Products.ZCTextIndex.Lexicon import Lexicon  # noqa: E402
from Products.ZCTextIndex.OkapiIndex import OkapiIndex  # noqa: E402
from Products.ZCTextIndex.ParseTree import ParseError, QueryError  # noqa: E402
from Products.ZCTextIndex.QueryParser import QueryParser  # noqa: E402

INDEXES = ("SearchableText", "Title", "Description")


def pool(ranges):
    return [chr(c) for a, b in ranges for c in range(a, b + 1)]


# Character pools of the synthetic words: no natural language, only
# generated sequences of characters of each script.
POOLS = {
    "latin-accents": pool([(0xC0, 0xD6), (0xD8, 0xF6), (0xF8, 0xFF)]),
    "latin-ext": pool([(0x100, 0x17F), (0x180, 0x24F)]),
    "greek": pool([(0x391, 0x3A1), (0x3A3, 0x3A9), (0x3B1, 0x3C9)]),
    "cyrillic": pool([(0x410, 0x44F)]),
    "cjk": pool([(0x4E00, 0x4E80)]),
    "kana": pool([(0x3041, 0x3090), (0x30A1, 0x30F0)]),
    "hangul": pool([(0xAC00, 0xAC80)]),
    "thai": pool([(0x0E01, 0x0E2E), (0x0E30, 0x0E3A)]),
    "arabic": pool([(0x0627, 0x064A), (0x0660, 0x0669)]),
    "devanagari": pool([(0x0915, 0x0939), (0x093E, 0x094C)]),
    "numbers": list("0123456789²³¹½¼Ⅻⅳ٣۵"),
    "compat": list("ﬁﬂﬀＡＢＣａｂｃ①②"),
    "marks": ["é", "a̱", "ö", "ñ", "i̇"],
}
SEPARATORS = [" ", " ", " ", "-", "'", ".", ", ", "/", " ", "_", "’"]


def make_vocabulary(rng):
    sys.path.insert(0, str(HERE.parents[3] / "pb" / "bench" / "plone" / "src"))
    try:
        from plonebench import corpus

        return corpus.make_words(random.Random(1), 2000)
    except ImportError:
        return ["".join(rng.choice("bcdfg") + rng.choice("aeiou") for _ in range(3)) for _ in range(2000)]


def special_word(rng, vocab):
    kind = rng.choice(list(POOLS))
    chars = POOLS[kind]
    word = "".join(rng.choice(chars) for _ in range(rng.randint(1, 5)))
    shape = rng.random()
    if shape < 0.3:
        # A word of the vocabulary with characters of the pool inside.
        base = rng.choice(vocab)
        cut = rng.randint(0, len(base))
        word = base[:cut] + word + base[cut:]
    elif shape < 0.4:
        word = rng.choice(vocab).capitalize() + word
    return word


def special_docs(rng, vocab, start, count):
    docs = []
    for n in range(count):
        tokens = []
        for _ in range(rng.randint(3, 30)):
            r = rng.random()
            if r < 0.5:
                w = rng.choice(vocab)
                w = rng.choice([w, w.upper(), w.capitalize(), accent(rng, w)])
            else:
                w = special_word(rng, vocab)
            tokens.append(w)
        text = ""
        for t in tokens:
            text += t + rng.choice(SEPARATORS)
        title = " ".join(tokens[: rng.randint(1, 4)])
        docs.append(
            {
                "rid": start + n,
                "type": "Document",
                "state": "published",
                "creator": "editor-000",
                "effective": n,
                "modified": n,
                "Title": title,
                "Description": text[: len(text) // 2] if n % 7 else "",
                "SearchableText": text,
            }
        )
    return docs


ACCENTS = {"a": "àáâä", "e": "èéêë", "i": "ìíîï", "o": "òóôö", "u": "ùúûü", "c": "ç", "n": "ñ"}


def accent(rng, word):
    out = []
    for c in word:
        if c in ACCENTS and rng.random() < 0.4:
            out.append(rng.choice(ACCENTS[c]))
        else:
            out.append(c)
    return "".join(out)


def queries(rng, vocab, docs, index):
    texts = [d[index] for d in docs if d.get(index)]
    sample_words = []
    for t in rng.sample(texts, min(400, len(texts))):
        sample_words.extend(t.split())
    words = [w.strip(".,'/-") for w in sample_words if w.strip(".,'/-")]
    out = []

    def add(family, q):
        out.append((family, q))

    for w in rng.sample(vocab, 25):
        add("word", w)
    for w in rng.sample(words, 25):
        add("word-of-text", w)
    for w in rng.sample(vocab, 10):
        add("case", rng.choice([w.upper(), w.capitalize()]))
    for w in rng.sample(vocab, 10):
        add("accents", accent(rng, w))
    for _ in range(20):
        add("and", f"{rng.choice(words)} {rng.choice(vocab)}")
        add("and-explicit", f"{rng.choice(vocab)} AND {rng.choice(vocab[:200])}")
        add("or", f"{rng.choice(vocab)} OR {rng.choice(words)}")
        add("not", f"{rng.choice(vocab[:100])} NOT {rng.choice(vocab[:300])}")
        add("and-not", f"{rng.choice(vocab[:100])} AND NOT {rng.choice(vocab)}")
        add("group", f"({rng.choice(vocab)} OR {rng.choice(vocab)}) AND {rng.choice(vocab[:100])}")
    for t in rng.sample(texts, 30):
        ws = t.split()
        if len(ws) >= 3:
            i = rng.randrange(len(ws) - 2)
            add("phrase", '"' + " ".join(ws[i : i + rng.randint(2, 3)]) + '"')
    for w in rng.sample(vocab, 20):
        add("glob-prefix", w[: rng.randint(2, 4)] + "*")
    for w in rng.sample(vocab, 10):
        i = rng.randrange(1, len(w))
        add("glob-question", w[:i] + "?" + w[i + 1 :])
    for w in rng.sample(words, 10):
        add("glob-of-text", w[: max(1, len(w) // 2)] + "*")
    for _ in range(10):
        add("mixed", f'"{rng.choice(vocab)} {rng.choice(vocab)}" OR {rng.choice(vocab)[:3]}*')
    for q in ("*abc", "NOT bocire", "AND", '"', "bo* AND", "()", "-", "a OR", "!!!"):
        add("syntax", q)
    return out


def main():
    corpus, docs_out, cases_out = sys.argv[2:5]
    rng = random.Random(284)
    vocab = make_vocabulary(rng)
    docs = [json.loads(line) for line in open(corpus)]
    docs += special_docs(rng, vocab, 1_000_000, int(os.environ.get("SPECIAL", "3000")))
    with open(docs_out, "w") as f:
        for d in docs:
            f.write(json.dumps(d, ensure_ascii=False) + "\n")
    cases = 0
    if os.environ.get("WORDS"):
        lexicon = Lexicon(splitter.Splitter(), splitter.I18NNormalizer())
        with open(os.environ["WORDS"], "w") as f:
            for d in docs:
                last = [d["SearchableText"]]
                for element in lexicon._pipeline:
                    last = element.process(last)
                words = [w.decode() if isinstance(w, bytes) else w for w in last]
                f.write(json.dumps({"rid": d["rid"], "words": words}, ensure_ascii=False) + "\n")
    with open(cases_out, "w") as out:
        for index in INDEXES:
            lexicon = Lexicon(splitter.Splitter(), splitter.I18NNormalizer())
            okapi = OkapiIndex(lexicon)
            for d in docs:
                text = d.get(index)
                # ZCTextIndex.index_object: no attribute, no entry; an
                # empty text is an entry of no word.
                if text is not None:
                    okapi.index_doc(d["rid"], [text])
            special = [d for d in docs if d["rid"] >= 1_000_000]
            qs = queries(rng, vocab, docs[: len(docs) - len(special)], index)
            if special:
                qs += [(f"special-{f}", q) for f, q in queries(rng, vocab, special, index)]
            for family, q in qs:
                case = {"index": index, "family": family, "query": q}
                try:
                    tree = QueryParser(lexicon).parseQuery(q)
                    case["tree"] = textquery.plan(tree, lexicon)
                    result = tree.executeQuery(okapi)
                except (ParseError, QueryError) as e:
                    case["error"] = type(e).__name__
                    case.pop("tree", None)
                    out.write(json.dumps(case, ensure_ascii=False) + "\n")
                    cases += 1
                    continue
                if result is None:
                    case["expected"] = []
                    case["all"] = True
                else:
                    case["expected"] = [[rid, score] for score, rid in result.byValue(0)]
                out.write(json.dumps(case, ensure_ascii=False) + "\n")
                cases += 1
    print(f"{len(docs)} docs, {cases} cases")


if __name__ == "__main__":
    main()
