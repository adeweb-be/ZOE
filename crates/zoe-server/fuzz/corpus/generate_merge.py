"""Write the seeds of the fuzz target `merge`.

Run it from the root of the repository with ZODB and BTrees, for example
in the environment of the Python tests:

    python/.venv-3.13/bin/python crates/zoe-server/fuzz/corpus/generate_merge.py

A seed is a conflict of three records, as ZODB writes them: a flag byte
(1 turns the posting-dict rule on, 2 puts it in shadow mode), the lengths of the old and of the
committed record, each as 2 bytes big-endian, then the old, the committed
and the new record. All data is synthetic.
"""

import struct
from io import BytesIO
from pathlib import Path

from BTrees.IIBTree import IIBucket, IISet
from BTrees.IOBTree import IOBTree, IOBucket
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree, OOBucket
from ZODB._compat import PersistentPickler, _protocol

HERE = Path(__file__).resolve().parent / "merge"


def record(obj):
    """Return the record of obj, as ObjectWriter writes it."""
    f = BytesIO()
    pickler = PersistentPickler(lambda o: None, f, _protocol)
    pickler.dump(type(obj))
    pickler.dump(obj.__getstate__())
    return f.getvalue()


def seed(old, committed, new, posting=False):
    records = [record(old), record(committed), record(new)]
    head = struct.pack(">BHH", int(posting), len(records[0]), len(records[1]))
    return head + b"".join(records)


SEEDS = {
    "length": seed(Length(10), Length(12), Length(7)),
    "iibucket-inserts": seed(IIBucket({1: 1}), IIBucket({1: 1, 2: 2}), IIBucket({1: 1, 3: 3})),
    "iiset-delete": seed(IISet([1, 2, 3]), IISet([1, 2]), IISet([1, 2, 3, 4])),
    "oobucket-strs": seed(
        OOBucket({"a": "x", "b": ("t", 1)}),
        OOBucket({"a": "y", "b": ("t", 1)}),
        OOBucket({"a": "x", "b": ("t", 1), "c": b"z"}),
    ),
    "oobtree-inline": seed(OOBTree({"a": 1}), OOBTree({"a": 1, "b": 2}), OOBTree({"a": 1, "c": 3})),
    "iobtree-postings": seed(
        IOBTree({10: {1: 3, 2: 1}}),
        IOBTree({10: {1: 3, 2: 1, 3: 4}}),
        IOBTree({10: {1: 3, 2: 1, 4: 2}}),
        posting=True,
    ),
    "iobucket-floats": seed(
        IOBucket({10: {1: 0.5}}), IOBucket({10: {1: 0.5, 2: 0.25}}), IOBucket({10: {1: 0.5, 3: 1.5}}), True
    ),
}

HERE.mkdir(exist_ok=True)
for name, data in SEEDS.items():
    (HERE / name).write_bytes(data)
