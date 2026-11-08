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
query: only the names of the indexes, of their options and of the sort
indexes, and the numbers of `depth`, `level` and `navtree_start`.

The module needs only the standard library.
"""

from __future__ import annotations

import collections
import json
from pathlib import Path

#: The version of the format that this module reads.
VERSION = 1

# The options of a record of a query whose value the inventory keeps: a
# small int of the path index, never a value of the content.
NUMBERS = ("depth", "level", "navtree", "navtree_start")


class LogError(ValueError):
    """A line of the log that this version does not read."""


def read(path: Path | str) -> list[dict]:
    """Return the lines of the log at `path`. A line that is not a JSON
    object, or a line of another version, raises `LogError` with its
    number, and no value of the line."""
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
            lines.append(line)
    return lines


def read_all(paths) -> list[dict]:
    """The lines of each log of `paths`, in order."""
    lines = []
    for path in paths:
        lines.extend(read(path))
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
    the numbers of `NUMBERS`: `query` for a plain value."""
    pairs = _items(value)
    if not pairs:
        return ("query",)
    out = []
    for key, item in sorted(pairs, key=lambda pair: str(pair[0])):
        key = str(key)
        if key in NUMBERS and isinstance(item, int) and not isinstance(item, bool):
            out.append(f"{key} {item}")
        elif key == "operator" and item in ("and", "or"):
            out.append(f"operator {item}")
        elif key == "range" and isinstance(item, str):
            out.append(f"range {item}")
        else:
            out.append(key)
    return tuple(out)


def _order(value) -> str:
    """The order of a sort, as `ascending` or `descending`."""
    values = value if isinstance(value, list) else [value]
    return ",".join("descending" if v else "ascending" for v in values)


def inventory(lines: list[dict]) -> dict:
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
                keys[name] += 1
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
    ]
    return "\n".join(out)
