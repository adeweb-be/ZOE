"""Generate the synthetic Data.fs fixture of the zoe-convert tests.

Run it from this directory in a throwaway environment:

    uv run --no-project --python 3.13 --with ZODB==6.3 --with BTrees==6.5 \
        --with persistent==6.8 --with zodbpickle==4.5 python generate.py

The script commits synthetic objects to a FileStorage in a temporary directory,
with a clock that starts at a fixed time, so that every run writes the same
bytes. It copies the result here:

- `Data.fs`, the FileStorage file;
- `blobs-bushy/`, its blob directory in the bushy layout;
- `blobs-lawn/`, the same blob files in the lawn layout;
- `expected.txt`, what ZODB reads from the file, in file order.

The history holds transactions with a user, a description and extension data,
objects in several revisions, a BTree with buckets, blobs in two revisions, a
pack (so the first transactions have the status "p"), and two undone
transactions after the pack. The undo writes data records with back pointers:
one to an earlier record, and one to no data (an undone creation). All data is
synthetic.

The lines of `expected.txt`:

    txn <tid> <status> <user> <description> <extension>
    rec <oid> <data>
    blob <oid> <tid> <path in blobs-bushy> <path in blobs-lawn>

A `txn` line starts each transaction. Its `rec` lines follow, then the `blob`
lines of its records that have a blob file. Oids and tids are 16 hex digits.
Byte fields are hex with the prefix "x", so an empty field is "x". The data of
a record without data is "-". Paths use "/".
"""

import shutil
import tempfile
import time
from pathlib import Path

import transaction
import ZODB
from BTrees.OOBTree import OOBTree
from persistent.list import PersistentList
from persistent.mapping import PersistentMapping
from ZODB.blob import Blob, BushyLayout, LawnLayout
from ZODB.FileStorage import FileStorage
from ZODB.utils import u64

HERE = Path(__file__).resolve().parent


class FixedClock:
    """A clock that starts at a fixed time and moves 7 seconds per read."""

    def __init__(self):
        self.now = 1_000_000_000.0

    def __call__(self):
        self.now += 7.0
        return self.now


def commit(user, description, **extension):
    txn = transaction.get()
    txn.user = user
    txn.description = description
    for key, value in extension.items():
        txn.extension[key] = value
    txn.commit()


def undo_last(db, description):
    db.undo(db.undoLog(0, 1)[0]["id"])
    commit("someone", description)


def build(work, clock):
    db = ZODB.DB(FileStorage(str(work / "Data.fs"), blob_dir=str(work / "blobs")))
    connection = db.open()
    root = connection.root()

    root["folder"] = folder = PersistentMapping()
    folder["doc-1"] = PersistentMapping(title="first title", body="x" * 300)
    folder["doc-2"] = PersistentMapping(title="second title", body="y" * 100)
    # More keys than one bucket holds, so the tree has buckets of its own.
    root["tree"] = tree = OOBTree()
    for i in range(80):
        tree[f"key-{i:03}"] = i
    root["list"] = PersistentList(["a", "b"])
    commit("someone", "create content", source="generate.py")

    folder["doc-1"]["title"] = "first title, changed"
    commit("someone", "edit a document")

    root["file"] = Blob(b"first blob revision\n" * 30)
    commit("editor", "add a blob", size=600)

    with root["file"].open("w") as f:
        f.write(b"second blob revision\n" * 40)
    commit("editor", "change the blob")
    pack_time = clock.now + 1

    del folder["doc-2"]
    commit("", "")

    # The pack keeps what is current at the pack time and marks the
    # transactions before it as packed.
    db.pack(t=pack_time)

    root["counter"] = PersistentMapping(value=1)
    commit("someone", "add a counter", note="after the pack")
    root["counter"]["value"] = 2
    commit("someone", "count")
    undo_last(db, "undo the count")

    with root["file"].open("w") as f:
        f.write(b"third blob revision\n" * 50)
    commit("editor", "change the blob again")

    root["temporary"] = PersistentMapping(note="temporary")
    commit("someone", "add a temporary object")
    undo_last(db, "undo the temporary object")

    connection.close()
    db.close()


def copy_blobs(source, bushy, lawn):
    """Copy the blob files of `source` to `bushy` and `lawn` in each layout."""
    layouts = {bushy: BushyLayout(), lawn: LawnLayout()}
    for target in layouts:
        shutil.rmtree(target, ignore_errors=True)
    for path in sorted(source.rglob("*.blob")):
        relative = path.relative_to(source)
        oid = BushyLayout().path_to_oid(str(relative.parent))
        tid = bytes.fromhex(path.stem[2:].rjust(16, "0"))
        for target, layout in layouts.items():
            copy = target / layout.getBlobFilePath(oid, tid)
            copy.parent.mkdir(parents=True, exist_ok=True)
            shutil.copyfile(path, copy)
    for target, layout in [(bushy, "bushy"), (lawn, "lawn")]:
        target.mkdir(exist_ok=True)
        (target / ".layout").write_text(layout)


def describe(data_fs, bushy, lawn):
    """Return the lines of `expected.txt` for `data_fs`."""
    storage = FileStorage(str(data_fs), read_only=True)
    lines = []
    try:
        for txn in storage.iterator():
            fields = [txn.user, txn.description, txn.extension_bytes]
            fields = " ".join("x" + bytes(field).hex() for field in fields)
            status = "x" + txn.status.encode().hex()
            lines.append(f"txn {u64(txn.tid):016x} {status} {fields}")
            blobs = []
            for record in txn:
                data = "-" if record.data is None else "x" + record.data.hex()
                lines.append(f"rec {u64(record.oid):016x} {data}")
                paths = [
                    Path(layout.getBlobFilePath(record.oid, record.tid))
                    for layout in (BushyLayout(), LawnLayout())
                ]
                if (bushy / paths[0]).exists():
                    assert (lawn / paths[1]).exists()
                    oid, tid = u64(record.oid), u64(record.tid)
                    names = " ".join(path.as_posix() for path in paths)
                    blobs.append(f"blob {oid:016x} {tid:016x} {names}")
            lines.extend(blobs)
    finally:
        storage.close()
    return lines


def main():
    clock = FixedClock()
    time.time = clock
    with tempfile.TemporaryDirectory() as tmp:
        work = Path(tmp)
        build(work, clock)
        shutil.copyfile(work / "Data.fs", HERE / "Data.fs")
        copy_blobs(work / "blobs", HERE / "blobs-bushy", HERE / "blobs-lawn")
    lines = describe(HERE / "Data.fs", HERE / "blobs-bushy", HERE / "blobs-lawn")
    (HERE / "expected.txt").write_text("\n".join(lines) + "\n")
    print(f"{len(lines)} lines in expected.txt")


if __name__ == "__main__":
    main()
