"""The repair of a damaged catalog, for the catalog mirror (ADR 0059).

The mirror refuses a catalog whose `uids` is not the inverse of `paths`
(`refused`), and an index whose reverse map names a rid outside `paths`
or whose UUIDs do not match its forward entries (`index_unavailable`). It
never guesses a value. This module finds that damage and repairs it in
the catalog itself. It changes nothing in the mirror.

`find_damage(catalog)` reads the catalog and writes nothing. `catalog` is
the `portal_catalog` tool of a site. It gives a dict with one entry for
each kind of damage, each with `found`, the count, and `rids`, the first
`SAMPLE` rids with their paths:

- `duplicates` (a path catalogued twice): the rids of `paths` that
  `uids[paths[rid]]` does not name;
- `stale_uids`: the entries of `uids` whose rid does not map back to that
  path in `paths`;
- `orphan_data`: the rids of `data` that are not in `paths`;
- `ghosts` (a ghost rid): the rids of the reverse map of an index that are
  not in `paths`, with the names of these indexes;
- `uuid_entries` (a UUIDIndex without its forward entry): the rids of the
  `_unindex` of a UUIDIndex whose UUID `_index` does not map to that rid,
  and the forward entries whose rid does not hold that UUID; `indexes`
  gives `_length`, `len(_index)` and `len(_unindex)` of each UUIDIndex
  whose `_length` differs from `len(_index)`;
- `half_catalogued` (a half-catalogued row): the rids of `paths` without an
  entry in one of `ROW_INDEXES`, the indexes of Plone that hold each
  content object, with the names of the missing indexes.

`length` gives the `Length` of the catalog and `len(data)`. `damaged` tells
if the catalog has damage of any kind.

`repair(catalog, dry_run=True)` gives the report of `find_damage` with a
dry run, and writes nothing. Else it repairs each kind in the current
transaction, and the caller commits:

- `duplicates`, `orphan_data` and `stale_uids`: it drops the entries of
  `uids` that do not map back, removes each extra rid from every index,
  then from `paths` and `data`, and drops the rows of `data` outside
  `paths`. A path whose only rid `uids` does not name gets its entry back;
- `ghosts`: it unindexes each ghost rid from each index that holds it;
- `uuid_entries`: it puts each missing forward entry back from `_unindex`,
  drops each forward entry without its reverse entry, and sets `_length`
  to `len(_index)`;
- `half_catalogued`: it catalogs the object again with all indexes, or
  uncatalogs the rid when its path does not traverse to an object with
  that path;
- it sets `Length` to `len(data)`.

The stock `unindex_object` of a UUIDIndex deletes `_index[uuid]` whichever
rid it names, and the stock `unindex_object` of an ExtendedPathIndex
deletes `_index_items[path]` so. So the repair removes a rid from these
indexes with care: a forward entry of another rid stays.

The report gives, for each kind, `found` and `repaired`, `unrepaired`
with the rid, the path and the reason of each rid that the repair left,
and `mirror`: `next_open` when the repair changed `paths` or `uids`. The
mirror refused such a catalog, and a refused mirror builds again only at
the next open of the store: the start of the process that holds it. Else
`commit`: the mirror reads the commit, and an index that it refused builds
again in the background. `None` when the repair changed nothing.

A row of `half_catalogued` needs a request, as the indexers of Plone can
look up views. Outside a request, as in `bin/instance run`, the repair
makes one, with `IDefaultBrowserLayer` and the browser layers of the site.

`RepairView` is `@@zoe-catalog-repair` on a site, for a Manager. A GET
gives the dry run as JSON. A POST needs the token of plone.protect; with
`repair=1`, it repairs, commits and gives the report.

Only the functions import Plone.
"""

import contextlib
import json
import logging

import transaction

__all__ = ["RepairView", "ROW_INDEXES", "SAMPLE", "find_damage", "repair"]

logger = logging.getLogger("zoe.plone.catalog_repair")

#: The rids that a report gives for each kind.
SAMPLE = 20

#: The indexes of Plone that hold each content object.
ROW_INDEXES = ("portal_type", "path", "allowedRolesAndUsers", "effectiveRange")

# The kinds of damage, in the order of the repair.
_KINDS = (
    "duplicates",
    "stale_uids",
    "orphan_data",
    "ghosts",
    "uuid_entries",
    "half_catalogued",
)

_missing = object()


def find_damage(catalog):
    """Find the damage of the catalog of the tool `catalog`. Write
    nothing. Return the report of the module docstring."""
    found = _find(_inner(catalog))
    return _report(found)


def repair(catalog, dry_run=True):
    """Repair the catalog of the tool `catalog` in the current transaction.
    With `dry_run`, only find the damage. Return the report of the module
    docstring. The caller commits."""
    inner = _inner(catalog)
    found = _find(inner)
    report = _report(found)
    report["dry_run"] = bool(dry_run)
    if dry_run:
        return report
    repaired = dict.fromkeys(_KINDS, 0)
    unrepaired = []

    def left(kind, rid, path, reason):
        unrepaired.append({"kind": kind, "rid": rid, "path": path, "reason": reason})

    _repair_paths(inner, found, repaired, left)
    _repair_ghosts(inner, found, repaired)
    _repair_uuids(inner, repaired, left)
    _repair_rows(catalog, inner, found, repaired, left)
    length = len(inner.data)
    length_repaired = _set_length(inner, "_length", length)
    report["unrepaired"] = unrepaired
    for kind in _KINDS:
        report[kind]["repaired"] = repaired[kind]
    report["length"]["repaired"] = length_repaired
    if repaired["duplicates"] or repaired["stale_uids"]:
        report["mirror"] = "next_open"
    elif any(repaired.values()) or length_repaired:
        report["mirror"] = "commit"
    else:
        report["mirror"] = None
    if any(repaired.values()) or length_repaired or unrepaired:
        logger.warning(
            "catalog repair of %s: repaired %s, Length %s, %d left",
            "/".join(catalog.getPhysicalPath()),
            {k: n for k, n in repaired.items() if n},
            "set" if length_repaired else "unchanged",
            len(unrepaired),
        )
    return report


def _inner(catalog):
    """The `Catalog` of the tool `catalog`, wrapped in the tool."""
    return catalog._catalog


def _class_name(obj):
    from Acquisition import aq_base

    cls = type(aq_base(obj))
    return f"{cls.__module__}.{cls.__qualname__}"


_SECURITY = "zoe.plone.security.ZoeSecurityIndex"


def _reverse(index):
    """The reverse map of `index` (rid to value), or None for an index
    without one in the catalog."""
    from Acquisition import aq_base

    base = aq_base(index)
    if _class_name(base) == _SECURITY:
        return base._inputs
    unindex = getattr(base, "_unindex", None)
    if unindex is not None and hasattr(unindex, "keys"):
        return unindex
    # ZCTextIndex: the words of each document.
    words = getattr(getattr(base, "index", None), "_docwords", None)
    if words is not None and hasattr(words, "keys"):
        return words
    return None


def _is_uuid_index(index):
    from Acquisition import aq_base
    from Products.PluginIndexes.UUIDIndex.UUIDIndex import UUIDIndex

    return isinstance(aq_base(index), UUIDIndex)


def _keys(tree):
    """The keys of `tree` as an `LLSet`."""
    from BTrees.LLBTree import LLSet

    return LLSet(tree.keys())


def _find(inner):
    """The damage of the `Catalog` `inner`, as lists of rids and details."""
    from BTrees.LLBTree import difference

    paths, uids, data = inner.paths, inner.uids, inner.data
    stale = [(path, rid) for path, rid in uids.items() if paths.get(rid) != path]
    stale_paths = {path for path, _ in stale}
    duplicates = []
    for rid, path in paths.items():
        named = uids.get(path)
        if named != rid:
            duplicates.append(rid)
    extras = set(duplicates)
    rows = _keys(paths)
    orphans = list(difference(_keys(data), rows))
    ghosts = {}
    uuids = []
    lengths = {}
    half = {}
    names = list(inner.indexes.keys())
    for name in names:
        index = inner.getIndex(name)
        reverse = _reverse(index)
        if reverse is None:
            continue
        held = _keys(reverse)
        for rid in difference(held, rows):
            ghosts.setdefault(rid, []).append(name)
        if name in ROW_INDEXES:
            for rid in difference(rows, held):
                if rid not in extras:
                    half.setdefault(rid, []).append(name)
        if _is_uuid_index(index):
            from Acquisition import aq_base

            base = aq_base(index)
            for rid, uuid in base._unindex.items():
                if rid in extras or rid not in paths:
                    continue
                if base._index.get(uuid, _missing) != rid:
                    uuids.append((name, rid))
            for uuid, rid in base._index.items():
                if base._unindex.get(rid, _missing) != uuid:
                    uuids.append((name, rid))
            length = base._length()
            if length != len(base._index):
                lengths[name] = {
                    "_length": length,
                    "_index": len(base._index),
                    "_unindex": len(base._unindex),
                }
    return {
        "paths": paths,
        "duplicates": duplicates,
        "stale_uids": stale,
        "stale_paths": stale_paths,
        "orphan_data": orphans,
        "ghosts": ghosts,
        "uuid_entries": uuids,
        "lengths": lengths,
        "half_catalogued": half,
        "length": (len(inner), len(data)),
    }


def _sample(items):
    return items[:SAMPLE]


def _report(found):
    paths = found["paths"]
    report = {
        "duplicates": {
            "found": len(found["duplicates"]),
            "rids": [
                {"rid": rid, "path": paths.get(rid)}
                for rid in _sample(found["duplicates"])
            ],
        },
        "stale_uids": {
            "found": len(found["stale_uids"]),
            "rids": [
                {"rid": rid, "path": path} for path, rid in _sample(found["stale_uids"])
            ],
        },
        "orphan_data": {
            "found": len(found["orphan_data"]),
            "rids": [
                {"rid": rid, "path": None} for rid in _sample(found["orphan_data"])
            ],
        },
        "ghosts": {
            "found": len(found["ghosts"]),
            "rids": [
                {"rid": rid, "path": None, "indexes": names}
                for rid, names in _sample(sorted(found["ghosts"].items()))
            ],
        },
        "uuid_entries": {
            "found": len(found["uuid_entries"]),
            "rids": [
                {"rid": rid, "path": paths.get(rid), "index": name}
                for name, rid in _sample(found["uuid_entries"])
            ],
            "indexes": found["lengths"],
        },
        "half_catalogued": {
            "found": len(found["half_catalogued"]),
            "rids": [
                {"rid": rid, "path": paths.get(rid), "indexes": names}
                for rid, names in _sample(sorted(found["half_catalogued"].items()))
            ],
        },
    }
    length, data = found["length"]
    report["length"] = {"Length": length, "data": data}
    report["damaged"] = bool(
        any(report[kind]["found"] for kind in _KINDS)
        or found["lengths"]
        or length != data
    )
    return report


def _remove(inner, index, rid):
    """Remove the rid `rid` from `index`, a forward entry of another rid
    kept. Return True when the index held the rid."""
    from Acquisition import aq_base

    base = aq_base(index)
    if _is_uuid_index(base):
        uuid = base._unindex.get(rid, _missing)
        if uuid is _missing:
            return False
        del base._unindex[rid]
        if base._index.get(uuid, _missing) == rid:
            del base._index[uuid]
            base._length.change(-1)
        return True
    items = getattr(base, "_index_items", None)
    if items is not None and hasattr(base, "_unindex"):
        # An ExtendedPathIndex: one rid for each path in `_index_items`.
        path = base._unindex.get(rid, _missing)
        if path is _missing:
            return False
        kept = items.get(path, _missing)
        index.unindex_object(rid)
        if kept is not _missing and kept != rid:
            items[path] = kept
        elif path not in items:
            owner = inner.uids.get(path, _missing)
            if owner is not _missing and base._unindex.get(owner) == path:
                items[path] = owner
        return True
    reverse = _reverse(base)
    if reverse is not None and rid not in reverse:
        return False
    index.unindex_object(rid)
    return True


def _drop_rid(inner, rid):
    """Remove the rid `rid` from every index, then from `paths` and
    `data`."""
    for name in list(inner.indexes.keys()):
        _remove(inner, inner.getIndex(name), rid)
    if rid in inner.paths:
        del inner.paths[rid]
    if rid in inner.data:
        del inner.data[rid]


def _repair_paths(inner, found, repaired, left):
    paths, uids, data = inner.paths, inner.uids, inner.data
    for path, rid in found["stale_uids"]:
        if uids.get(path, _missing) == rid:
            del uids[path]
            repaired["stale_uids"] += 1
    by_path = {}
    for rid in found["duplicates"]:
        path = paths.get(rid)
        if path is not None:
            by_path.setdefault(path, []).append(rid)
    for path, rids in by_path.items():
        named = uids.get(path, _missing)
        if named is _missing or paths.get(named) != path:
            if len(rids) == 1:
                uids[path] = rids[0]
                repaired["duplicates"] += 1
            else:
                for rid in rids:
                    left(
                        "duplicates",
                        rid,
                        path,
                        "several rids hold this path and uids names none of them",
                    )
            continue
        for rid in rids:
            _drop_rid(inner, rid)
            repaired["duplicates"] += 1
    for rid in found["orphan_data"]:
        if rid in data and rid not in paths:
            del data[rid]
            repaired["orphan_data"] += 1


def _repair_ghosts(inner, found, repaired):
    for rid, names in sorted(found["ghosts"].items()):
        if rid in inner.paths:
            continue
        removed = False
        for name in names:
            if name in inner.indexes:
                removed |= _remove(inner, inner.getIndex(name), rid)
        repaired["ghosts"] += int(removed)


def _repair_uuids(inner, repaired, left):
    """Make each UUIDIndex consistent: each UUID of `_unindex` maps back to
    its rid in `_index`, each forward entry has its reverse entry, and
    `_length` is `len(_index)`."""
    from Acquisition import aq_base

    for name in list(inner.indexes.keys()):
        index = inner.getIndex(name)
        if not _is_uuid_index(index):
            continue
        base = aq_base(index)
        for rid, uuid in list(base._unindex.items()):
            named = base._index.get(uuid, _missing)
            if named == rid:
                continue
            if named is _missing or base._unindex.get(named, _missing) != uuid:
                base._index[uuid] = rid
                repaired["uuid_entries"] += 1
            else:
                left(
                    "uuid_entries",
                    rid,
                    inner.paths.get(rid),
                    f"the rid {named} holds the same UUID in {name}",
                )
        for uuid, rid in list(base._index.items()):
            if base._unindex.get(rid, _missing) != uuid:
                del base._index[uuid]
                repaired["uuid_entries"] += 1
        _set_length(base, "_length", len(base._index))


def _set_length(obj, name, value):
    """Set the `Length` `name` of `obj` to `value`. Return True when it
    changed."""
    from BTrees.Length import Length

    length = getattr(obj, name, None)
    if isinstance(length, Length):
        if length() == value:
            return False
        length.set(value)
        return True
    setattr(obj, name, Length(value))
    return True


def _repair_rows(catalog, inner, found, repaired, left):
    """Catalog each half-catalogued object again with all indexes, or drop
    its rid when its path does not traverse."""
    from ZODB.POSException import ConflictError

    rows = [
        (rid, names)
        for rid, names in sorted(found["half_catalogued"].items())
        if rid in inner.paths
    ]
    if not rows:
        return
    with _with_request(catalog) as tool:
        root = tool.getPhysicalRoot()
        for rid, names in rows:
            path = inner.paths[rid]
            obj = _traverse(root, path)
            if obj is None:
                if inner.uids.get(path, _missing) == rid:
                    del inner.uids[path]
                _drop_rid(inner, rid)
                repaired["half_catalogued"] += 1
                continue
            try:
                tool.catalog_object(obj, uid=path)
            except ConflictError:
                raise
            except Exception as error:
                left(
                    "half_catalogued",
                    rid,
                    path,
                    f"the reindex raised {type(error).__name__}: {error}",
                )
                continue
            missing = [
                name
                for name in names
                if name in inner.indexes
                and rid not in (_reverse(inner.getIndex(name)) or (rid,))
            ]
            if missing:
                left(
                    "half_catalogued",
                    rid,
                    path,
                    f"the reindex gave no entry in {', '.join(missing)}",
                )
            else:
                repaired["half_catalogued"] += 1


def _traverse(root, path):
    """The object at `path` from the Zope root `root`, or None when the path
    does not traverse to an object with that physical path."""
    try:
        obj = root.unrestrictedTraverse(path)
    except (AttributeError, KeyError, LookupError, TypeError, ValueError):
        return None
    get = getattr(obj, "getPhysicalPath", None)
    if get is None or "/".join(get()) != path:
        # Acquisition found an object of another path.
        return None
    return obj


@contextlib.contextmanager
def _with_request(catalog):
    """Yield the tool `catalog` with a request: the request of the thread,
    or else a new one with `IDefaultBrowserLayer` and the browser layers of
    the site, and the site as the current site."""
    from Acquisition import aq_base, aq_inner, aq_parent
    from zope.component.hooks import getSite, setSite
    from zope.globalrequest import getRequest, setRequest

    if getRequest() is not None:
        yield catalog
        return
    from plone.browserlayer.utils import registered_layers
    from Testing.makerequest import makerequest
    from zope.interface import alsoProvides
    from zope.publisher.interfaces.browser import IDefaultBrowserLayer

    old_site = getSite()
    app = makerequest(aq_base(catalog.getPhysicalRoot()))
    tool = app.unrestrictedTraverse(catalog.getPhysicalPath())
    site = aq_parent(aq_inner(tool))
    request = app.REQUEST
    setRequest(request)
    try:
        setSite(site)
        alsoProvides(request, IDefaultBrowserLayer)
        layers = registered_layers()
        if layers:
            alsoProvides(request, *layers)
        yield tool
    finally:
        setRequest(None)
        setSite(old_site)


class RepairView:
    """`@@zoe-catalog-repair`: the dry run of the repair of the catalog of
    the site as JSON on a GET. A POST needs the token of plone.protect;
    with `repair=1`, it repairs, commits, and gives the report."""

    def __init__(self, context, request):
        self.context = context
        self.request = request

    def __call__(self):
        from Products.CMFCore.utils import getToolByName

        tool = getToolByName(self.context, "portal_catalog")
        request = self.request
        if request.get("REQUEST_METHOD", "GET").upper() == "POST":
            from plone.protect import CheckAuthenticator

            # A missing or a bad token raises Forbidden.
            CheckAuthenticator(request)
            if request.form.get("repair") not in (None, "", "0"):
                report = repair(tool, dry_run=False)
                transaction.commit()
            else:
                report = repair(tool, dry_run=True)
        else:
            report = repair(tool, dry_run=True)
        request.response.setHeader("Content-Type", "application/json")
        request.response.setHeader("Cache-Control", "no-store")
        return json.dumps(report)
