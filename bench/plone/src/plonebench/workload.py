"""The scenarios: which roles the simulated users take, and their targets.

Pure functions of the manifest and the user index, so the tests can check
them without Locust, and the report can name the mix of each run.

`write_targets(FIXTURE_DIR)` writes the targets file of a fixture,
`targets.sqlite`, beside its `manifest.json`, from the manifest and the
spec in it. Locust and the verification read it (`TargetsFile`): a pick
or a lookup reads a few rows, so their cost and their memory do not grow
with the site. `Targets` gives the same answers from a manifest in
memory.
"""

from __future__ import annotations

import json
import os
import sqlite3
from collections import Counter
from dataclasses import dataclass
from functools import cached_property
from pathlib import Path

from plonebench import corpus

# A role: the account it logs in with, and whether it goes through the
# rendered pages of Plone (`ui`) or through plone.restapi (`rest`).
ROLES = {
    "browse_anon": ("anonymous", "ui"),
    "browse_member": (corpus.MEMBER, "ui"),
    "search": (corpus.MEMBER, "ui"),
    "edit": (corpus.EDITOR, "ui"),
    "create_spread": (corpus.EDITOR, "ui"),
    "create_shared": (corpus.EDITOR, "ui"),
    "workflow": (corpus.EDITOR, "ui"),
    "edit_shared": (corpus.EDITOR, "ui"),
    "patch_fields": (corpus.EDITOR, "rest"),
    "patch_same": (corpus.EDITOR, "rest"),
    "warm": (corpus.EDITOR, "ui"),
    # The background roles: one user each, beside the mix.
    "reindex": (corpus.MAINTAINER, "ui"),
    "import": (corpus.IMPORTER, "rest"),
}

BACKGROUND = ("reindex", "import")


@dataclass(frozen=True)
class Scenario:
    name: str
    mix: dict[str, int]
    background: tuple[str, ...] = ()
    description: str = ""
    # True when the scenario writes. A write scenario starts from a fresh
    # copy of the fixture.
    writes: bool = True


SCENARIOS = {
    "browse": Scenario(
        "browse",
        {"browse_anon": 1, "browse_member": 1},
        description="rendered pages: folders, documents, news, files, breadcrumbs; "
        "anonymous and logged-in members",
        writes=False,
    ),
    "catalog": Scenario(
        "catalog",
        {"search": 1},
        description="@@search with text, filters, sorting and pages; collections; "
        "REST @search with metadata",
        writes=False,
    ),
    "edit": Scenario(
        "edit",
        {"edit": 1},
        description="the edit form of Plone on Documents, each user its own Documents",
    ),
    "create-spread": Scenario(
        "create-spread",
        {"create_spread": 1},
        description="the add form of Plone, each user in folders of its own section",
    ),
    "create-shared": Scenario(
        "create-shared",
        {"create_shared": 1},
        description="the add form of Plone, every user in the one folder bench/inbox",
    ),
    "workflow": Scenario(
        "workflow",
        {"workflow": 1},
        description="publish and retract through the workflow menu, with an "
        "anonymous visibility check",
    ),
    "mixed": Scenario(
        "mixed",
        {
            "browse_anon": 8,
            "browse_member": 3,
            "search": 4,
            "edit": 2,
            "create_spread": 1,
            "workflow": 1,
            "create_shared": 1,
        },
        description="a mix of interactive traffic, weights in the mix",
    ),
    "mixed-background": Scenario(
        "mixed-background",
        {
            "browse_anon": 8,
            "browse_member": 3,
            "search": 4,
            "edit": 2,
            "create_spread": 1,
            "workflow": 1,
            "create_shared": 1,
        },
        background=BACKGROUND,
        description="the mixed scenario, with one reindex of SearchableText in a loop "
        "and one REST import in a loop beside it",
    ),
    "contention-folder": Scenario(
        "contention-folder",
        {"edit_shared": 1},
        description="the edit form, each user its own Document, all Documents in one "
        "folder (shared folder and catalog structures)",
    ),
    "contention-fields": Scenario(
        "contention-fields",
        {"patch_fields": 1},
        description="REST PATCH: two users on each Document, one writes the title, "
        "the other the description",
    ),
    "contention-same": Scenario(
        "contention-same",
        {"patch_same": 1},
        description="REST PATCH: every user writes the title of one of the hot "
        "Documents (incompatible edits of one field)",
    ),
    "warmup": Scenario(
        "warmup",
        {"browse_anon": 2, "browse_member": 1, "search": 1, "warm": 1},
        description="the warmup: pages, searches, the edit form and its cancel, "
        "and the add form, without a save",
        writes=False,
    ),
}


def assign_roles(mix: dict[str, int], count: int) -> list[str]:
    """Return the role of each of `count` users: a smooth weighted round
    robin, so any prefix of the list follows the weights as closely as a
    whole number of users can."""
    current = {role: 0 for role in mix}
    total = sum(mix.values())
    roles = []
    for _ in range(count):
        for role, weight in mix.items():
            current[role] += weight
        chosen = max(current, key=lambda role: (current[role], -list(mix).index(role)))
        current[chosen] -= total
        roles.append(chosen)
    return roles


def ordinal(roles: list[str], index: int) -> int:
    """Return the rank of user `index` among the users of its role."""
    role = roles[index]
    return sum(1 for r in roles[:index] if r == role)


class _Roles:
    """The targets of the roles that follow from the spec, the accounts
    and the Documents of each section. A subclass gives `spec`, `users`
    and `section_docs`."""

    spec: corpus.SiteSpec
    users: dict[str, list[str]]

    def section_docs(self, section: int, pool: str) -> list[dict]:
        raise NotImplementedError

    def editor(self, rank: int) -> tuple[str, int]:
        """Return the account and the section of the editor of `rank`."""
        editors = self.users[corpus.EDITOR]
        account = editors[rank % len(editors)]
        return account, corpus.editor_section(self.spec, editors.index(account))

    def own_docs(self, rank: int, pool: str, users: int) -> list[dict]:
        """The Documents of `pool` that the editor of `rank` owns: those of
        its section, split among the users of the role in that section.
        Two ranks never share a Document while `users` is at least the
        number of users of the role."""
        _, section = self.editor(rank)
        sections = self.spec.sections
        per_section = max(1, -(-users // sections))
        docs = self.section_docs(section, pool)
        return docs[(rank // sections) % per_section :: per_section]

    def shared_doc(self, rank: int) -> str:
        folder = corpus.shared_folder(self.spec)
        return f"{folder}/doc-{rank % self.spec.shared:03d}"

    def pair_doc(self, rank: int) -> tuple[str, str]:
        """The Document and the field of a user of contention-fields: users
        2k and 2k+1 share Document k; the first writes the title, the
        second the description."""
        folder = corpus.shared_folder(self.spec)
        doc = f"{folder}/doc-{(rank // 2) % self.spec.shared:03d}"
        return doc, ("title" if rank % 2 == 0 else "description")

    def hot_doc(self, rank: int) -> str:
        return f"{corpus.BENCH}/{corpus.HOT}/doc-{rank % self.spec.hot:03d}"


# The fields of an item of the manifest that the targets file keeps.
ITEM_FIELDS = (
    "path",
    "portal_type",
    "title",
    "state",
    "pool",
    "section",
    "size",
    "uid",
    "versions",
    "workflow_history",
)


@dataclass
class Targets(_Roles):
    """The targets of the roles and the items of a manifest in memory."""

    manifest: dict

    @classmethod
    def load(cls, path: str) -> Targets:
        with open(path) as file:
            return cls(json.load(file))

    @cached_property
    def spec(self) -> corpus.SiteSpec:
        return corpus.SiteSpec.from_json(self.manifest.get("spec", {}))

    @property
    def users(self) -> dict[str, list[str]]:
        return self.manifest["users"]

    def items(self, **where) -> list[dict]:
        return [
            item
            for item in self.manifest["items"]
            if all(item.get(key) == value for key, value in where.items())
        ]

    def browsable(self) -> list[dict]:
        """The items an anonymous visitor can read and that no scenario
        changes the state of."""
        return [
            item
            for item in self.manifest["items"]
            if item["pool"] != corpus.WORKFLOW
            and item["portal_type"] != "Collection"
            and (item["state"] in (corpus.PUBLISHED, None))
            and not item["path"].startswith(corpus.BENCH)
        ]

    def section_docs(self, section: int, pool: str) -> list[dict]:
        return [
            item
            for item in self.manifest["items"]
            if item["portal_type"] == "Document"
            and item["section"] == section
            and item["pool"] == pool
        ]

    def section_folders(self, section: int) -> list[str]:
        return [
            item["path"]
            for item in self.manifest["items"]
            if item["portal_type"] == "Folder" and item["section"] == section
        ]

    # The items of the manifest, for the verification. `TargetsFile`
    # gives the same answers from the targets file.

    @cached_property
    def _indexes(self) -> dict[str, int]:
        return {item["path"]: n for n, item in enumerate(self.manifest["items"])}

    @cached_property
    def _children(self) -> dict[str, list[str]]:
        children: dict[str, list[str]] = {}
        for item in self.manifest["items"]:
            children.setdefault(_parent(item["path"]), []).append(item["path"])
        return children

    def __len__(self) -> int:
        return len(self.manifest["items"])

    @property
    def seed(self) -> int:
        return self.spec.seed

    @property
    def catalog_size(self) -> int | None:
        return self.manifest.get("catalog_size")

    @cached_property
    def duplicate_uids(self) -> list[str]:
        return _duplicates(item.get("uid") for item in self.manifest["items"])

    def index(self, path: str) -> int | None:
        """Return the index of `path` in the manifest, or None."""
        return self._indexes.get(path)

    def paths(self, start: int, stop: int) -> list[str]:
        """Return the paths of the items from `start` to `stop`, excluded."""
        return [item["path"] for item in self.manifest["items"][start:stop]]

    def item(self, path: str) -> dict | None:
        index = self._indexes.get(path)
        return None if index is None else self.manifest["items"][index]

    def items_by_path(self, paths) -> dict[str, dict]:
        """Return the item of each path of `paths` that the manifest holds."""
        found = (self.item(path) for path in paths)
        return {item["path"]: item for item in found if item is not None}

    def children(self, path: str) -> list[str]:
        """Return the paths of the children of `path` in the manifest, in
        its order. The empty path is the site."""
        return list(self._children.get(path, ()))


def view(manifest) -> Targets | TargetsFile:
    """Return the items of `manifest`: a `TargetsFile` or a `Targets` as
    it is, a manifest in memory as a `Targets`."""
    if isinstance(manifest, Targets | TargetsFile):
        return manifest
    return Targets(manifest)


def _parent(path: str) -> str:
    return path.rpartition("/")[0]


def _duplicates(values) -> list[str]:
    counts = Counter(value for value in values if value)
    return sorted(value for value, count in counts.items() if count > 1)


# The targets file: its name beside `manifest.json`, and the version of
# its layout. A file of another version, or of another manifest, is
# written again.
TARGETS_FILE = "targets.sqlite"
TARGETS_VERSION = 1

_TABLES = """
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE items (
    idx INTEGER PRIMARY KEY,
    parent TEXT NOT NULL,
    path TEXT NOT NULL,
    portal_type TEXT NOT NULL,
    title TEXT,
    state TEXT,
    pool TEXT,
    section INTEGER,
    size INTEGER,
    uid TEXT,
    versions INTEGER,
    workflow_history INTEGER
);
CREATE TABLE folders (
    n INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    start INTEGER NOT NULL,
    stop INTEGER NOT NULL
);
CREATE TABLE docs (
    n INTEGER PRIMARY KEY,
    path TEXT NOT NULL,
    portal_type TEXT NOT NULL,
    parent INTEGER
);
CREATE TABLE files (n INTEGER PRIMARY KEY, path TEXT NOT NULL, size INTEGER NOT NULL);
"""
_INDEXES = """
CREATE UNIQUE INDEX items_path ON items (path);
CREATE INDEX items_parent ON items (parent, idx);
CREATE INDEX items_section ON items (section, portal_type, pool, idx);
"""
_ITEM_COLUMNS = ", ".join(ITEM_FIELDS)
# The most values in one IN of a query.
_CHUNK = 500


def targets_path(manifest: str | os.PathLike) -> Path:
    """Return the path of the targets file beside the manifest `manifest`."""
    return Path(manifest).with_name(TARGETS_FILE)


def _stamp(manifest: Path) -> list[int]:
    stat = manifest.stat()
    return [stat.st_size, stat.st_mtime_ns]


def write_targets(fixture_dir: str | os.PathLike) -> Path:
    """Write the targets file of the fixture in `fixture_dir` from its
    `manifest.json` and the spec in it, and return its path.

    It plans no site and changes no file of the fixture: so a fixture of
    any build gets it. The file holds the accounts, the search terms, the
    subjects and the code words of the spec; the browsable Folders,
    Documents and News Items, and Files, in the order of the manifest,
    with the slice of the Documents and News Items below each Folder and
    the Folder of each one; the Collections; and each item with its
    index in the manifest and its parent. It replaces the file of an
    earlier call in one rename.
    """
    directory = Path(fixture_dir)
    source = directory / "manifest.json"
    stamp = _stamp(source)
    with open(source) as file:
        manifest = json.load(file)
    target = directory / TARGETS_FILE
    partial = directory / f".{TARGETS_FILE}.{os.getpid()}"
    partial.unlink(missing_ok=True)
    db = sqlite3.connect(partial)
    try:
        _fill(db, manifest, stamp)
        db.commit()
    except BaseException:
        db.close()
        partial.unlink(missing_ok=True)
        raise
    db.close()
    os.replace(partial, target)
    return target


def ensure_targets(fixture_dir: str | os.PathLike) -> Path:
    """Return the targets file of the fixture in `fixture_dir`. Write it
    first when it is missing, of another version, or of another
    manifest."""
    directory = Path(fixture_dir)
    path = directory / TARGETS_FILE
    if path.exists():
        try:
            stamp = _stamp(directory / "manifest.json")
            with TargetsFile.open(path) as targets:
                fresh = targets.meta.get("manifest") == stamp
        except (sqlite3.Error, KeyError, ValueError):
            fresh = False
        if fresh:
            return path
    return write_targets(directory)


def _fill(db: sqlite3.Connection, manifest: dict, stamp: list[int]) -> None:
    spec = corpus.SiteSpec.from_json(manifest["spec"])
    items = manifest["items"]
    db.executescript(_TABLES)
    db.executemany(
        "INSERT INTO items VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
        (
            (
                n,
                _parent(item["path"]),
                item["path"],
                item["portal_type"],
                item.get("title"),
                item.get("state"),
                item.get("pool"),
                item.get("section"),
                item.get("size", 0),
                item.get("uid"),
                item.get("versions", 0),
                item.get("workflow_history", 0),
            )
            for n, item in enumerate(items)
        ),
    )
    browsable = Targets(manifest).browsable()
    folders = [item for item in browsable if item["portal_type"] == "Folder"]
    docs = [i for i in browsable if i["portal_type"] in ("Document", "News Item")]
    files = [item for item in browsable if item["portal_type"] == "File"]
    folder_of = {}
    for n, folder in enumerate(folders):
        folder_of.setdefault(folder["path"], n)
    # The Documents and News Items below a Folder, at any depth. The
    # manifest lists each container before what it holds, so they are one
    # slice of `docs`.
    spans: dict[int, list[int]] = {}
    for d, doc in enumerate(docs):
        parts = doc["path"].split("/")
        for depth in range(1, len(parts)):
            n = folder_of.get("/".join(parts[:depth]))
            if n is not None:
                span = spans.setdefault(n, [d, d, 0])
                span[1] = d
                span[2] += 1
    rows = []
    for n, folder in enumerate(folders):
        first, last, count = spans.get(n, (0, -1, 0))
        if count != last - first + 1:
            raise ValueError(
                f"the Documents below {folder['path']} are not one slice of the "
                "browsable Documents"
            )
        rows.append((n, folder["path"], first, last + 1))
    db.executemany("INSERT INTO folders VALUES (?, ?, ?, ?)", rows)
    db.executemany(
        "INSERT INTO docs VALUES (?, ?, ?, ?)",
        (
            (d, doc["path"], doc["portal_type"], folder_of.get(_parent(doc["path"])))
            for d, doc in enumerate(docs)
        ),
    )
    db.executemany(
        "INSERT INTO files VALUES (?, ?, ?)",
        ((n, item["path"], item["size"]) for n, item in enumerate(files)),
    )
    db.executescript(_INDEXES)
    meta = {
        "version": TARGETS_VERSION,
        "manifest": stamp,
        "site": manifest["site"],
        "spec": manifest["spec"],
        "users": manifest["users"],
        "search_terms": manifest["search_terms"],
        "subjects": manifest["subjects"],
        "catalog_size": manifest.get("catalog_size"),
        "code_words": corpus.code_words(corpus.site_words(spec)),
        "collections": [i["path"] for i in items if i["portal_type"] == "Collection"],
        "duplicate_uids": _duplicates(item.get("uid") for item in items),
        "counts": {
            "items": len(items),
            "folders": len(folders),
            "docs": len(docs),
            "files": len(files),
        },
    }
    db.executemany(
        "INSERT INTO meta VALUES (?, ?)",
        ((key, json.dumps(value)) for key, value in meta.items()),
    )


class TargetsFile(_Roles):
    """The targets file of a fixture, read-only. Each pick and each lookup
    reads a few rows. One instance serves one thread: the greenlets of
    Locust share the thread of their process."""

    def __init__(self, db: sqlite3.Connection):
        self.db = db
        self.meta = {
            key: json.loads(value)
            for key, value in db.execute("SELECT key, value FROM meta")
        }
        if self.meta.get("version") != TARGETS_VERSION:
            raise ValueError(f"the targets file has version {self.meta.get('version')}")
        self.spec = corpus.SiteSpec.from_json(self.meta["spec"])
        self.users = self.meta["users"]
        self.site = self.meta["site"]
        self.search_terms = self.meta["search_terms"]
        self.subjects = self.meta["subjects"]
        self.code_words = self.meta["code_words"]
        self.collections = [{"path": path} for path in self.meta["collections"]]
        self.catalog_size = self.meta["catalog_size"]
        self.duplicate_uids = self.meta["duplicate_uids"]
        counts = self.meta["counts"]
        self.count = counts["items"]
        self.folders = counts["folders"]
        self.docs = counts["docs"]
        self.files = counts["files"]

    @classmethod
    def open(cls, path: str | os.PathLike) -> TargetsFile:
        uri = Path(path).absolute().as_uri() + "?mode=ro"
        db = sqlite3.connect(uri, uri=True)
        try:
            return cls(db)
        except BaseException:
            db.close()
            raise

    def close(self) -> None:
        self.db.close()

    def __enter__(self) -> TargetsFile:
        return self

    def __exit__(self, *exc) -> None:
        self.close()

    # The picks of Locust.

    def folder(self, n: int) -> dict:
        """The browsable Folder `n`, with the slice of the Documents and
        News Items below it (`start`, `stop`)."""
        path, start, stop = self.db.execute(
            "SELECT path, start, stop FROM folders WHERE n = ?", (n,)
        ).fetchone()
        return {"path": path, "start": start, "stop": stop}

    def doc(self, n: int) -> dict:
        """The browsable Document or News Item `n`, with the number of its
        Folder (`parent`), or None when its Folder is not browsable."""
        path, portal_type, parent = self.db.execute(
            "SELECT path, portal_type, parent FROM docs WHERE n = ?", (n,)
        ).fetchone()
        return {"path": path, "portal_type": portal_type, "parent": parent}

    def file(self, n: int) -> dict:
        path, size = self.db.execute(
            "SELECT path, size FROM files WHERE n = ?", (n,)
        ).fetchone()
        return {"path": path, "size": size}

    def section_docs(self, section: int, pool: str) -> list[dict]:
        return self._items(
            "section = ? AND portal_type = 'Document' AND pool = ?", (section, pool)
        )

    def section_folders(self, section: int) -> list[str]:
        return [
            path
            for (path,) in self.db.execute(
                "SELECT path FROM items WHERE section = ? AND portal_type = 'Folder' "
                "ORDER BY idx",
                (section,),
            )
        ]

    # The items of the manifest, for the verification.

    def __len__(self) -> int:
        return self.count

    @property
    def seed(self) -> int:
        return self.spec.seed

    def index(self, path: str) -> int | None:
        row = self.db.execute(
            "SELECT idx FROM items WHERE path = ?", (path,)
        ).fetchone()
        return None if row is None else row[0]

    def paths(self, start: int, stop: int) -> list[str]:
        return [
            path
            for (path,) in self.db.execute(
                "SELECT path FROM items WHERE idx >= ? AND idx < ? ORDER BY idx",
                (start, stop),
            )
        ]

    def item(self, path: str) -> dict | None:
        found = self._items("path = ?", (path,))
        return found[0] if found else None

    def items_by_path(self, paths) -> dict[str, dict]:
        paths = list(dict.fromkeys(paths))
        found: dict[str, dict] = {}
        for start in range(0, len(paths), _CHUNK):
            chunk = paths[start : start + _CHUNK]
            marks = ", ".join("?" * len(chunk))
            for item in self._items(f"path IN ({marks})", chunk):
                found[item["path"]] = item
        return found

    def children(self, path: str) -> list[str]:
        return [
            child
            for (child,) in self.db.execute(
                "SELECT path FROM items WHERE parent = ? ORDER BY idx", (path,)
            )
        ]

    def _items(self, where: str, args) -> list[dict]:
        rows = self.db.execute(
            f"SELECT {_ITEM_COLUMNS} FROM items WHERE {where} ORDER BY idx", args
        )
        return [dict(zip(ITEM_FIELDS, row, strict=True)) for row in rows]


def steps_from_env(value: str | None) -> list[tuple[int, float]]:
    """Parse PLONEBENCH_STEPS: `users:seconds,users:seconds,...`."""
    if not value:
        return []
    steps = []
    for part in value.split(","):
        users, seconds = part.split(":")
        steps.append((int(users), float(seconds)))
    return steps


def env(name: str, default: str | None = None) -> str | None:
    return os.environ.get(f"PLONEBENCH_{name}", default)
