"""Write the seeds of the fuzz target `plan`.

Run it from the root of the repository with msgpack, for example in the
environment of the Python tests:

    python/.venv-3.13/bin/python crates/zoe-server/fuzz/corpus/generate_plan.py

A seed is the argument `plan` of a `catalog_query` call, one msgpack value
in the form of ADR 0059, section 8: plans of each kind of clause, with a
sort, a batch and columns, plans with a text clause of ADR 0060, section 12,
and plans that the server refuses. A seed whose name starts with `overlay`
also holds the argument `overlay` after the plan: a second msgpack value,
`[[oid, pickle]...]`, with records of BTrees buckets and of the metadata
`data`, one with a provisional oid, written as ZODB writes a record (a
pickle of the class, then a pickle of the state). All data is synthetic.
"""

import io
import pickle
from pathlib import Path

import msgpack

HERE = Path(__file__).resolve().parent / "plan"


def plan(clauses, sort=(), limit=None, b_start=0, b_size=None, cols=None, rn=0):
    return {
        "c": list(clauses),
        "s": [list(s) for s in sort],
        "l": limit,
        "bs": b_start,
        "bz": b_size,
        "cols": cols,
        "rn": rn,
    }


def keys(index, values, conjunction=False, bounds=None, exclude=()):
    return {
        "k": "v",
        "i": index,
        "q": list(values),
        "and": conjunction,
        "r": bounds,
        "not": list(exclude),
    }


def path(index, paths, level=0, depth=-1, navtree=False, start=0):
    return {
        "k": "p",
        "i": index,
        "q": [list(p) for p in paths],
        "lv": level,
        "and": False,
        "d": depth,
        "nt": navtree,
        "ns": start,
        "ip": ["", "site"],
    }


def node(kind, **fields):
    return {"n": kind, **fields}


def text(index, query, languages=None, cleared=(), staged=(), settings=None):
    return {
        "k": "t",
        "i": index,
        "q": query,
        "lg": languages,
        "cl": list(cleared),
        "st": [list(s) for s in staged],
        "set": settings,
    }


SEEDS = {
    "field": plan([keys("portal_type", ["Document"])]),
    "field-many": plan([keys("portal_type", ["Document", "News Item"])]),
    "sorted": plan(
        [keys("portal_type", ["News Item"])],
        sort=[["sortable_title", False]],
        limit=3,
    ),
    "range": plan([keys("sortable_title", [], bounds=["item 0003", "item 0011"])]),
    "open-range": plan([keys("modified", [], bounds=[None, 29000000])]),
    "not": plan([keys("review_state", [], exclude=["private"])]),
    "keywords-and": plan([keys("Subject", ["alpha", "beta"], conjunction=True)]),
    "tuple": plan([keys("getObjPositionInParent", [[1, "a"], [2.5, None]])]),
    "boolean": plan([{"k": "b", "i": "is_folderish", "q": True}]),
    "boolean-nil": plan([{"k": "b", "i": "is_folderish", "q": None}]),
    "date-range": plan([{"k": "d", "i": "effectiveRange", "t": 29000000}]),
    "date-range-nil": plan([{"k": "d", "i": "effectiveRange", "t": None}]),
    "path": plan([path("path", [["/site/folder", None], ["/site/other", 1]], depth=1)]),
    "navtree": plan(
        [path("path", [["/site/folder/doc", None]], navtree=True, start=1)]
    ),
    "batch-rows": plan(
        [keys("portal_type", ["Document"])],
        sort=[["getId", True], ["created", None]],
        b_start=2,
        b_size=3,
        cols=["Title", "getId", "modified"],
        rn=3,
    ),
    "text": plan(
        [
            keys("portal_type", ["Document"]),
            text(
                "SearchableText",
                node(
                    "and",
                    items=[
                        node("atom", w=["alpha"]),
                        node("phrase", w=["beta", "gamma"]),
                        node("or", items=[node("glob", p="delt*"), node("atom", w=[])]),
                    ],
                    nots=[node("atom", w=["omega"])],
                ),
                languages=["en"],
            ),
        ],
        b_size=10,
    ),
    "text-sorted": plan(
        [text("Title", node("atom", w=["alpha"]))],
        sort=[["sortable_title", False]],
        limit=5,
    ),
    "text-staged": plan(
        [
            text(
                "SearchableText",
                node("phrase", w=["alpha", "beta", "gamma"]),
                cleared=["Description"],
                staged=[
                    ["SearchableText", 7, "en", "alpha beta gamma"],
                    ["Title", -3, "", None],
                ],
                settings={
                    "mode": "enhanced",
                    "language": ["en", "fr"],
                    "boosts": {"Title": 3.0, "Description": 1.5},
                },
            )
        ]
    ),
    "unknown-kind": plan([{"k": "o", "i": "getObjPositionInParent", "q": [1]}]),
    "unknown-key": {**plan([keys("getId", ["a"])]), "o": [["getId", 1]]},
    "malformed": {"c": "not a list"},
    "empty": plan([]),
}


# The high bit of a provisional oid.
PROVISIONAL = 1 << 63


def oid(n):
    return n.to_bytes(8, "big")


def record(module, name, state, refs=()):
    """A record as ZODB writes it: the class as a GLOBAL, then the state.
    Each item of `refs` stands for a persistent reference to that oid."""
    klass = b"\x80\x03c" + f"{module}\n{name}\n".encode() + b"q\x00."
    out = io.BytesIO()

    class Ref:
        def __init__(self, n):
            self.n = n

    pickler = pickle.Pickler(out, protocol=3)
    pickler.persistent_id = lambda o: oid(o.n) if isinstance(o, Ref) else None
    pickler.dump(state(*[Ref(n) for n in refs]))
    return klass + out.getvalue()


BUCKET = record(
    "BTrees.IOBTree", "IOBucket", lambda: ((1000, "Document", 1002, "Folder"),)
)
DATA = record("BTrees.IOBTree", "IOBucket", lambda: ((1000, ("item-0", "Item 0")),))
TREE = record(
    "BTrees.IOBTree",
    "IOBTree",
    lambda a, b: ((a, 1002, b), a),
    refs=(12, PROVISIONAL | 1),
)
OVERLAYS = {
    "overlay": [[oid(12), BUCKET], [oid(14), DATA]],
    "overlay-provisional": [
        [oid(11), TREE],
        [oid(12), BUCKET],
        [oid(PROVISIONAL | 1), BUCKET],
    ],
    "overlay-nil": None,
    "overlay-empty": [],
}
MALFORMED_OVERLAY = [[oid(12)], [b"short", BUCKET]]


def main():
    HERE.mkdir(exist_ok=True)
    for name, value in SEEDS.items():
        (HERE / name).write_bytes(msgpack.packb(value, use_bin_type=True))
    query = msgpack.packb(SEEDS["field"], use_bin_type=True)
    for name, value in OVERLAYS.items():
        (HERE / name).write_bytes(query + msgpack.packb(value, use_bin_type=True))
    malformed = msgpack.packb(MALFORMED_OVERLAY, use_bin_type=True)
    (HERE / "overlay-malformed").write_bytes(query + malformed)


if __name__ == "__main__":
    main()
