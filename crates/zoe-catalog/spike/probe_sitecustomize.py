"""Record each call of Catalog.search: the query shape, the sort, the
batch, and the catalog records that the transaction changed."""

import importlib.util
import io
import json
import os
import sys
import threading

LOG = os.environ.get("ZOE283_SEARCH_LOG")
TARGET = "Products.ZCatalog.Catalog"
_lock = threading.Lock()
_walked = {}


def _write(record):
    line = json.dumps(record, default=repr)
    with _lock:
        with open(LOG, "a") as out:
            out.write(line + "\n")


OPTIONS = ("operator", "range", "depth", "navtree", "navtree_start", "level", "usage")


def _options(value):
    out = {}
    for key in value.keys():
        if key in OPTIONS:
            out[key] = repr(value[key])[:40]
        elif key in ("query", "not"):
            inner = value[key]
            if isinstance(inner, (list, tuple, set, frozenset)):
                out[key] = "seq%d" % len(inner)
            else:
                out[key] = type(inner).__name__
        else:
            out[key] = "?"
    return out


def _shape(value):
    if isinstance(value, dict):
        return {"kind": "dict", "keys": sorted(str(k) for k in value), "opts": _options(value)}
    if hasattr(value, "keys") and hasattr(value, "get") and not isinstance(value, (str, bytes)):
        try:
            return {"kind": type(value).__name__, "keys": sorted(str(k) for k in value.keys()), "opts": _options(value)}
        except Exception:
            pass
    if isinstance(value, (list, tuple, set, frozenset)):
        return {"kind": "seq", "n": len(value)}
    return {"kind": type(value).__name__}


def _catalog_oids(catalog):
    import persistent

    seen = set()
    stack = [catalog]
    while stack:
        obj = stack.pop()
        if isinstance(obj, persistent.Persistent):
            oid = obj._p_oid
            if oid is not None:
                if oid in seen:
                    continue
                seen.add(oid)
            try:
                state = obj.__getstate__()
            except Exception:
                continue
            stack.append(state)
        elif isinstance(obj, dict):
            stack.extend(obj.values())
        elif isinstance(obj, (list, tuple)):
            stack.extend(obj)
    return seen


def _changed(catalog):
    """The registered objects of the catalog, their pickles and the new
    objects that they reach."""
    import persistent
    import transaction
    from zodbpickle import pickle as zpickle

    jar = catalog._p_jar
    if jar is None:
        return None
    registered = list(getattr(jar, "_registered_objects", ()))
    if not registered:
        return {"records": 0, "bytes": 0, "new": 0, "new_bytes": 0}
    from Acquisition import aq_base

    catalog = aq_base(catalog)
    key = catalog._p_oid
    txn = id(transaction.get())
    oids, last = _walked.get(key, (None, None))
    mine = [o for o in registered if oids is not None and o._p_oid in oids]
    if oids is None or (last != txn and len(mine) < len(registered)):
        oids = _catalog_oids(catalog)
        _walked[key] = (oids, txn)
        mine = [o for o in registered if o._p_oid in oids]
    if not mine:
        return {"records": 0, "bytes": 0, "new": 0, "new_bytes": 0}
    new = []
    newids = {}

    def pid(obj):
        if isinstance(obj, type) or not isinstance(obj, persistent.Persistent):
            return None
        if obj._p_oid is None:
            if id(obj) not in newids:
                newids[id(obj)] = len(new)
                new.append(obj)
            return ("new", newids[id(obj)])
        return obj._p_oid

    def size(obj):
        out = io.BytesIO()
        p = zpickle.Pickler(out, 3)
        p.persistent_id = pid
        p.dump((type(obj), None))
        p.dump(obj.__getstate__())
        return len(out.getvalue())

    import time as _time

    started = _time.perf_counter()
    total = sum(size(o) for o in mine)
    done = 0
    new_bytes = 0
    while done < len(new):
        new_bytes += size(new[done])
        done += 1
    pickle_seconds = _time.perf_counter() - started
    classes = {}
    for o in mine:
        name = type(o).__name__
        classes[name] = classes.get(name, 0) + 1
    return {
        "records": len(mine),
        "bytes": total,
        "new": len(new),
        "new_bytes": new_bytes,
        "pickle_seconds": pickle_seconds,
        "classes": classes,
    }


def _patch(module):
    Catalog = module.Catalog
    original = Catalog.search

    def search(self, query, *args, **kw):
        record = {"pid": os.getpid(), "test": os.environ.get("ZOE283_TAG", "")}
        try:
            indexes = {}
            for name, value in dict(query).items():
                index = self.indexes.get(name) if hasattr(self, "indexes") else None
                entry = {"shape": _shape(value)}
                if index is not None:
                    entry["meta_type"] = getattr(index, "meta_type", type(index).__name__)
                indexes[name] = entry
            record["query"] = indexes
            record["args"] = [repr(a)[:200] for a in args]
            sort_index = args[0] if args else kw.get("sort_index")
            if sort_index is not None:
                items = sort_index if isinstance(sort_index, (list, tuple)) else [sort_index]
                record["sort"] = [getattr(i, "id", getattr(i, "getId", lambda: repr(i))()) if not isinstance(i, str) else i for i in items]
                record["sort_types"] = [getattr(i, "meta_type", type(i).__name__) for i in items]
            record["kw"] = {k: repr(v)[:200] for k, v in kw.items()}
        except Exception as error:  # the probe never breaks a search
            record["probe_error"] = repr(error)
        try:
            record["changed"] = _changed(self)
        except Exception as error:
            record["changed_error"] = repr(error)
        result = original(self, query, *args, **kw)
        try:
            record["len"] = len(result)
            record["actual"] = getattr(result, "actual_result_count", None)
        except Exception:
            pass
        _write(record)
        return result

    Catalog.search = search


class _Finder:
    def find_spec(self, name, path, target=None):
        if name != TARGET:
            return None
        for finder in sys.meta_path:
            if finder is self or not hasattr(finder, "find_spec"):
                continue
            spec = finder.find_spec(name, path, target)
            if spec is not None:
                break
        else:
            return None
        loader = spec.loader
        original = loader.exec_module

        def exec_module(module):
            original(module)
            _patch(module)

        loader.exec_module = exec_module
        return spec


if LOG:
    sys.meta_path.insert(0, _Finder())
