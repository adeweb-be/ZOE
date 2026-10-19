"""`just bench publish`: copy result documents into a checkout of the
results repository (ADR 0054).

`publish` checks everything before it writes: every document, the path
of each file that it writes in the checkout, and the new index. When one
check fails, it prints each reason, exits with 1 and writes nothing. Else
it copies the bytes of each document to its path in the layout, copies
the page of bench/dashboard/ when it exists, and writes index.json of
version 2 from the whole tree. It never runs git in the checkout, never
commits and never pushes.

A document is publishable when it is of a known family and valid against
the schema of its family, when it is not a quick run, when its commit is on
the branch REF of the ZOE repository, when its tree had no change that the
commit does not hold, when each run of a B5 summary passed its
verification, and when its path is free in the checkout. No file that
`publish` writes passes through a symbolic link, so it writes nothing
outside the checkout.
"""

import json
import os
import subprocess
import sys
from pathlib import Path

import jsonschema

from shootout import layout

BENCH = Path(__file__).resolve().parents[3]
SCHEMAS = BENCH / "schema"
DASHBOARD = BENCH / "dashboard"
# The ZOE repository of the harness. The commit of a document must be on
# one of its branches.
REPOSITORY = BENCH.parent
SCHEMA_OF = {
    "shootout": "results.schema.json",
    "native": "native.schema.json",
    "client": "client.schema.json",
    "plone": "plone.schema.json",
}
MAIN = "origin/main"


class UsageError(Exception):
    """A usage error: publish exits with 2 and changes nothing."""


def publish(sources: list[str], checkout: str, main: str = MAIN) -> int:
    """Publish the documents of sources into checkout. Return the exit
    code."""
    try:
        root = Path(checkout)
        if not root.is_dir():
            raise UsageError(f"{checkout} is not a directory")
        documents = _documents(sources)
        if _git("rev-parse", "--verify", "--quiet", f"{main}^{{commit}}") is None:
            raise UsageError(f"{main} is not a commit of the ZOE repository")
    except UsageError as error:
        print(f"publish: {error}", file=sys.stderr)
        return 2

    schemas = {
        kind: json.loads((SCHEMAS / name).read_text())
        for kind, name in SCHEMA_OF.items()
    }
    on_main: dict[str, bool] = {}
    targets: dict[str, bytes] = {}
    published: dict[str, dict] = {}
    copies = []
    refused = 0
    for shown, path in documents:
        data = path.read_bytes()
        target, reasons, document = _check(data, main, schemas, on_main)
        if target is not None:
            if targets.get(target, data) != data:
                reasons.append(f"another document goes to {target}")
            elif problem := _path_problem(root, target, checkout):
                reasons.append(problem)
            elif (root / target).is_file() and (root / target).read_bytes() != data:
                reasons.append(f"{target} exists in {checkout} with other content")
            targets.setdefault(target, data)
        for reason in reasons:
            print(f"publish: {shown}: {reason}", file=sys.stderr)
        if reasons:
            refused += 1
        else:
            copies.append((target, data))
            published[target] = document

    pages, problems = _dashboard(root, checkout)
    if problem := _path_problem(root, "index.json", checkout):
        problems.append((checkout, problem))
    try:
        entries = layout.index_entries(root, published)
    except ValueError as error:
        problems.append((checkout, str(error)))
    for shown, problem in problems:
        print(f"publish: {shown}: {problem}", file=sys.stderr)
    if refused or problems:
        if refused:
            print(
                f"publish: refused {refused} of {len(documents)} documents; "
                f"nothing changed in {checkout}",
                file=sys.stderr,
            )
        else:
            print(
                f"publish: {checkout} cannot take the documents; nothing changed "
                f"in {checkout}",
                file=sys.stderr,
            )
        return 1

    for target, data in copies:
        path = root / target
        if not (path.is_file() and path.read_bytes() == data):
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_bytes(data)
    for relative, data in pages:
        path = root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data)
    index = layout.write_entries(root, entries)
    for target in dict.fromkeys(target for target, _ in copies):
        print(root / target)
    print(index)
    return 0


def _path_problem(root: Path, relative: str, checkout: str) -> str | None:
    """Return why publish cannot write the file relative under root, or
    None. A part of the path must not be a symbolic link, and the file
    must be a regular file when it exists."""
    parts = relative.split("/")
    path = root
    for number, part in enumerate(parts):
        path = path / part
        if path.is_symlink():
            return f"{relative} passes through a symbolic link in {checkout}"
        if not path.exists():
            break
        if number < len(parts) - 1 and not path.is_dir():
            return f"{relative} passes through a file in {checkout}"
        if number == len(parts) - 1 and not path.is_file():
            return f"{relative} exists in {checkout} and is not a file"
    if not (root / relative).resolve().is_relative_to(root.resolve()):
        return f"{relative} is outside {checkout}"
    return None


def _dashboard(root: Path, checkout: str):
    """Return the files of the page of bench/dashboard/ that publish
    copies, as (path in the checkout, bytes), and the reasons why a file
    cannot go to the checkout, as (file, reason). The files are the files
    that git tracks there, without dotfiles, __pycache__ and symbolic
    links."""
    if not DASHBOARD.is_dir():
        return [], []
    # bench/dashboard of the repository of the harness.
    repository = DASHBOARD.parents[1]
    base = DASHBOARD.relative_to(repository).as_posix()
    listed = _git("ls-files", "-z", "--", base, cwd=repository)
    if listed is None:
        return [], [(base, "git cannot list the files of the page")]
    pages, problems = [], []
    for name in sorted(filter(None, listed.split("\0"))):
        source = repository / name
        relative = Path(name).relative_to(base).as_posix()
        parts = relative.split("/")
        if any(part.startswith(".") or part == "__pycache__" for part in parts):
            continue
        if source.is_symlink() or not source.is_file():
            continue
        if not source.resolve().is_relative_to(DASHBOARD.resolve()):
            continue
        if parts[0] == "results" or relative == "index.json":
            problems.append((name, f"goes to {relative}, which publish writes"))
        elif problem := _path_problem(root, relative, checkout):
            problems.append((name, problem))
        else:
            pages.append((relative, source.read_bytes()))
    return pages, problems


def _documents(sources: list[str]) -> list[tuple[str, Path]]:
    """Return each document of sources: a file, or the *.json files
    directly in a directory. Each comes with its path as the command line
    gives it."""
    documents = []
    for source in sources:
        path = Path(source)
        if not path.exists():
            raise UsageError(f"{source} does not exist")
        if path.is_dir():
            found = sorted(p for p in path.glob("*.json") if p.is_file())
            if not found:
                raise UsageError(f"{source} holds no document")
            documents += [(os.path.join(source, p.name), p) for p in found]
        else:
            documents.append((source, path))
    return documents


def _check(
    data: bytes, main: str, schemas: dict, on_main: dict[str, bool]
) -> tuple[str | None, list[str], dict | None]:
    """Return the path of a document in the layout, or None, the reasons
    why it is not publishable, and the document."""
    try:
        document = json.loads(data)
    except ValueError:
        return None, ["is not a JSON document"], None
    kind = layout.family(document) if isinstance(document, dict) else None
    if kind is None:
        return None, ["not a document of a known family"], None
    try:
        jsonschema.validate(document, schemas[kind])
    except jsonschema.ValidationError as error:
        reason = f"not valid against {SCHEMA_OF[kind]}: {error.message}"
        return None, [reason], None
    reasons = []
    run = document["run"]
    if kind == "shootout" and (run["quick"] or run.get("set") == "quick"):
        reasons.append("a quick run")
    commit = run["commit"]
    if commit is None:
        reasons.append("no commit")
    else:
        if commit not in on_main:
            on_main[commit] = (
                _git("merge-base", "--is-ancestor", commit, main) is not None
            )
        if not on_main[commit]:
            reasons.append(f"commit {commit[:12]} is not on {main}")
    dirty = run.get("dirty")
    if dirty is True:
        reasons.append("a run of a dirty tree")
    elif dirty is None:
        reasons.append("no record of a clean tree")
    if kind == "plone":
        for result in document["runs"]:
            if result["verification"] != "pass":
                reasons.append(
                    f"the verification of run {result['id']} is "
                    f"{result['verification']}"
                )
    target = layout.published_path(document)
    if target is None:
        reasons.append("its path in the layout holds a part that is not a name")
    return target, reasons, document


def _git(*args: str, cwd: Path = REPOSITORY) -> str | None:
    """Run git in the ZOE repository. Return its output, or None when it
    fails."""
    try:
        done = subprocess.run(
            ["git", *args],
            cwd=cwd,
            capture_output=True,
            text=True,
            check=False,
        )
    except OSError:
        return None
    return done.stdout if done.returncode == 0 else None
