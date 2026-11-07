"""Write the seeds of the fuzz target `plan`.

Run it from the root of the repository with msgpack, for example in the
environment of the Python tests:

    python/.venv-3.13/bin/python crates/zoe-server/fuzz/corpus/generate_plan.py

A seed is the argument `plan` of a `catalog_query` call, one msgpack value
in the form of ADR 0059, section 8: plans of each kind of clause, with a
sort, a batch and columns, and plans that the server refuses. All data is
synthetic.
"""

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
    "unknown-kind": plan([{"k": "t", "i": "SearchableText", "q": "word"}]),
    "unknown-key": {**plan([keys("getId", ["a"])]), "o": [["getId", 1]]},
    "malformed": {"c": "not a list"},
    "empty": plan([]),
}


def main():
    HERE.mkdir(exist_ok=True)
    for name, value in SEEDS.items():
        (HERE / name).write_bytes(msgpack.packb(value, use_bin_type=True))


if __name__ == "__main__":
    main()
