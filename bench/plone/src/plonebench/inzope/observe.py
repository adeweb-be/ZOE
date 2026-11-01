"""Read the state of the site that the verification needs. Read-only.

    run.py ZOPE_CONF observe.py REQUEST_JSON OUT_JSON

REQUEST_JSON (`verification.request`) names the paths of the ledger, the
search tokens to look up, the paths whose version titles to read, the
containers of the creations, the ranges of the manifest around the
objects of the ledger, a fixed sample of the manifest, and the targets
file of the fixture (`workload.TargetsFile`). The targets file gives the
paths of the ranges and the children of a container in the manifest. The
script never commits: it runs before any repair or reindex, so it sees a
defect as the run left it.

What it reads follows the run, not the site:

- Each path of the ledger and of the sample: whether the object exists,
  its title, description and workflow state, the length of its workflow
  history, its versions (and their titles when asked), whether an
  anonymous user may View it, its UID, its history id, and its catalog
  entries (count, Title, review_state, modified, allowedRolesAndUsers has
  Anonymous).
- Each container of a creation: its children against the children of
  the manifest. A child that the manifest does not hold is a new object.
- The catalog entry of each item of the ranges, of each child of the
  container of an object of the ledger, and of each child of a container
  of a creation: `uids`, `paths`, `data` and the path index agree on its
  record id, and its container holds it. The script gives the Title and
  the review_state of each entry, and the verification compares them
  with the manifest.
- The entries next to the record id of each object of the ledger and of
  each new object, in `paths`, in `data` and in the path index: each one
  is in all three with one path, `uids` gives its record id back, and its
  container holds its object.
- The UID and the history id of each object that it observes find no
  other object in the catalog.
- The length of the catalog.

For each token it reads the paths that a query of the Title index finds.
"""

import json
import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager, noSecurityManager
from AccessControl.SpecialUsers import nobody, system
from Acquisition import aq_base
from plone import api
from plone.uuid.interfaces import ATTRIBUTE_NAME
from zope.component.hooks import setSite

from plonebench import workload

SITE_ID = "Plone"
# The record ids on each side of an entry that the run wrote. A bucket of
# an IOBTree holds at most 60 keys.
WINDOW = 64
# The examples of each kind of defect in the output.
EXAMPLES = 50
request_path, out_path = sys.argv[1:3]
with open(request_path) as file:
    request = json.load(file)

newSecurityManager(None, system)
site = app[SITE_ID]
setSite(site)
catalog = api.portal.get_tool("portal_catalog")
repository = api.portal.get_tool("portal_repository")
handler = api.portal.get_tool("portal_historyidhandler")
site_path = "/" + SITE_ID
records = catalog._catalog
uids, rid_paths, data = records.uids, records.paths, records.data
schema = records.schema
path_index = records.getIndex("path")
unindex = path_index._unindex
index_items = getattr(path_index, "_index_items", None)
has_history_index = "cmf_uid" in catalog.indexes()
targets = workload.TargetsFile.open(request["targets"])


def anonymous_can_view(obj):
    newSecurityManager(None, nobody)
    try:
        from AccessControl import getSecurityManager

        return bool(getSecurityManager().checkPermission("View", obj))
    finally:
        noSecurityManager()
        newSecurityManager(None, system)


def observe(path, with_titles):
    try:
        obj = site.unrestrictedTraverse(path)
    except (KeyError, AttributeError):
        return {"exists": False}
    if aq_base(obj) is aq_base(site):
        return {"exists": False}
    content.add(path)
    try:
        state = api.content.get_state(obj)
    except Exception:
        state = None
    try:
        history = repository.getHistoryMetadata(obj)
        versions = len(history) if history else 0
    except Exception:
        history, versions = None, 0
    titles = None
    if with_titles and versions:
        titles = []
        for version in range(versions):
            try:
                titles.append(repository.retrieve(obj, version).object.Title())
            except Exception as error:
                titles.append(f"<{type(error).__name__}>")
    workflow_history = getattr(aq_base(obj), "workflow_history", {}) or {}
    brains = catalog.unrestrictedSearchResults(
        path={"query": f"{site_path}/{path}", "depth": 0}
    )
    return {
        "exists": True,
        "title": obj.Title(),
        "description": obj.Description(),
        "state": state,
        "modified": str(obj.modified()),
        "uid": getattr(aq_base(obj), ATTRIBUTE_NAME, None),
        "versions": versions,
        "version_titles": titles,
        "workflow_history": sum(len(v) for v in workflow_history.values()),
        "anonymous_view": anonymous_can_view(obj),
        "history_id": handler.queryUid(obj, None),
        "catalog": [
            {
                "Title": text(b.Title),
                "review_state": text(b.review_state),
                "modified": str(b.modified),
                "anonymous": "Anonymous" in allowed(b),
            }
            for b in brains
        ],
    }


def text(value):
    """Return a metadata value of the catalog as JSON: a string, else None.
    An object without a workflow has `Missing.Value` as its review_state."""
    return value if isinstance(value, str) else None


def allowed(brain):
    """The values of allowedRolesAndUsers in the catalog for a brain. It is
    an index, not metadata, so the brain does not carry it."""
    data = catalog.getIndexDataForRID(brain.getRID())
    return data.get("allowedRolesAndUsers") or ()


def full(path):
    return f"{site_path}/{path}" if path else site_path


def relative(physical):
    return physical[len(site_path) + 1 :]


def parent_of(path):
    return path.rpartition("/")[0]


# The containers that the script read, by path: the object, or None when
# no container holds it. The script reaches each one through the ids of
# its own container, never through acquisition.
containers = {"": site}
ids = {}
content = set()


def container(path):
    if path not in containers:
        parent, _, name = path.rpartition("/")
        holder = container(parent)
        obj = None
        if holder is not None and name in ids_of(parent):
            obj = holder._getOb(name)
            if getattr(aq_base(obj), ATTRIBUTE_NAME, None) is not None:
                content.add(path)
        containers[path] = obj
    return containers[path]


def ids_of(path):
    """The ids of the children of the container at `path`."""
    if path not in ids:
        holder = container(path)
        ids[path] = set(holder.objectIds()) if holder is not None else set()
    return ids[path]


def holds(path):
    """True when the container of `path` holds an object of its id."""
    parent, _, name = path.rpartition("/")
    return name in ids_of(parent)


uncatalogued = []
orphans = []
duplicates = {"count": 0}
entries = {}
checked = set()


def check_entry(path):
    """Check the catalog entry of the object at `path`, from its path."""
    if path in checked:
        return
    checked.add(path)
    physical = full(path)
    rid = uids.get(physical)
    if rid is None:
        uncatalogued.append(path)
        return
    record = data.get(rid)
    problems = []
    if rid_paths.get(rid) != physical:
        problems.append("paths")
    if record is None:
        problems.append("data")
    if unindex.get(rid) != physical:
        problems.append("path index")
    if index_items is not None and index_items.get(physical) != rid:
        problems.append("path index items")
    if problems:
        uncatalogued.append(f"{path} (not in {', '.join(problems)})")
        return
    if path and not holds(path):
        orphans.append(f"{path} (no object)")
    entries[path] = {
        name: text(record[schema[name]]) if name in schema else None
        for name in ("Title", "review_state")
    }


window = set()


def check_window(rid):
    """Check the entries next to record id `rid`, from their record ids."""
    low, high = max(rid - WINDOW, -(2**31)), min(rid + WINDOW, 2**31 - 1)
    keys = set(rid_paths.keys(low, high)) | set(data.keys(low, high))
    keys |= set(unindex.keys(low, high))
    for key in sorted(keys - window):
        window.add(key)
        physical = rid_paths.get(key)
        indexed = unindex.get(key)
        problems = []
        if physical is None:
            problems.append("paths")
        if key not in data:
            problems.append("data")
        if indexed is None or indexed != physical:
            problems.append("path index")
        if problems:
            orphans.append(
                f"record {key} {physical or indexed} (not in {', '.join(problems)})"
            )
            continue
        owner = uids.get(physical)
        if owner is None:
            orphans.append(f"record {key} {physical} (not in uids)")
        elif owner != key:
            duplicates["count"] += 1
        elif physical != site_path and not holds(relative(physical)):
            orphans.append(f"{relative(physical)} (no object)")


def walk(obj, prefix):
    """Yield the content objects in `obj` and below, with their paths."""
    for name, child in obj.objectItems():
        path = f"{prefix}/{name}"
        base = aq_base(child)
        if getattr(base, ATTRIBUTE_NAME, None) is None:
            continue
        yield path, child
        if getattr(base, "isPrincipiaFolderish", False):
            yield from walk(child, path)


titles_for = set(request.get("version_titles", []))
observed = {}
for path in [*request["paths"], *request.get("sample", [])]:
    if path not in observed:
        observed[path] = observe(path, path in titles_for)

written = [path for path in request["paths"] if observed[path]["exists"]]
# The containers whose children the script lists: each container of a
# creation, and the container of each object of the ledger.
listed = sorted(
    set(request.get("containers", [])) | {parent_of(path) for path in written}
)

# New objects: the children of a listed container that the manifest does
# not hold, and what they hold. The script loads only these children.
extra = {}
new_objects = {}
children = []
for path in listed:
    holder = container(path)
    if holder is None:
        continue
    known = {child.rpartition("/")[2] for child in targets.children(path)}
    for name in sorted(ids_of(path)):
        child_path = f"{path}/{name}" if path else name
        if name in known:
            children.append(child_path)
            continue
        child = holder._getOb(name)
        if getattr(aq_base(child), ATTRIBUTE_NAME, None) is None:
            continue
        found = [(child_path, child)]
        if getattr(aq_base(child), "isPrincipiaFolderish", False):
            found += list(walk(child, child_path))
        for new_path, obj in found:
            content.add(new_path)
            new_objects[new_path] = obj
            extra[new_path] = {"title": obj.Title(), "type": obj.portal_type}

# The catalog entries, from the content: the items of the manifest next to
# each object of the ledger, and the children of each listed container.
for start, stop in request.get("neighbours", []):
    for path in targets.paths(start, stop):
        check_entry(path)
for path in [*written, *children, *new_objects]:
    check_entry(path)

# The catalog entries, from their record ids: the entries next to each
# object of the ledger, each container of a creation and each new object.
for path in [*written, *request.get("containers", []), *new_objects]:
    rid = uids.get(full(path))
    if rid is not None:
        check_window(rid)

# Unique ids: the UID and the history id of each object find no other
# object in the catalog.
duplicate_uids = {}
duplicate_history_ids = {}


def others(query, path):
    found = {relative(b.getPath()) for b in catalog.unrestrictedSearchResults(**query)}
    return sorted(found | {path}) if found - {path} else None


identities = [
    (path, seen["uid"], seen["history_id"])
    for path, seen in observed.items()
    if seen["exists"]
]
identities += [
    (path, getattr(aq_base(obj), ATTRIBUTE_NAME, None), handler.queryUid(obj, None))
    for path, obj in new_objects.items()
]
for path, uid, history_id in identities:
    if uid:
        paths = others({"UID": uid}, path)
        if paths:
            duplicate_uids[uid] = paths
    if history_id is not None and has_history_index:
        paths = others({"cmf_uid": history_id}, path)
        if paths:
            duplicate_history_ids[str(history_id)] = paths

tokens = {}
for token in request.get("tokens", []):
    # The four words of a code, in the Title index: an AND of the words.
    brains = catalog.unrestrictedSearchResults(Title=token)
    tokens[token] = sorted(relative(b.getPath()) for b in brains)

result = {
    "objects": observed,
    "extra": extra,
    "duplicate_uids": duplicate_uids,
    "duplicate_history_ids": duplicate_history_ids,
    "tokens": tokens,
    "content_objects": len(content),
    "catalog_entries": len(records),
    "catalog_duplicates": duplicates["count"],
    "catalog_orphans": sorted(orphans)[:EXAMPLES],
    "uncatalogued": sorted(uncatalogued)[:EXAMPLES],
    "entries": entries,
    "window_entries": len(window),
    "sample": request.get("sample", []),
}
with open(out_path, "w") as file:
    json.dump(result, file)
print(
    f"observe: {len(observed)} paths, {len(extra)} new objects, "
    f"{len(content)} content objects, {len(entries)} entries by path, "
    f"{len(window)} entries by record id, {len(records)} catalog entries"
)
targets.close()
transaction.abort()
app._p_jar.db().close()
