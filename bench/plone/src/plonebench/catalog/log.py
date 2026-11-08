"""The query log of the catalog: its read, the decode of its typed values,
and the inventory of a set of logs (ADR 0059, decision 3).

The probe of the harness (`probe/plonebench_catalog.py`) writes the log,
one JSON line for each call of `Catalog.search`, in the version `VERSION`
of the format; the module docstring of the probe gives the format. `read`
refuses a line of another version. `decode` makes the Python values of a
query again, in the interpreter of a Plone instance. The inventory counts
the index types, the options, the sorts and their shares, and the share of
the searches in a transaction that changed the catalog, as the finding of
the catalog mirror gives them for the Plone suites. It holds no value of a
query: only the names of the index types and of the sort indexes, the
names of the options of `OPTIONS`, the keys of `KEYS`, and the counts. So
an inventory can leave the runner of a production site.

A line with `"kind": "unrecorded"` is no search: the probe writes it at
the exit of a process that could not record some calls, with their count
`n`. `read` keeps such lines out of its result and adds their counts to
`totals`.

The module needs only the standard library.
"""

from __future__ import annotations

import collections
import json
from pathlib import Path

#: The version of the format that this module reads.
VERSION = 1

# The options of a record of a query that the inventory names. Another
# option is `other`.
OPTIONS = (
    "query",
    "operator",
    "range",
    "not",
    "usage",
    "depth",
    "level",
    "navtree",
    "navtree_start",
)
# The keys of a query that name no index and that the inventory names: the
# keys of ZCatalog and of Plone. Another key, such as the name of a field
# of a form, is `other`.
KEYS = (
    "sort_on",
    "sort_order",
    "sort_limit",
    "b_start",
    "b_size",
    "Language",
    "show_inactive",
    "show_all",
    "metadata_fields",
    "use_site_search_settings",
    "fullobjects",
)


class LogError(ValueError):
    """A line of the log that this version does not read."""


def read(path: Path | str, totals: dict | None = None) -> list[dict]:
    """Return the lines of the searches of the log at `path`. A line that is
    not a JSON object, or a line of another version, raises `LogError` with
    its number, and no value of the line. The count of each line of the kind
    `unrecorded` goes to `totals["unrecorded"]`."""
    lines = []
    with open(path) as f:
        for number, text in enumerate(f, 1):
            if not text.strip():
                continue
            try:
                line = json.loads(text)
            except ValueError:
                raise LogError(f"line {number} of the log is not JSON") from None
            if not isinstance(line, dict):
                raise LogError(f"line {number} of the log is not an object")
            if line.get("v") != VERSION:
                raise LogError(
                    f"line {number} of the log has the version {line.get('v')!r}:"
                    f" this replay reads the version {VERSION}"
                )
            if line.get("kind") == "unrecorded":
                if totals is not None:
                    totals["unrecorded"] = totals.get("unrecorded", 0) + int(
                        line.get("n", 0)
                    )
                continue
            lines.append(line)
    return lines


def read_all(paths, totals: dict | None = None) -> list[dict]:
    """The lines of the searches of each log of `paths`, in order."""
    lines = []
    for path in paths:
        lines.extend(read(path, totals))
    return lines


def decode(value):
    """Return the Python value of the typed form `value`. A `DateTime`
    needs the interpreter of a Plone instance. `{"$": "other"}` raises
    `LogError`."""
    if isinstance(value, list):
        return [decode(item) for item in value]
    if not isinstance(value, dict):
        return value
    kind = value.get("$")
    if kind == "tuple":
        return tuple(decode(item) for item in value["v"])
    if kind == "set":
        return {decode(item) for item in value["v"]}
    if kind == "dict":
        return {decode(k): decode(v) for k, v in value["v"]}
    if kind == "DateTime":
        from DateTime import DateTime

        made = DateTime.__new__(DateTime)
        made.__setstate__(tuple(value["v"]))
        return made
    if kind == "datetime":
        import datetime

        return datetime.datetime.fromisoformat(value["v"])
    if kind == "date":
        import datetime

        return datetime.date.fromisoformat(value["v"])
    raise LogError(f"a value of the type {kind!r}")


def _items(encoded) -> list:
    """The pairs of a typed mapping, or an empty list."""
    if isinstance(encoded, dict) and encoded.get("$") == "dict":
        return encoded["v"]
    return []


def _options(value) -> tuple[str, ...]:
    """The names of the options of the value of an index in a query, with
    no value: `query` for a plain value, `other` for a name out of
    `OPTIONS`."""
    pairs = _items(value)
    if not pairs:
        return ("query",)
    names = {str(key) if str(key) in OPTIONS else "other" for key, _ in pairs}
    return tuple(sorted(names))


def _order(value) -> str:
    """The order of a sort, as `ascending` or `descending`."""
    values = value if isinstance(value, list) else [value]
    return ",".join("descending" if v else "ascending" for v in values)


def inventory(lines: list[dict], unrecorded: int = 0) -> dict:
    """The inventory of the searches of `lines`: by index type, by index
    type and options, by sort, the keys of the queries that name no index,
    such as `sort_limit`, `b_start` and `b_size`, and the shares of the
    searches in a transaction that changed the catalog,
    and of the searches that the replay cannot type."""
    calls = len(lines)
    types = collections.Counter()
    options = collections.Counter()
    sorts = collections.Counter()
    keys = collections.Counter()
    changed = 0
    other = 0
    for line in lines:
        classes = line.get("indexes") or {}
        pairs = {str(k): v for k, v in _items(line.get("query"))}
        seen = set()
        shapes = set()
        for name, value in pairs.items():
            kind = classes.get(name)
            if kind is None:
                keys[name if name in KEYS else "other"] += 1
                continue
            seen.add(kind)
            shapes.add((kind, ", ".join(_options(value))))
        # A share is of the calls: a call counts once for each type, and
        # once for each type with its options.
        types.update(seen)
        options.update(shapes)
        sort = line.get("sort")
        if not sort:
            sorts[("none", "")] += 1
        else:
            sort = [sort] if isinstance(sort, str) else sort
            names = [f"{n} ({classes.get(n, '?')})" for n in sort]
            limit = "sort_limit" if line.get("limit") is not None else "no limit"
            order = f"{_order(line.get('reverse'))}, {limit}"
            sorts[(" then ".join(names), order)] += 1
        changed += bool(line.get("changed"))
        other += not line.get("replayable", True)

    def share(n):
        return round(100 * n / calls, 2) if calls else 0.0

    return {
        "calls": calls,
        "index_types": [
            {"type": kind, "calls": n, "share": share(n)}
            for kind, n in types.most_common()
        ],
        "options": [
            {"type": kind, "options": opts, "calls": n, "share": share(n)}
            for (kind, opts), n in options.most_common()
        ],
        "sorts": [
            {"sort": name, "order": order, "calls": n, "share": share(n)}
            for (name, order), n in sorts.most_common()
        ],
        "keys": [
            {"key": key, "calls": n, "share": share(n)} for key, n in keys.most_common()
        ],
        "changed": {"calls": changed, "share": share(changed)},
        "not_replayable": {"calls": other, "share": share(other)},
        "unrecorded": unrecorded,
    }


def format_inventory(found: dict, title: str = "") -> str:
    """The inventory as Markdown tables."""
    out = []
    if title:
        out += [f"### {title}", ""]
    out += [f"{found['calls']} calls of `Catalog.search`.", ""]
    out += ["| Index type | Calls with it | Share |", "| --- | ---: | ---: |"]
    out += [
        f"| {row['type']} | {row['calls']:,} | {row['share']:.2f} % |"
        for row in found["index_types"]
    ]
    out += ["", "| Index type | Options | Share |", "| --- | --- | ---: |"]
    out += [
        f"| {row['type']} | {row['options']} | {row['share']:.2f} % |"
        for row in found["options"]
    ]
    out += ["", "| Sort | Order | Share |", "| --- | --- | ---: |"]
    out += [
        f"| {row['sort']} | {row['order']} | {row['share']:.2f} % |"
        for row in found["sorts"]
    ]
    out += ["", "| Key | Share |", "| --- | ---: |"]
    out += [f"| `{row['key']}` | {row['share']:.2f} % |" for row in found["keys"]]
    out += [
        "",
        f"In a transaction that changed the catalog: {found['changed']['calls']:,}"
        f" calls ({found['changed']['share']:.2f} %).",
        f"Not replayable (a value of another type): "
        f"{found['not_replayable']['calls']:,} calls"
        f" ({found['not_replayable']['share']:.2f} %).",
        f"Not recorded (the probe could not encode the call):"
        f" {found.get('unrecorded', 0):,} calls.",
    ]
    return "\n".join(out)
