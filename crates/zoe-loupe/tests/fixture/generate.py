"""Generate the fixture of the zoe-loupe tests: a small site like a Plone site.

Run it from this directory in a throwaway environment:

    uv run --no-project --python 3.13 --with ZODB==6.3 --with BTrees==6.5 \
        --with persistent==6.8 --with zodbpickle==4.5 python generate.py

The script commits synthetic objects to a FileStorage in a temporary directory,
with a clock that starts at a fixed time, so that every run writes the same
bytes. It writes `records.txt`: the transactions in commit order with their
records, and after each transaction the object that each path of `PATHS` leads
to.

The objects have the shapes of a Plone site:

- the root mapping holds the Zope application under "Application";
- the application keeps its items as attributes, as OFS does;
- a folder keeps its items in an OOBTree under `_tree` and counts them in a
  `Length` under `_count`, as BTreeFolder2 does;
- annotations are an OOBTree under `__annotations__`, as zope.annotation
  writes them.

One folder holds enough items for its tree to have several buckets. A document
holds a Decimal, a value that zoe-pickle keeps as an opaque node. The history
starts with the transaction in which ZODB creates the empty root mapping. Then
it edits a document, adds and removes an item, and renames a folder, so that
each object has a history to travel in. All data is synthetic.

The lines of `records.txt`:

    txn <tid> <user> <description> <extension>
    rec <oid> <data>
    path <path> <oid> <class>
    path <path> -

A `txn` line starts each transaction. Its `rec` lines follow in the order of
the file, then one `path` line for each path of `PATHS`: the oid and the class
of the object that the path leads to after the transaction, or "-" when it
leads to no object. Oids and tids are 16 hex digits. Byte fields are hex with
the prefix "x", so an empty field is "x".
"""

import sys
import tempfile
import time
import types
from decimal import Decimal
from pathlib import Path

import transaction
import ZODB
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree
from persistent import Persistent
from persistent.mapping import PersistentMapping
from ZODB.FileStorage import FileStorage
from ZODB.utils import get_pickle_metadata, u64

HERE = Path(__file__).resolve().parent

# The module that holds the classes of the content, in the class pickles.
MODULE = "app.content"

# The paths that the tests resolve, from the Zope application.
PATHS = [
    "/plone",
    "/plone/folder",
    "/plone/folder/doc",
    "/plone/folder/doc/__annotations__",
    "/plone/folder/doc/__annotations__/app.settings",
    "/plone/folder/doc-2",
    "/plone/big",
    "/plone/big/item-000",
    "/plone/big/item-057",
    "/plone/big/item-099",
    "/plone/missing",
]


class Application(Persistent):
    """The Zope application. It keeps its items as attributes, as OFS does."""

    def __init__(self):
        self._objects = ()

    def add(self, item):
        setattr(self, item.id, item)
        self._objects += ({"id": item.id, "meta_type": type(item).__name__},)


class Item(Persistent):
    """A content item with an id, a title and other attributes."""

    def __init__(self, id, title, **attributes):
        self.id = id
        self.title = title
        for name, value in attributes.items():
            setattr(self, name, value)


class Document(Item):
    """A document."""


class Folder(Item):
    """A folder that keeps its items as BTreeFolder2 does."""

    def __init__(self, id, title):
        super().__init__(id, title)
        self._tree = OOBTree()
        self._count = Length()

    def add(self, item):
        self._tree[item.id] = item
        self._count.change(1)

    def remove(self, id):
        del self._tree[id]
        self._count.change(-1)


class Site(Folder):
    """The site: a folder in the application."""


def register(*classes):
    """Put the classes in the module MODULE, so that the pickles name it."""
    package = types.ModuleType("app")
    module = types.ModuleType(MODULE)
    package.content = module
    sys.modules["app"] = package
    sys.modules[MODULE] = module
    for cls in classes:
        cls.__module__ = MODULE
        setattr(module, cls.__name__, cls)


class FixedClock:
    """A clock that starts at a fixed time and moves 7 seconds per read."""

    def __init__(self):
        self.now = 1_000_000_000.0

    def __call__(self):
        self.now += 7.0
        return self.now


def resolve(app, path):
    """Return the object that a path leads to from the application, or None."""
    obj = app
    for name in path.strip("/").split("/"):
        if isinstance(obj, (OOBTree, PersistentMapping)):
            obj = obj.get(name)
        else:
            obj._p_activate()
            tree = obj.__dict__.get("_tree")
            if tree is not None and name in tree:
                obj = tree[name]
            else:
                obj = obj.__dict__.get(name)
        if not isinstance(obj, Persistent):
            return None
    return obj


def commit(db, app, paths, user, description):
    """Commit the transaction and note where each path leads after it."""
    txn = transaction.get()
    txn.user = user
    txn.description = description
    txn.commit()
    lines = []
    for path in PATHS:
        obj = resolve(app, path)
        if obj is None:
            lines.append(f"path {path} -")
        else:
            data, _ = db.storage.load(obj._p_oid)
            module, name = get_pickle_metadata(data)
            lines.append(f"path {path} {u64(obj._p_oid):016x} {module}.{name}")
    paths[db.lastTransaction()] = lines


def build(db):
    """Commit the history of the site. Return the path lines by tid."""
    # ZODB.DB commits the empty root mapping first. No path leads anywhere.
    paths = {db.lastTransaction(): [f"path {path} -" for path in PATHS]}
    connection = db.open()
    root = connection.root()

    app = root["Application"] = Application()
    site = Site("plone", "Site")
    app.add(site)
    folder = Folder("folder", "Folder")
    site.add(folder)
    doc = Document("doc", "Doc, first", text="first text", weight=Decimal("1.5"))
    folder.add(doc)
    commit(db, app, paths, "someone", "create the site")

    doc.title = "Doc, second"
    commit(db, app, paths, "editor", "edit the document")

    big = Folder("big", "Big folder")
    site.add(big)
    for i in range(100):
        big.add(Document(f"item-{i:03}", f"Item {i}"))
    commit(db, app, paths, "someone", "add many items")

    doc.__annotations__ = OOBTree()
    doc.__annotations__["app.settings"] = PersistentMapping(flag=True)
    commit(db, app, paths, "editor", "annotate the document")

    folder.title = "Folder, renamed"
    folder.add(Document("doc-2", "Second doc"))
    commit(db, app, paths, "someone", "rename the folder and add a document")

    folder.remove("doc-2")
    commit(db, app, paths, "editor", "remove a document")

    connection.close()
    return paths


def describe(data_fs, paths):
    """Return the lines of `records.txt` for `data_fs`."""
    storage = FileStorage(str(data_fs), read_only=True)
    lines = []
    try:
        for txn in storage.iterator():
            fields = [txn.user, txn.description, txn.extension_bytes]
            fields = " ".join("x" + bytes(field).hex() for field in fields)
            lines.append(f"txn {u64(txn.tid):016x} {fields}")
            for record in txn:
                assert record.data is not None
                lines.append(f"rec {u64(record.oid):016x} x{record.data.hex()}")
            lines.extend(paths[txn.tid])
    finally:
        storage.close()
    return lines


def main():
    register(Application, Item, Document, Folder, Site)
    time.time = FixedClock()
    with tempfile.TemporaryDirectory() as tmp:
        data_fs = Path(tmp) / "Data.fs"
        db = ZODB.DB(FileStorage(str(data_fs)))
        try:
            paths = build(db)
        finally:
            db.close()
        lines = describe(data_fs, paths)
    (HERE / "records.txt").write_text("\n".join(lines) + "\n")
    print(f"{len(lines)} lines in records.txt")


if __name__ == "__main__":
    main()
