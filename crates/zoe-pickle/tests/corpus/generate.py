"""Generate the synthetic corpus of ZODB records for the zoe-pickle tests.

Run it from this directory in a throwaway environment:

    uv run --no-project --python 3.13 --with ZODB==6.3 --with BTrees==6.5 \
        --with persistent==6.8 --with zodbpickle==4.5 python generate.py

The script commits synthetic objects to a FileStorage in a temporary
directory. Then it reads every stored record back and writes its exact bytes
(the class pickle, then the state pickle) to `<construct>-<n>.record` in this
directory. All data is synthetic.
"""

import datetime
import os
import sys
import tempfile
import types
from collections import Counter
from pathlib import Path

import transaction
import ZODB
from BTrees.IIBTree import IITreeSet
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree, OOBucket
from persistent import Persistent
from persistent.list import PersistentList
from persistent.mapping import PersistentMapping
from persistent.wref import WeakRef
from ZODB.FileStorage import FileStorage
from ZODB.utils import get_pickle_metadata
from zodbpickle import binary

HERE = Path(__file__).resolve().parent

# The construct name of a record that no pair names, by class.
CLASS_CONSTRUCTS = {
    "PersistentMapping": "persistent-mapping",
    "PersistentList": "persistent-list",
    "WithNewArgs": "class-with-newargs",
}

# The pickle of a set follows the iteration order, and the order of str items
# follows the hash seed. A fixed seed makes the corpus reproducible.
if os.environ.get("PYTHONHASHSEED") != "0":
    env = dict(os.environ, PYTHONHASHSEED="0")
    os.execve(sys.executable, [sys.executable, *sys.argv], env)

# The classes and functions below live in a synthetic module, so that the
# pickles name `zoe_corpus` and not `__main__`.
MODULE = types.ModuleType("zoe_corpus")
sys.modules[MODULE.__name__] = MODULE


def in_module(obj):
    obj.__module__ = MODULE.__name__
    setattr(MODULE, obj.__name__, obj)
    return obj


@in_module
class Holder(Persistent):
    """A persistent object that keeps its keyword arguments as attributes."""

    def __init__(self, **attributes):
        for name, value in attributes.items():
            setattr(self, name, value)


@in_module
class WithNewArgs(Persistent):
    """A persistent object with __getnewargs__: ZODB refers to it by oid only."""

    def __getnewargs__(self):
        return ()


@in_module
class Token:
    """A plain object whose __reduce__ names a factory function."""

    def __init__(self, label, rank):
        self.label = label
        self.rank = rank

    def __reduce__(self):
        return make_token, (self.label, self.rank)


@in_module
def make_token(label, rank):
    return Token(label, rank)


@in_module
class Pair:
    """A plain object whose __reduce__ gives a class, arguments and a state."""

    def __init__(self, left, right, note="synthetic"):
        self.left = left
        self.right = right
        self.note = note

    def __reduce__(self):
        return Pair, (self.left, self.right), {"note": self.note}


@in_module
class Plain:
    """A plain object that pickles through copyreg: NEWOBJ, then BUILD."""

    def __init__(self, **attributes):
        self.__dict__.update(attributes)


def constructs():
    """Return (construct, persistent object) pairs, in commit order."""
    target = Holder(name="target-1")
    shared_list = [1, 2, 3]
    recursive_list = ["self"]
    recursive_list.append(recursive_list)
    shared_text = "shared-text"
    big_tree = OOBTree({f"key-{i:04d}": f"value-{i}" for i in range(100)})
    big_set = IITreeSet(range(-300, 3000, 3))

    return [
        # Persistent references: (oid, class), oid only, and weak.
        ("persistent-reference", Holder(ref=target)),
        (
            "persistent-reference",
            Holder(
                refs=[
                    target,
                    target,
                    PersistentMapping(),
                    PersistentList(),
                    Length(1),
                ]
            ),
        ),
        ("persistent-reference", Holder(ref=WithNewArgs())),
        ("persistent-reference", Holder(weak=WeakRef(target))),
        ("class-with-newargs", WithNewArgs()),
        # BTrees: a standalone bucket, trees with one and many buckets, sets.
        ("oobucket", OOBucket({"alpha": 1, "beta": (2, 3), "gamma": None})),
        ("oobtree", OOBTree({"one": 1, "two": 2})),
        ("oobtree", big_tree),
        ("iitreeset", IITreeSet([1, 300, 70000, -5])),
        ("iitreeset", big_set),
        ("length", Length(0)),
        ("length", Length(42)),
        ("length", Length(70000)),
        ("length", Length(-3)),
        # PersistentMapping and PersistentList, with batch boundaries.
        (
            "persistent-mapping",
            PersistentMapping({"title": "item-1", "count": 7, "tags": ["a", "b"]}),
        ),
        ("persistent-mapping", PersistentMapping()),
        (
            "persistent-mapping",
            PersistentMapping({f"k{i}": i for i in range(1001)}),
        ),
        ("persistent-list", PersistentList(["a", 1, 2.5, None])),
        ("persistent-list", PersistentList()),
        ("persistent-list", PersistentList(range(1001))),
        ("persistent-list", PersistentList(range(2001))),
        # zodbpickle.binary: SHORT_BINSTRING and BINSTRING.
        ("zodbpickle-binary", Holder(data=binary(b"short\x00\xff"))),
        ("zodbpickle-binary", Holder(data=binary(b"x" * 300))),
        ("zodbpickle-binary", Holder(data=binary(b""))),
        ("bytes", Holder(short=b"\x00\x01\x02", long=b"y" * 300, empty=b"")),
        # Tuples: empty, counted and MARK-based, nested, with shared items.
        ("tuple", Holder(value=())),
        ("tuple", Holder(one=(1,), two=(1, 2), three=(1, 2, 3))),
        ("tuple", Holder(value=(1, 2, 3, 4, 5), nested=((1,), (2, (3, 4))))),
        ("tuple", Holder(value=(shared_text, shared_text, [shared_text]))),
        # Sets pickle through REDUCE of builtins.set and builtins.frozenset.
        ("set", Holder(value={1, 2, 3})),
        ("set", Holder(value=frozenset({"a", "b", "c"}))),
        ("set", Holder(value=set(), pairs={(1, 2), (3, 4)})),
        # Unicode: ASCII, non-ASCII, long, and a lone surrogate.
        ("unicode", Holder(value="plain-ascii")),
        ("unicode", Holder(a="clé", b="значение", c="数据", d="🔑-key")),
        ("unicode", Holder(value="é" * 300)),
        ("unicode", Holder(value="lone-\ud800-surrogate")),
        # Integers: every width, negative, beyond 64 bits, LONG4.
        (
            "small-int",
            Holder(values=[0, 1, 255, 256, 65535, 65536, 2**31 - 1]),
        ),
        ("negative-int", Holder(values=[-1, -129, -(2**31), -(2**31) - 1])),
        ("large-int", Holder(values=[2**31, 2**63 - 1, 2**64, 2**70, -(2**100)])),
        ("large-int", Holder(value=2**2100, negative=-(2**2100))),
        # Floats, including signed zero, infinities and NaN.
        (
            "float",
            Holder(values=[0.0, -0.0, 1.5, -2.25, 1e300, 5e-324]),
        ),
        ("float", Holder(values=[float("inf"), float("-inf"), float("nan")])),
        (
            "none-and-bool",
            Holder(none=None, yes=True, no=False, all=[None, True, False]),
        ),
        # Shared and recursive values: memo puts and gets.
        ("shared-value", Holder(a=shared_list, b=shared_list, loop=recursive_list)),
        # A custom __reduce__ with a factory function, one with a class and a
        # state, and both kinds twice in one record.
        ("custom-reduce", Holder(value=Token("token-1", 1))),
        ("custom-reduce", Holder(value=Pair(1, "two"))),
        (
            "custom-reduce",
            Holder(
                values=[
                    Token("token-2", 2),
                    Token("token-3", 3),
                    Pair(3, 4),
                    Pair(5, 6),
                ]
            ),
        ),
        # Other inline objects: copyreg (NEWOBJ and BUILD) and datetime.
        ("inline-object", Holder(value=Plain(label="plain-1", size=3))),
        ("datetime", Holder(value=datetime.datetime(1, 2, 3, 4, 5, 6))),
    ]


def main():
    for old in HERE.glob("*.record"):
        old.unlink()

    with tempfile.TemporaryDirectory() as tmp:
        storage = FileStorage(os.path.join(tmp, "Data.fs"))
        db = ZODB.DB(storage)
        conn = db.open()
        root = conn.root()
        pairs = constructs()
        for index, (_, obj) in enumerate(pairs):
            root[f"object-{index:02d}"] = obj
        transaction.commit()
        records = [
            (record.oid, record.data) for txn in storage.iterator() for record in txn
        ]
        db.close()

    # Write the named objects first, in the order of the pairs. The other
    # records (the root, the buckets of the big trees, the targets of
    # references) follow in storage order and take a name from their class.
    order = {obj._p_oid: index for index, (_, obj) in enumerate(pairs)}
    names = {obj._p_oid: construct for construct, obj in pairs}
    records.sort(key=lambda record: order.get(record[0], len(order)))
    counts = Counter()
    for oid, data in records:
        _, klass = get_pickle_metadata(data)
        construct = names.get(oid, CLASS_CONSTRUCTS.get(klass, klass.lower()))
        counts[construct] += 1
        (HERE / f"{construct}-{counts[construct]}.record").write_bytes(data)

    for construct, count in sorted(counts.items()):
        print(f"{construct}: {count}")
    print(f"total: {sum(counts.values())}")


if __name__ == "__main__":
    main()
