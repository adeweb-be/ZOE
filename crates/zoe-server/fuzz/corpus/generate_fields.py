"""Write the seeds of the fuzz target `fields`.

Run it from the root of the repository with ZODB, DateTime and zoedb, for
example in the environment of the Python tests:

    python/.venv-3.13/bin/python crates/zoe-server/fuzz/corpus/generate_fields.py

A seed is a conflict of three records of a persistent object, as ZODB
writes them: a flag byte (the three low bits pick the default strategy, 0
strict, 1 lww, 2 max, 3 sum, 4 union, 5 append; bit 3 couples the fields
`a` and `b`), the lengths of the old and of the committed record, each as
2 bytes big-endian, then the old, the committed and the new record. Two
seeds hold a record of a merge policy and a record of a root that names
it. All data is synthetic.
"""

import struct
import sys
from io import BytesIO
from pathlib import Path

import persistent
from DateTime import DateTime
from ZODB._compat import PersistentPickler, _protocol
from zoe.policy import MergePolicy

HERE = Path(__file__).resolve().parent / "fields"

STRICT, LWW, MAX, SUM, UNION, APPEND = range(6)
COUPLED = 8


class Item(persistent.Persistent):
    pass


Item.__module__ = "m"
Item.__qualname__ = Item.__name__ = "Item"
sys.modules.setdefault("m", type(sys)("m")).Item = Item


def record(klass, state, references=None):
    """Return the record of an object of klass in state, as ObjectWriter
    writes it."""
    f = BytesIO()
    pickler = PersistentPickler(references or (lambda o: None), f, _protocol)
    pickler.dump(klass)
    pickler.dump(state)
    return f.getvalue()


def seed(flags, old, committed, new):
    records = [record(Item, old), record(Item, committed), record(Item, new)]
    head = struct.pack(">BHH", flags, len(records[0]), len(records[1]))
    return head + b"".join(records)


BASE = {
    "title": "t-0",
    "a": 1,
    "b": 2,
    "tags": ["x", "y"],
    "log": [("created", 0)],
    "modified": DateTime("2026/01/02 03:04:05 UTC"),
    "set": {1, 2},
}


def changed(**fields):
    return {**BASE, **fields}


POLICY = record(
    MergePolicy,
    {
        "version": 1,
        "classes": {
            "m.Item": {
                "tier2": "on",
                "default": "strict",
                "fields": {"modified": "max", "tags": "union"},
                "coupled": [["a", "b"]],
            }
        },
    },
)


class Ref:
    pass


def root():
    f = BytesIO()
    from persistent.mapping import PersistentMapping

    def pid(obj):
        if isinstance(obj, Ref):
            return (b"\0" * 7 + b"\1", MergePolicy)
        return None

    pickler = PersistentPickler(pid, f, _protocol)
    pickler.dump(PersistentMapping)
    pickler.dump({"data": {"zoe.merge_policy": Ref()}})
    return f.getvalue()


SEEDS = {
    "two-fields": seed(STRICT, BASE, changed(title="t-1"), changed(a=5)),
    "lww": seed(LWW, BASE, changed(title="t-1"), changed(title="t-2")),
    "max-date": seed(
        MAX,
        BASE,
        changed(modified=DateTime("2026/02/02 UTC")),
        changed(modified=DateTime("2026/03/02 UTC")),
    ),
    "sum": seed(SUM, BASE, changed(a=4), changed(a=7)),
    "union": seed(UNION, BASE, changed(tags=["x", "z"]), changed(tags=["y", "w"])),
    "append": seed(
        APPEND,
        BASE,
        changed(log=[("created", 0), ("one", 1)]),
        changed(log=[("created", 0), ("two", 2)]),
    ),
    "coupled": seed(COUPLED, BASE, changed(a=5), changed(b=6)),
    "set": seed(STRICT, BASE, changed(set={2, 1}), changed(title="t-2")),
}


def main():
    HERE.mkdir(exist_ok=True)
    for name, data in SEEDS.items():
        (HERE / name).write_bytes(data)
    # The readers of the policy and of the root take the new record.
    for name, data in (("policy", POLICY), ("root", root())):
        head = struct.pack(">BHH", STRICT, 0, 0)
        (HERE / name).write_bytes(head + data)


if __name__ == "__main__":
    main()
