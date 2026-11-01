"""The verification of a run: the ledger against the state of the site.

`request(records, manifest)` names what `inzope/observe.py` must read.
`judge(records, manifest, observed)` checks it and returns one entry per
check, with its counts and up to EXAMPLES violations. Both are pure, so
the tests check the rules without Zope. `manifest` is a manifest in
memory, or the targets file of the fixture (`workload.TargetsFile`), so
the runner does not load the whole manifest.

The reads are bounded by the run, not by the site. The observer reads the
objects that the ledger names, the containers of the creations, a fixed
sample of the manifest, and catalog entries only around what the run
wrote: the items of the manifest next to an object of the ledger, the
children of its container, and the entries next to its record id. The
length of the catalog covers the rest of the site: a run that adds or
loses an entry anywhere changes it. A run writes only these objects,
these containers and the catalog buckets that hold their entries, so a
defect that a run makes lies there. `prepare` checks the fixture itself
once.

A timed-out or cut write is `ambiguous`: it may have committed. The rules
accept both of its results, and count them apart.
"""

from __future__ import annotations

import random
import re
from collections import defaultdict

from plonebench import corpus, workload
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
# The items of the manifest on each side of an object of the ledger whose
# catalog entry the observer checks. The build gives consecutive record
# ids in the order of the manifest, and a bucket of an IOBTree holds at
# most 60 keys: so these items hold the entries of the buckets that a
# write of the object rewrites.
NEIGHBOURS = 64
# The items of the fixed sample.
SAMPLE = 100
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


def container_of(record: Record) -> str | None:
    """Return the container of a creation: the folder that it names, else
    its target when it did not succeed, else the parent of its target."""
    folder = record.values.get("folder")
    if folder:
        return folder
    if not record.target:
        return None
    return record.target if record.outcome != OK else record.target.rpartition("/")[0]


def neighbour_ranges(indexes, count: int, width: int = NEIGHBOURS) -> list[list[int]]:
    """Return the ranges `[start, stop)` of the items of a manifest of
    `count` items that lie at most `width` items from one of `indexes`,
    merged and in order."""
    ranges: list[list[int]] = []
    for index in sorted(set(indexes)):
        start, stop = max(0, index - width), min(count, index + width + 1)
        if ranges and start <= ranges[-1][1]:
            ranges[-1][1] = max(ranges[-1][1], stop)
        else:
            ranges.append([start, stop])
    return ranges


def sample(manifest, size: int = SAMPLE) -> list[str]:
    """Return the fixed sample of the manifest: `size` paths drawn with the
    seed of the spec, in the order of the manifest. Each run of one
    fixture gets the same sample."""
    site = workload.view(manifest)
    count = len(site)
    rng = random.Random(f"{site.seed}:sample")
    picks = sorted(rng.sample(range(count), min(size, count)))
    paths: list[str] = []
    for start, stop in neighbour_ranges(picks, count, 0):
        paths.extend(site.paths(start, stop))
    return paths


def request(records: list[Record], manifest) -> dict:
    """Return what the observer reads after the run of `records`. Its size
    follows the ledger, not the site: the paths of the ledger, the
    containers of the creations, the ranges of the manifest around the
    objects of the ledger, and the fixed sample."""
    site = workload.view(manifest)
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
    containers = sorted(
        {
            container
            for record in records
            if record.kind in CREATE_KINDS
            and (container := container_of(record)) is not None
        }
    )
    # The objects of the ledger that the fixture holds: the objects that
    # the run wrote and the containers of its creations.
    indexes = [
        index
        for path in sorted(paths | set(containers))
        if (index := site.index(path)) is not None
    ]
    return {
        "paths": sorted(paths),
        "tokens": tokens[:MAX_TOKENS],
        "version_titles": titles,
        "containers": containers,
        "neighbours": neighbour_ranges(indexes, len(site)),
        "sample": sample(site),
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


def _same_state(catalog_state, state) -> bool:
    """True when the review_state of a catalog entry is the state `state`.
    An object without a workflow has an empty review_state."""
    return catalog_state == state or (catalog_state in (None, "") and state is None)


def judge(records: list[Record], manifest, observed: dict) -> dict:
    site = workload.view(manifest)
    objects = observed["objects"]
    entries = observed.get("entries", {})
    sampled = observed.get("sample", [])
    # The targets of the writes and the containers of the creations, with
    # any outcome: the run may have changed them, so the manifest does not
    # give their values.
    written = {r.target for r in _writes(records) if r.target} | {
        container
        for r in records
        if r.kind in CREATE_KINDS and (container := container_of(r)) is not None
    }
    items = site.items_by_path(written | set(entries) | set(sampled))
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
        "sample": Check(
            "sample",
            "each object of a fixed sample that the run did not write has the "
            "title, state, versions and workflow history of the manifest",
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
    duplicates += len(site.duplicate_uids)
    for uid in site.duplicate_uids[:EXAMPLES]:
        checks["unique_ids"].fail(f"UID {uid}: more than one item of the manifest")
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
    # The length of the catalog: the entries of the fixture and of each new
    # object. An entry that the catalog adds or loses anywhere changes it.
    if site.catalog_size is not None and "catalog_entries" in observed:
        expected = site.catalog_size + len(observed["extra"])
        if observed["catalog_entries"] == expected:
            checks["catalog"].ok()
        else:
            checks["catalog"].fail(
                f"the catalog holds {observed['catalog_entries']} entries, "
                f"not {expected}"
            )
    # The entries next to the writes of the run, against the manifest.
    for path, entry in sorted(entries.items()):
        item = items.get(path)
        if item is None or path in written:
            continue
        problems = []
        if "title" in item and entry["Title"] != item["title"]:
            problems.append("Title")
        if "state" in item and not _same_state(entry["review_state"], item["state"]):
            problems.append("review_state")
        if problems:
            checks["catalog"].fail(f"{path}: {', '.join(problems)} of the manifest")
        else:
            checks["catalog"].ok()
    # The fixed sample, against the manifest.
    for path in sampled:
        item = items.get(path)
        if item is None or path in written:
            continue
        seen = objects.get(path)
        if not seen or not seen["exists"]:
            checks["sample"].fail(f"{path}: missing")
            continue
        compared = [("title", "title"), ("state", "state"), ("versions", "versions")]
        # The build reads the workflow history of an item through
        # acquisition. So the manifest gives an item without a workflow,
        # such as a File, the history of its container.
        if item.get("state") is not None:
            compared.append(("workflow history", "workflow_history"))
        problems = [
            name for name, key in compared if key in item and seen[key] != item[key]
        ]
        if problems:
            checks["sample"].fail(f"{path}: {', '.join(problems)} of the manifest")
        else:
            checks["sample"].ok()

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
