"""The corpus and the query set of the compatibility mode of `ZoeTextIndex`
(ADR 0060, section 11).

`crates/zoe-catalog/tests/data/text_compat.py` writes the results of
ZCTextIndex on them for `crates/zoe-catalog/tests/text_compat.rs`, and
`test_zoe_text_compat.py` runs them on a catalog with both indexes. The
module needs only the standard library, so the generator loads it by its
path.

The texts hold generated words and the character classes of the splitter
of Plone: Latin with accents, Latin extended, Greek, Cyrillic, CJK, kana,
Hangul, Thai, Arabic, Devanagari, numbers, compatibility characters and
combining marks, with the separators of the finding of the text index. No
natural language. A seeded generator that uses only `random()` makes the
same corpus on each version of Python.

`states()` gives the texts of each index at each state: `base`, `edits`
(texts changed, documents unindexed, new documents), `more` (more
changes), `clear` (`Description` cleared, then a few of its texts again),
and `staged`: the texts of `edits` with the changes that a transaction
stages over that state. `changes()` gives the operations from one state to
the next. A text is a str, an empty str for an entry of no word, or absent
for no entry.
"""

import random

#: The text indexes of the corpus, as in a Plone site.
INDEXES = ("SearchableText", "Title", "Description")

#: The order of the states.
STATES = ("base", "edits", "more", "clear")


def _pool(ranges):
    return [chr(c) for a, b in ranges for c in range(a, b + 1)]


POOLS = {
    "latin-accents": _pool([(0xC0, 0xD6), (0xD8, 0xF6), (0xF8, 0xFF)]),
    "latin-ext": _pool([(0x100, 0x17F), (0x180, 0x24F)]),
    "greek": _pool([(0x391, 0x3A1), (0x3A3, 0x3A9), (0x3B1, 0x3C9)]),
    "cyrillic": _pool([(0x410, 0x44F)]),
    "cjk": _pool([(0x4E00, 0x4E40)]),
    "kana": _pool([(0x3041, 0x3060), (0x30A1, 0x30C0)]),
    "hangul": _pool([(0xAC00, 0xAC40)]),
    "thai": _pool([(0x0E01, 0x0E2E), (0x0E30, 0x0E3A)]),
    "arabic": _pool([(0x0627, 0x064A), (0x0660, 0x0669)]),
    "devanagari": _pool([(0x0915, 0x0939), (0x093E, 0x094C)]),
    "numbers": list("0123456789²³¹½¼Ⅻⅳ٣۵"),
    "compat": list("ﬁﬂﬀＡＢＣａｂｃ①②"),
    "marks": ["é", "a̱", "ö", "ñ", "i̇", "é"],
}

#: The separators of the finding.
SEPARATORS = [" ", " ", " ", "-", "'", ".", ", ", "/", " ", "_", "’", "\n"]

ACCENTS = {
    "a": "àáâä",
    "e": "èéêë",
    "i": "ìíîï",
    "o": "òóôö",
    "u": "ùúûü",
    "c": "ç",
    "n": "ñ",
}


def _int(rng, low, high):
    """A random int in [low, high]."""
    return low + int(rng.random() * (high - low + 1))


def _pick(rng, items):
    return items[int(rng.random() * len(items))]


def _sample(rng, items, k):
    items = list(items)
    out = []
    for _ in range(min(k, len(items))):
        out.append(items.pop(int(rng.random() * len(items))))
    return out


def vocabulary(rng, size=300):
    """Generated words of consonants and vowels, each once."""
    words = []
    seen = set()
    while len(words) < size:
        word = "".join(
            _pick(rng, "bcdfgklmnprstvz") + _pick(rng, "aeiou")
            for _ in range(_int(rng, 2, 4))
        )
        if word not in seen:
            seen.add(word)
            words.append(word)
    return words


def accent(rng, word):
    """`word` with some accents."""
    return "".join(
        _pick(rng, ACCENTS[c]) if c in ACCENTS and rng.random() < 0.4 else c
        for c in word
    )


def special(rng, vocab):
    """A word of the characters of one class, alone, inside a word of the
    vocabulary, or after it."""
    chars = POOLS[_pick(rng, sorted(POOLS))]
    word = "".join(_pick(rng, chars) for _ in range(_int(rng, 1, 5)))
    shape = rng.random()
    if shape < 0.3:
        base = _pick(rng, vocab)
        cut = _int(rng, 0, len(base))
        word = base[:cut] + word + base[cut:]
    elif shape < 0.4:
        word = _pick(rng, vocab).capitalize() + word
    return word


def text(rng, vocab, low=3, high=30):
    """A text of generated words and separators."""
    tokens = []
    for _ in range(_int(rng, low, high)):
        if rng.random() < 0.65:
            # The first words of the vocabulary are frequent.
            w = _pick(rng, vocab[:40] if rng.random() < 0.4 else vocab)
            w = _pick(rng, [w, w, w.upper(), w.capitalize(), accent(rng, w)])
        else:
            w = special(rng, vocab)
        tokens.append(w)
    out = ""
    for t in tokens:
        out += t + _pick(rng, SEPARATORS)
    return out


def _document(rng, vocab):
    """The texts of one document: `Title` and `Description` may be absent,
    and `Description` empty."""
    body = text(rng, vocab)
    if rng.random() < 0.1:
        # A long text with a word many times.
        word = _pick(rng, vocab[:40])
        body = body + (" " + word) * _int(rng, 3, 9) + " " + text(rng, vocab, 20, 60)
    texts = {"SearchableText": body}
    if rng.random() < 0.9:
        texts["Title"] = text(rng, vocab, 1, 5)
    roll = rng.random()
    if roll < 0.1:
        texts["Description"] = ""
    elif roll < 0.8:
        texts["Description"] = body[: len(body) // 2]
    return texts


def _rid(rng, used):
    while True:
        rid = _int(rng, -(2**31), 2**31 - 1)
        if rid not in used:
            used.add(rid)
            return rid


def _seeded():
    rng = random.Random(297)
    return rng, vocabulary(rng)


def states():
    """The texts of each index at each state: `{state: {index: {rid:
    text}}}`, with the state `staged` over `edits`."""
    rng, vocab = _seeded()
    used = set()
    docs = {}
    for _ in range(240):
        docs[_rid(rng, used)] = _document(rng, vocab)

    def snapshot(docs):
        return {
            index: {rid: t[index] for rid, t in docs.items() if index in t}
            for index in INDEXES
        }

    out = {"base": snapshot(docs)}
    rids = sorted(docs)
    # edits: changed texts, unindexed documents, new documents.
    for rid in _sample(rng, rids, 50):
        docs[rid] = _document(rng, vocab)
    for rid in _sample(rng, sorted(docs), 20):
        del docs[rid]
    for _ in range(15):
        docs[_rid(rng, used)] = _document(rng, vocab)
    out["edits"] = snapshot(docs)
    edits = {rid: dict(t) for rid, t in docs.items()}
    # more: more changes of one index at a time.
    for rid in _sample(rng, sorted(docs), 25):
        index = _pick(rng, INDEXES)
        docs[rid][index] = text(rng, vocab, 1, 12)
    for rid in _sample(rng, sorted(docs), 8):
        docs[rid].pop(_pick(rng, INDEXES), None)
    out["more"] = snapshot(docs)
    # clear: `Description` cleared, then a few of its texts again.
    for rid in sorted(docs):
        docs[rid].pop("Description", None)
    for rid in _sample(rng, sorted(docs), 30):
        docs[rid]["Description"] = text(rng, vocab, 2, 10)
    out["clear"] = snapshot(docs)
    # staged: the changes of a transaction over `edits`.
    for rid in _sample(rng, sorted(edits), 6):
        edits[rid] = _document(rng, vocab)
    for rid in _sample(rng, sorted(edits), 3):
        del edits[rid]
    for _ in range(3):
        edits[_rid(rng, used)] = _document(rng, vocab)
    out["staged"] = snapshot(edits)
    return out


def changes(before, after):
    """The operations from the texts `before` to the texts `after` of the
    indexes, in order: `("set", index, rid, text)` and `("unindex", index,
    rid)`. A clear is not among them."""
    ops = []
    for index in INDEXES:
        old, new = before.get(index, {}), after.get(index, {})
        for rid in sorted(set(old) | set(new)):
            if rid not in new:
                ops.append(("unindex", index, rid))
            elif old.get(rid) != new[rid]:
                ops.append(("set", index, rid, new[rid]))
    return ops


def queries(index):
    """The query set of `index`, from the texts of the state `base`: a list
    of `(family, query)`."""
    rng, vocab = _seeded()
    rng = random.Random(f"queries-{index}")
    texts = [t for t in states()["base"][index].values() if t]
    sample = []
    for t in _sample(rng, texts, 120):
        sample.extend(t.split())
    words = [w.strip(".,'/-’_") for w in sample]
    words = [w for w in words if w and "*" not in w and "?" not in w]
    common = vocab[:40]
    out = []

    def add(family, query):
        out.append((family, query))

    for w in _sample(rng, vocab, 15):
        add("word", w)
    for w in _sample(rng, words, 15):
        add("word-of-text", w)
    for w in _sample(rng, common, 6):
        add("case", _pick(rng, [w.upper(), w.capitalize()]))
    for w in _sample(rng, common, 6):
        add("accents", accent(rng, w))
    for _ in range(6):
        add("and", f"{_pick(rng, words)} {_pick(rng, common)}")
        add("and-explicit", f"{_pick(rng, common)} AND {_pick(rng, common)}")
        add("or", f"{_pick(rng, vocab)} OR {_pick(rng, words)}")
        add("not", f"{_pick(rng, common)} NOT {_pick(rng, common)}")
        add("and-not", f"{_pick(rng, common)} AND NOT {_pick(rng, vocab)}")
        add(
            "group",
            f"({_pick(rng, common)} OR {_pick(rng, vocab)}) AND {_pick(rng, common)}",
        )
        add(
            "group-not",
            f"{_pick(rng, common)} AND NOT ({_pick(rng, common)} OR {_pick(rng, vocab)})",
        )
    for t in _sample(rng, texts, 24):
        ws = t.split()
        if len(ws) >= 3:
            i = _int(rng, 0, len(ws) - 3)
            n = 2 if rng.random() < 0.5 else 3
            add(f"phrase-{n}", '"' + " ".join(ws[i : i + n]) + '"')
    for w in _sample(rng, common, 4):
        add("phrase-repeat", f'"{w} {w}"')
    for w in _sample(rng, vocab, 10):
        add("glob-prefix", w[: _int(rng, 2, 4)] + "*")
    for w in _sample(rng, vocab, 6):
        i = _int(rng, 1, len(w) - 1)
        add("glob-question", w[:i] + "?" + w[i + 1 :])
    for w in _sample(rng, vocab, 4):
        add("glob-inner", w[:2] + "*" + w[-1])
    for w in _sample(rng, words, 10):
        add("glob-of-text", w[: max(1, len(w) // 2)] + "*")
    for _ in range(4):
        add(
            "mixed",
            f'"{_pick(rng, common)} {_pick(rng, common)}" OR {_pick(rng, vocab)[:3]}*',
        )
    add("empty", "")
    add("only-separators", "- . ,")
    add("and-separators", f"{_pick(rng, common)} AND !!!")
    for q in ("*abc", "?bc", "AND", '"', "a OR", "()", "NOT kalo", "OR OR"):
        add("syntax", q)
    return out
