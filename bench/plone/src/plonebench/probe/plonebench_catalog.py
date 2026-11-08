"""The query log of a Zope process: one JSON line for each call of
`Products.ZCatalog.Catalog.Catalog.search`.

The WSGI probe (`plonebench_probe.py`) installs it at the first request
when the variable `PLONEBENCH_CATALOG_LOG` names the file of the log. The
runner sets the variable for each instance with `--catalog-log`. A script
can call `install(path)` itself. The probe wraps the search that the
process has at the install, so it records each call, also one that the
catalog backend of zoe.plone answers from the mirror.

Each line is a JSON object of the version `VERSION` of the format:

- `v`: the version;
- `t`: the time of the call, in seconds since the epoch;
- `catalog`: the physical path of the catalog tool, as a list;
- `query`: the query as `Catalog.search` got it, in the typed form of
  `encode`;
- `sort`: the id of the sort index, a list of ids when the call got a
  list, or None; `reverse`, `limit` and `merge`: the other arguments of
  the call;
- `indexes`: the class of each index that the query or the sort names;
- `changed`: true when the Connection of the catalog holds a registered
  object of the catalog or a savepoint record, as in a transaction that
  changed the catalog;
- `n` and `a`: `len()` and `actual_result_count` of the result;
- `replayable`: false when the query holds a value of a type that the
  format does not carry (`{"$": "other"}`).

The typed form: None, a bool, an int, a float, a str and a list stay as
they are; a tuple is `{"$": "tuple", "v": [...]}`, a set `{"$": "set",
"v": [...]}`, a mapping `{"$": "dict", "v": [[key, value]...]}`, a
`DateTime` `{"$": "DateTime", "v": [micros, naive, zone]}`, a
`datetime.datetime` or a `datetime.date` `{"$": "datetime"}` or
`{"$": "date"}` with its ISO text. `plonebench.catalog.log` reads it.

The log holds the values of the queries: a production log stays on the
runner of its site. It needs only the standard library at import.
"""

from __future__ import annotations

import datetime
import json
import os
import threading
import time

#: The version of the format.
VERSION = 1

_lock = threading.Lock()
_state = {"out": None, "errors": 0}

# The modules of the classes of the objects of a catalog.
_CATALOG_MODULES = (
    "BTrees.",
    "Products.ZCatalog.",
    "Products.PluginIndexes.",
    "Products.ExtendedPathIndex.",
)


class _Other(Exception):
    """A value of a type that the format does not carry."""


def encode(value, depth=0):
    """Return the typed form of `value`. A value of another type gives
    `{"$": "other", "class": NAME}`."""
    if depth > 32:
        return {"$": "other", "class": "deep"}
    if value is None or isinstance(value, (bool, int, float, str)):
        return value
    if isinstance(value, list):
        return [encode(item, depth + 1) for item in value]
    if isinstance(value, tuple):
        return {"$": "tuple", "v": [encode(item, depth + 1) for item in value]}
    if isinstance(value, (set, frozenset)):
        return {"$": "set", "v": [encode(item, depth + 1) for item in value]}
    cls = type(value)
    if cls.__name__ == "DateTime" and cls.__module__ == "DateTime.DateTime":
        micros, naive, zone = value.__getstate__()
        return {"$": "DateTime", "v": [micros, naive, zone]}
    if isinstance(value, datetime.datetime):
        return {"$": "datetime", "v": value.isoformat()}
    if isinstance(value, datetime.date):
        return {"$": "date", "v": value.isoformat()}
    keys = getattr(value, "keys", None)
    if callable(keys):
        return {
            "$": "dict",
            "v": [[encode(k, depth + 1), encode(value[k], depth + 1)] for k in keys()],
        }
    return {"$": "other", "class": f"{cls.__module__}.{cls.__qualname__}"}


def _replayable(encoded):
    if isinstance(encoded, list):
        return all(_replayable(item) for item in encoded)
    if isinstance(encoded, dict):
        kind = encoded.get("$")
        if kind == "other":
            return False
        if kind in ("tuple", "set"):
            return all(_replayable(item) for item in encoded["v"])
        if kind == "dict":
            return all(_replayable(k) and _replayable(v) for k, v in encoded["v"])
    return True


def _changed(catalog):
    """Tell if the Connection of `catalog` holds a registered object of a
    catalog or a savepoint record."""
    from Acquisition import aq_base

    jar = getattr(aq_base(catalog), "_p_jar", None)
    if jar is None:
        return False
    savepoint = getattr(jar, "_savepoint_storage", None)
    if savepoint is not None:
        try:
            if len(savepoint):
                return True
        except TypeError:
            return True
    for obj in list(getattr(jar, "_registered_objects", ())):
        if type(obj).__module__.startswith(_CATALOG_MODULES):
            return True
    return False


def _indexes(catalog, query, sort):
    """The class of each index that the query or the sort names."""
    from Acquisition import aq_base

    out = {}
    names = [name for name in query if name in catalog.indexes]
    sorts = [sort] if isinstance(sort, str) else sort or []
    for name in [*names, *sorts]:
        try:
            index = catalog.getIndex(name)
        except Exception:
            continue
        out[name] = type(aq_base(index)).__name__
    return out


def _sort_ids(sort_index):
    """The id of the sort index, or the list of the ids of the sort
    indexes, as `Catalog.search` got one or a list."""
    if sort_index is None:
        return None
    if isinstance(sort_index, (list, tuple)):
        return [index.getId() for index in sort_index]
    return sort_index.getId()


def _catalog_path(catalog):
    from Acquisition import aq_parent

    tool = aq_parent(catalog)
    try:
        return list(tool.getPhysicalPath())
    except Exception:
        return None


def _record(catalog, query, sort_index, reverse, limit, merge, result):
    encoded = encode(dict(query))
    sort = _sort_ids(sort_index)
    line = {
        "v": VERSION,
        "t": time.time(),
        "pid": os.getpid(),
        "catalog": _catalog_path(catalog),
        "query": encoded,
        "sort": sort,
        "reverse": encode(reverse),
        "limit": limit if isinstance(limit, int) else None,
        "merge": bool(merge),
        "indexes": _indexes(catalog, query, sort),
        "changed": _changed(catalog),
        "n": len(result),
        "a": getattr(result, "actual_result_count", len(result)),
        "replayable": _replayable(encoded),
    }
    text = json.dumps(line, sort_keys=True)
    with _lock:
        out = _state["out"]
        if out is not None:
            out.write(text + "\n")


def install(path):
    """Write the query log of this process to the end of the file `path`.
    A second call does nothing."""
    from Products.ZCatalog.Catalog import Catalog

    with _lock:
        if _state["out"] is not None:
            return
        directory = os.path.dirname(path)
        if directory:
            os.makedirs(directory, exist_ok=True)
        _state["out"] = open(path, "a", buffering=1)
    inner = Catalog.search

    def search(self, query, sort_index=None, reverse=False, limit=None, merge=True):
        result = inner(self, query, sort_index, reverse, limit, merge)
        try:
            _record(self, query, sort_index, reverse, limit, merge, result)
        except Exception:
            with _lock:
                _state["errors"] += 1
        return result

    Catalog.search = search


def install_from_environment():
    """Install the log when `PLONEBENCH_CATALOG_LOG` names its file."""
    path = os.environ.get("PLONEBENCH_CATALOG_LOG", "").strip()
    if path:
        install(path)
