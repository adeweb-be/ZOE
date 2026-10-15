"""The verification of a run: the ledger against the state of the site.

`request(records, manifest)` names what `inzope/observe.py` must read.
`judge(records, manifest, observed)` checks it and returns one entry per
check, with its counts and up to EXAMPLES violations. Both are pure, so
the tests check the rules without Zope.

A timed-out or cut write is `ambiguous`: it may have committed. The rules
accept both of its results, and count them apart.
"""

from __future__ import annotations

import re
from collections import defaultdict

from plonebench import corpus
from plonebench.ledger import AMBIGUOUS, FAILED, OK, REJECTED, Record

EXAMPLES = 10
CREATE_KINDS = ("create", "import")
EDIT_KINDS = ("edit", "patch-title", "patch-description")
WORKFLOW_KINDS = ("workflow",)
# The paths whose version titles the observer reads: the contention
# targets, which few Documents hold.
TITLE_PATHS = (f"{corpus.BENCH}/{corpus.HOT}/", f"{corpus.BENCH}/{corpus.SHARED}/")
MAX_TITLE_PATHS = 64
MAX_TOKENS = 4000
_TOKEN = re.compile(r"^(?:Edit|New|Patch|Import) ([a-z]+ [a-z]+ [a-z]+ [a-z]+)(?: |$)")


def token_of(title: str | None) -> str | None:
    """Return the operation code that a title of the benchmark holds."""
    if not title:
        return None
    match = _TOKEN.match(title)
    return match.group(1) if match else None


def fields_of(record: Record) -> dict[str, str]:
    """Return the fields that an edit record wrote."""
    return {k: v for k, v in record.values.items() if k in ("title", "description")}


def _writes(records: list[Record]) -> list[Record]:
    return [r for r in records if r.kind in EDIT_KINDS + WORKFLOW_KINDS + CREATE_KINDS]


def request(records: list[Record], manifest: dict) -> dict:
    paths: set[str] = set()
    tokens: list[str] = []
    for record in _writes(records):
        if record.target and record.outcome in (OK, AMBIGUOUS, REJECTED):
            if record.kind in CREATE_KINDS and record.outcome != OK:
                continue
            paths.add(record.target)
    by_target: dict[str, list[Record]] = defaultdict(list)
    for record in records:
        if (
            record.kind in EDIT_KINDS + CREATE_KINDS
            and record.outcome == OK
            and record.target
        ):
            by_target[record.target].append(record)
    for ops in by_target.values():
        ops.sort(key=lambda r: r.end)
        for record in ops[-2:]:
            token = token_of(record.values.get("title"))
            if token:
                tokens.append(token)
    titles = sorted(p for p in paths if p.startswith(TITLE_PATHS))[:MAX_TITLE_PATHS]
    return {
        "paths": sorted(paths),
        "tokens": tokens[:MAX_TOKENS],
        "version_titles": titles,
        "manifest_paths": [item["path"] for item in manifest["items"]],
    }


class Check:
    def __init__(self, name: str, description: str):
        self.name = name
        self.description = description
        self.checked = 0
        self.violations = 0
        self.notes: dict[str, int] = defaultdict(int)
        self.examples: list[str] = []

    def ok(self, count: int = 1):
        self.checked += count

    def fail(self, example: str):
        self.checked += 1
        self.violations += 1
        if len(self.examples) < EXAMPLES:
            self.examples.append(example)

    def note(self, key: str, count: int = 1):
        self.notes[key] += count

    def to_json(self) -> dict:
        return {
            "description": self.description,
            "checked": self.checked,
            "violations": self.violations,
            "notes": dict(self.notes),
            "examples": self.examples,
        }


def _allowed_single(ops: list[Record], field: str) -> set[str] | None:
    """The values that a field written by one user in sequence may hold:
    the value of its last acknowledged write, and of each ambiguous write
    that started after it. None when no write was acknowledged."""
    ops = sorted(ops, key=lambda r: r.start)
    acknowledged = [r for r in ops if r.outcome == OK]
    if not acknowledged:
        return None
    last = acknowledged[-1]
    allowed = {fields_of(last)[field]}
    for r in ops:
        if r.outcome == AMBIGUOUS and r.start > last.start and field in fields_of(r):
            allowed.add(fields_of(r)[field])
    return allowed


def _allowed_multi(ops: list[Record], field: str) -> set[str] | None:
    """The values that a field written by several users may hold: the value
    of an acknowledged write that no acknowledged write started after the
    end of, and of any ambiguous write. A write acknowledged before another
    one started committed before it, so it cannot be the last."""
    acknowledged = [r for r in ops if r.outcome == OK]
    if not acknowledged:
        return None
    latest_start = max(r.start for r in acknowledged)
    allowed = {fields_of(r)[field] for r in acknowledged if r.end >= latest_start}
    allowed |= {
        fields_of(r)[field]
        for r in ops
        if r.outcome == AMBIGUOUS and field in fields_of(r)
    }
    return allowed


def judge(records: list[Record], manifest: dict, observed: dict) -> dict:
    items = {item["path"]: item for item in manifest["items"]}
    objects = observed["objects"]
    checks = {
        "created": Check("created", "each acknowledged creation exists with its title"),
        "failed_writes": Check(
            "failed_writes", "a write that the site refused did not commit"
        ),
        "phantoms": Check(
            "phantoms", "no new object without an operation of the ledger"
        ),
        "unique_ids": Check("unique_ids", "UIDs and history ids are unique"),
        "edits": Check(
            "edits", "each edited field holds the value of the last acknowledged write"
        ),
        "versions": Check(
            "versions",
            "each acknowledged edit added one version, and a refused one none",
        ),
        "version_titles": Check(
            "version_titles", "each acknowledged title is in the version history"
        ),
        "workflow": Check(
            "workflow",
            "the state is that of the last acknowledged transition, and each "
            "transition is in the workflow history",
        ),
        "permissions": Check(
            "permissions", "anonymous may View an object exactly when it is published"
        ),
        "catalog": Check(
            "catalog",
            "each object has one catalog entry with its title, state, date "
            "and security",
        ),
        "search": Check(
            "search",
            "the token of the current title finds the object, a superseded "
            "token does not",
        ),
    }

    writes = _writes(records)
    for r in writes:
        checks_outcome = checks["failed_writes"]
        if r.outcome in (FAILED, REJECTED, AMBIGUOUS):
            checks_outcome.note(r.outcome)

    # Creations.
    created_tokens = {}
    for r in records:
        if r.kind not in CREATE_KINDS:
            continue
        token = token_of(r.values.get("title"))
        if token:
            created_tokens[token] = r
        if r.outcome == OK:
            seen = objects.get(r.target, {"exists": False})
            if not seen["exists"]:
                checks["created"].fail(f"{r.op}: {r.target} is missing")
            elif seen["title"] != r.values.get("title"):
                checks["created"].fail(f"{r.op}: {r.target} has another title")
            else:
                checks["created"].ok()
    # A new object is known when the token of its title is the token of a
    # creation of the ledger.
    present_tokens = set()
    for path, extra in observed["extra"].items():
        token = token_of(extra["title"])
        record = created_tokens.get(token)
        if record is None:
            checks["phantoms"].fail(f"{path}: {extra['type']} {extra['title']!r}")
            continue
        present_tokens.add(token)
        checks["phantoms"].ok()
        if record.outcome == FAILED:
            checks["failed_writes"].fail(
                f"{record.op}: refused creation committed at {path}"
            )
        elif record.outcome == AMBIGUOUS:
            checks["created"].note("ambiguous_committed")
        elif record.outcome == REJECTED:
            checks["created"].note("rejected_committed")
    for token, record in created_tokens.items():
        if token in present_tokens:
            continue
        if record.outcome == FAILED:
            checks["failed_writes"].ok()
        elif record.outcome == AMBIGUOUS:
            checks["created"].note("ambiguous_absent")
        elif record.outcome == OK:
            pass  # counted above as missing

    duplicates = len(observed["duplicate_uids"]) + len(
        observed["duplicate_history_ids"]
    )
    for uid, paths in list(observed["duplicate_uids"].items())[:EXAMPLES]:
        checks["unique_ids"].fail(f"UID {uid}: {paths}")
    for hid, paths in list(observed["duplicate_history_ids"].items())[:EXAMPLES]:
        checks["unique_ids"].fail(f"history id {hid}: {paths}")
    if not duplicates:
        checks["unique_ids"].ok(observed["content_objects"])

    # Edits: group by target and field.
    edits: dict[tuple[str, str], list[Record]] = defaultdict(list)
    for r in records:
        if r.kind in EDIT_KINDS and r.target:
            for field in fields_of(r):
                edits[(r.target, field)].append(r)
    for (target, field), ops in sorted(edits.items()):
        seen = objects.get(target)
        if not seen or not seen["exists"]:
            if any(r.outcome == OK for r in ops):
                checks["edits"].fail(f"{target}: missing")
            continue
        writers = {r.user for r in ops if r.outcome in (OK, AMBIGUOUS)}
        allowed = (_allowed_single if len(writers) <= 1 else _allowed_multi)(ops, field)
        if allowed is None:
            continue
        value = seen[field]
        if value in allowed:
            checks["edits"].ok()
        else:
            checks["edits"].fail(
                f"{target} {field}: {value!r} is not the last acknowledged value "
                f"({len(writers)} writers)"
            )
    by_target: dict[str, list[Record]] = defaultdict(list)
    for r in records:
        if r.kind in EDIT_KINDS and r.target:
            by_target[r.target].append(r)
    for target, ops in sorted(by_target.items()):
        seen = objects.get(target)
        item = items.get(target)
        if not seen or not seen["exists"] or item is None:
            continue
        added = seen["versions"] - item["versions"]
        acknowledged = sum(1 for r in ops if r.outcome == OK)
        unknown = sum(1 for r in ops if r.outcome in (AMBIGUOUS, REJECTED))
        if acknowledged <= added <= acknowledged + unknown:
            checks["versions"].ok()
        else:
            checks["versions"].fail(
                f"{target}: {added} versions added for {acknowledged} acknowledged "
                f"and {unknown} unknown edits"
            )
        titles = seen.get("version_titles")
        if titles is not None:
            missing = [
                r.values["title"]
                for r in ops
                if r.outcome == OK
                and "title" in r.values
                and r.values["title"] not in titles
            ]
            if missing:
                checks["version_titles"].fail(
                    f"{target}: {len(missing)} acknowledged titles missing"
                )
            else:
                checks["version_titles"].ok()

    # Workflow.
    transitions: dict[str, list[Record]] = defaultdict(list)
    for r in records:
        if r.kind in WORKFLOW_KINDS and r.target:
            transitions[r.target].append(r)
    for target, ops in sorted(transitions.items()):
        seen = objects.get(target)
        item = items.get(target)
        if not seen or not seen["exists"] or item is None:
            continue
        ops.sort(key=lambda r: r.start)
        acknowledged = [r for r in ops if r.outcome == OK]
        unknown = sum(1 for r in ops if r.outcome in (AMBIGUOUS, REJECTED))
        if acknowledged:
            last = acknowledged[-1]
            allowed = {last.values["state"]} | {
                r.values.get("state")
                for r in ops
                if r.outcome in (AMBIGUOUS, REJECTED) and r.start > last.start
            }
            if seen["state"] not in allowed:
                checks["workflow"].fail(
                    f"{target}: state {seen['state']} after an acknowledged "
                    f"{last.values['action']}"
                )
                continue
        added = seen["workflow_history"] - item.get("workflow_history", 0)
        if len(acknowledged) <= added <= len(acknowledged) + unknown:
            checks["workflow"].ok()
        else:
            checks["workflow"].fail(
                f"{target}: {added} workflow history entries for {len(acknowledged)} "
                f"acknowledged and {unknown} unknown transitions"
            )

    # Permissions, catalog and search on every observed object.
    for path, seen in sorted(objects.items()):
        if not seen["exists"]:
            continue
        if seen["state"] is not None:
            expected = seen["state"] == corpus.PUBLISHED
            if seen["anonymous_view"] == expected:
                checks["permissions"].ok()
            else:
                checks["permissions"].fail(
                    f"{path}: anonymous View {seen['anonymous_view']} "
                    f"in state {seen['state']}"
                )
        brains = seen["catalog"]
        if len(brains) != 1:
            checks["catalog"].fail(f"{path}: {len(brains)} catalog entries")
            continue
        brain = brains[0]
        problems = []
        if brain["Title"] != seen["title"]:
            problems.append("Title")
        if brain["review_state"] != seen["state"] and not (
            brain["review_state"] in (None, "") and seen["state"] is None
        ):
            problems.append("review_state")
        if brain["modified"] != seen["modified"]:
            problems.append("modified")
        if seen["state"] is not None and brain["anonymous"] != seen["anonymous_view"]:
            problems.append("allowedRolesAndUsers")
        if problems:
            checks["catalog"].fail(f"{path}: stale {', '.join(problems)}")
        else:
            checks["catalog"].ok()
    for name in ("catalog_orphans", "uncatalogued"):
        for path in observed.get(name, []):
            checks["catalog"].fail(f"{path}: {name}")
    if observed.get("catalog_duplicates"):
        checks["catalog"].fail(
            f"{observed['catalog_duplicates']} duplicate catalog paths"
        )

    tokens = observed.get("tokens", {})
    owners = {}
    for r in records:
        token = token_of(r.values.get("title"))
        if token and r.target:
            owners[token] = r.target
    for token, found in sorted(tokens.items()):
        owner = owners.get(token)
        seen = objects.get(owner) if owner else None
        if not seen or not seen["exists"]:
            continue
        if token == token_of(seen["title"]):
            if owner in found:
                checks["search"].ok()
            else:
                checks["search"].fail(
                    f"{owner}: the current title token {token} misses it"
                )
        elif owner in found:
            checks["search"].fail(f"{owner}: superseded token {token} still finds it")
        else:
            checks["search"].ok()

    result = {name: check.to_json() for name, check in checks.items()}
    violations = sum(check.violations for check in checks.values())
    return {
        "verdict": "pass" if violations == 0 else "fail",
        "violations": violations,
        "checks": result,
    }
