"""The layouts of the results.

The harness of zodbshootout writes its own tree, which `just bench gate`
reads: a full run goes to `results/<machine>/<python>/<storage>/<run>.json`,
and a quick run to the same path under `quick/`.

A checkout of the results repository holds every family (ADR 0054):

    index.json
    results/shootout/<machine>/<python>/<storage>/<started>-<commit>.json
    results/native/<machine>/<python>/<started>-<commit>.json
    results/client/<machine>/<python>/<started>-<commit>.json
    results/plone/<machine>/<python>/<fixture>/<started>-<commit>.json

A file under another directory of `results/` is a result of zodbshootout
in the layout of version 1. `index.json` of version 2 lists every document
of the tree, oldest first.
"""

import json
import re
from pathlib import Path

from shootout.environment import python_tag

INDEX_VERSION = 2

# The family of a document, by the one key at its top level that names it.
FAMILY_KEYS = {
    "storage": "shootout",
    "cells": "native",
    "rows": "client",
    "fixture": "plone",
}
FAMILIES = tuple(FAMILY_KEYS.values())

# A part of a path in the layout: no separator, and no leading dot.
SEGMENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]*$")


def run_set(run: dict) -> str:
    """Return the set of a run: quick or full. A result without the
    key `set` is quick or full."""
    return run.get("set") or ("quick" if run["quick"] else "full")


def _stem(run: dict) -> str:
    """Return the name of a document without its suffix:
    <started>-<commit>, or <started> without a commit."""
    stamp = run["started"].replace("-", "").replace(":", "")
    return f"{stamp}-{run['commit'][:12]}" if run["commit"] else stamp


def result_path(root: Path, result: dict) -> Path:
    """Return the path of a result of zodbshootout in the tree of the
    harness."""
    run = result["run"]
    return (
        root
        / ("quick" if run_set(run) == "quick" else "results")
        / result["machine"]["name"]
        / python_tag(result["python"])
        / result["storage"]
        / f"{_stem(run)}.json"
    )


def family(document: dict) -> str | None:
    """Return the family of a document, or None when no key, or more than
    one, names it."""
    found = [name for key, name in FAMILY_KEYS.items() if key in document]
    return found[0] if len(found) == 1 else None


def published_path(document: dict) -> str | None:
    """Return the path of a document in a checkout of the results
    repository, with `/`. Return None when a part of it cannot be a part
    of a path."""
    kind = family(document)
    parts = [document["machine"]["name"], python_tag(document["python"])]
    if kind == "shootout":
        parts.append(document["storage"])
    elif kind == "plone":
        parts.append(document["fixture"]["name"])
    parts.append(f"{_stem(document['run'])}.json")
    if not all(SEGMENT.fullmatch(part) for part in parts):
        return None
    return "/".join(["results", kind, *parts])


def _once(names) -> list[str]:
    """Return each name once, in the order of its first occurrence."""
    return list(dict.fromkeys(names))


def entry(path: str, document: dict) -> dict:
    """Return the entry of a document in index.json of version 2. The
    first directory under results/ names its family. Another directory is
    the layout of version 1, of zodbshootout."""
    parts = path.split("/")
    kind = parts[1] if len(parts) > 2 and parts[1] in FAMILIES else "shootout"
    if family(document) != kind:
        raise ValueError(f"{path} is not a document of the family {kind}")
    fixture = None
    if kind == "shootout":
        stacks = [document["storage"]]
        scenarios = _once(s["name"] for s in document["scenarios"])
    elif kind == "native":
        cells = document["cells"]
        stacks = _once(name for cell in cells for name in cell["workers"])
        scenarios = _once(s["name"] for cell in cells for s in cell["scenarios"])
    elif kind == "client":
        stacks = _once(row["client"] for row in document["rows"])
        scenarios = _once(row["workload"] for row in document["rows"])
    else:
        stacks = _once(stack["name"] for stack in document["stacks"])
        scenarios = _once(s["name"] for s in document["scenarios"])
        fixture = document["fixture"]["name"]
    return {
        "family": kind,
        "path": path,
        "machine": document["machine"]["name"],
        "python": python_tag(document["python"]),
        "stacks": stacks,
        "fixture": fixture,
        "scenarios": scenarios,
        "started": document["run"]["started"],
        "commit": document["run"]["commit"],
    }


def index_entries(root: Path, extra: dict[str, dict] | None = None) -> list[dict]:
    """Return the entries of index.json of version 2: every document under
    results/ of root, and the documents of extra by their path, oldest
    first. A document of extra takes the place of the file at its path. It
    does not read the old index, so it reads a tree of either version.

    Raise ValueError when a file under results/ is not a document that
    the index can read, or when a path there is a symbolic link."""
    extra = extra or {}
    documents = dict(extra)
    for path in sorted((root / "results").rglob("*")):
        relative = path.relative_to(root).as_posix()
        if path.is_symlink():
            raise ValueError(f"{relative} is a symbolic link")
        if relative in extra or path.suffix != ".json" or not path.is_file():
            continue
        try:
            documents[relative] = json.loads(path.read_text())
        except (ValueError, UnicodeDecodeError):
            raise ValueError(f"{relative} is not a JSON document") from None
    entries = []
    for relative, document in documents.items():
        try:
            found = entry(relative, document)
            if not isinstance(found["started"], str):
                raise TypeError("started")
        except (KeyError, TypeError, AttributeError, IndexError):
            raise ValueError(
                f"{relative} is not a document that the index can read"
            ) from None
        entries.append(found)
    entries.sort(key=lambda e: (e["started"], e["path"]))
    return entries


def write_entries(root: Path, entries: list[dict]) -> Path:
    """Write index.json of version 2 with entries."""
    index = root / "index.json"
    index.write_text(
        json.dumps({"index_version": INDEX_VERSION, "results": entries}, indent=2)
        + "\n"
    )
    return index


def write_index(root: Path) -> Path:
    """Write index.json of version 2 from every document under results/."""
    return write_entries(root, index_entries(root))
