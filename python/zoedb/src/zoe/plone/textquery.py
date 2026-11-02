"""The query translator of the text index (spike prototype, not merged).

``plan`` turns the parse tree of ZCTextIndex's ``QueryParser`` into the
plan of a text clause: a tree of ``and``, ``or``, ``atom``, ``phrase`` and
``glob`` nodes, with the words that the lexicon gives. The parser and the
lexicon pipeline stay in Python, so the syntax, the stop words and the
errors are those of ZCTextIndex. The server runs the plan on tantivy.
"""

from Products.ZCTextIndex.Lexicon import _text2list
from Products.ZCTextIndex.ParseTree import QueryError


def words(lexicon, text):
    """Return the words of ``text`` as ``Lexicon.termToWordIds`` finds
    them: each element of the pipeline, with ``process_post_glob`` when it
    has one."""
    last = _text2list(text)
    for element in lexicon._pipeline:
        process = getattr(element, "process_post_glob", element.process)
        last = process(last)
    return [w.decode("utf-8") if isinstance(w, bytes) else w for w in last]


def plan(node, lexicon):
    """Return the plan of the parse tree ``node``."""
    kind = node.nodeType()
    if kind == "AND":
        items, nots = [], []
        for sub in node.getValue():
            if sub.nodeType() == "NOT":
                nots.append(plan(sub.getValue(), lexicon))
            else:
                items.append(plan(sub, lexicon))
        return {"op": "and", "items": items, "nots": nots}
    if kind == "OR":
        return {"op": "or", "items": [plan(sub, lexicon) for sub in node.getValue()]}
    if kind == "ATOM":
        return {"op": "atom", "words": words(lexicon, node.getValue())}
    if kind == "PHRASE":
        return {"op": "phrase", "words": words(lexicon, node.getValue())}
    if kind == "GLOB":
        pattern = node.getValue()
        if isinstance(pattern, bytes):
            pattern = pattern.decode("utf-8")
        if pattern[:1] in ("*", "?"):
            raise QueryError(f"pattern {pattern!r} shouldn't start with glob character")
        return {"op": "glob", "pattern": pattern}
    raise QueryError(f"cannot plan a {kind} node")
