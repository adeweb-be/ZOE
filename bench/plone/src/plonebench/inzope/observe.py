"""Read the state of the site that the verification needs. Read-only.

    run.py ZOPE_CONF observe.py REQUEST_JSON OUT_JSON

REQUEST_JSON names the paths to read, the search tokens to look up, the
paths whose version titles to read, and the paths of the manifest. The
script never commits: it runs before any repair or reindex, so it sees a
defect as the run left it.

For each path it reads: whether the object exists, its title, description
and workflow state, the length of its workflow history, its versions (and
their titles when asked), whether an anonymous user may View it, its
history id, and its catalog entries (count, Title, review_state,
modified, allowedRolesAndUsers has Anonymous). For each token it reads the
paths that a SearchableText query finds. It also lists every content
object that is not in the manifest, the UIDs and history ids held by more
than one object, and the size of the catalog against the content.
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

SITE_ID = "Plone"
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
                "Title": b.Title,
                "review_state": b.review_state,
                "modified": str(b.modified),
                "anonymous": "Anonymous" in allowed(b),
            }
            for b in brains
        ],
    }


def allowed(brain):
    """The values of allowedRolesAndUsers in the catalog for a brain. It is
    an index, not metadata, so the brain does not carry it."""
    data = catalog.getIndexDataForRID(brain.getRID())
    return data.get("allowedRolesAndUsers") or ()


def walk(container, prefix):
    for name, obj in container.objectItems():
        path = f"{prefix}/{name}" if prefix else name
        base = aq_base(obj)
        if getattr(base, ATTRIBUTE_NAME, None) is None:
            continue
        yield path, obj
        if getattr(base, "isPrincipiaFolderish", False):
            yield from walk(obj, path)


titles_for = set(request.get("version_titles", []))
observed = {path: observe(path, path in titles_for) for path in request["paths"]}

known = set(request["manifest_paths"])
extra = {}
uids = {}
history_ids = {}
content = 0
walked = set()
for path, obj in walk(site, ""):
    content += 1
    walked.add(path)
    uids.setdefault(getattr(aq_base(obj), ATTRIBUTE_NAME), []).append(path)
    history_id = handler.queryUid(obj, None)
    if history_id is not None:
        history_ids.setdefault(history_id, []).append(path)
    if path not in known:
        extra[path] = {"title": obj.Title(), "type": obj.portal_type}

tokens = {}
for token in request.get("tokens", []):
    brains = catalog.unrestrictedSearchResults(SearchableText=token)
    tokens[token] = sorted(b.getPath()[len(site_path) + 1 :] for b in brains)

brains = catalog.unrestrictedSearchResults(path=site_path)
# The site root has an entry of its own, with the empty path here.
catalog_paths = [
    b.getPath()[len(site_path) + 1 :] for b in brains if b.getPath() != site_path
]
result = {
    "objects": observed,
    "extra": extra,
    "duplicate_uids": {uid: paths for uid, paths in uids.items() if len(paths) > 1},
    "duplicate_history_ids": {
        str(hid): paths for hid, paths in history_ids.items() if len(paths) > 1
    },
    "tokens": tokens,
    "content_objects": content,
    "catalog_entries": len(catalog_paths),
    "catalog_duplicates": len(catalog_paths) - len(set(catalog_paths)),
    # Catalog entries without an object, and objects without an entry.
    "catalog_orphans": sorted(set(catalog_paths) - walked)[:50],
    "uncatalogued": sorted(walked - set(catalog_paths))[:50],
}
with open(out_path, "w") as file:
    json.dump(result, file)
print(
    f"observe: {len(observed)} paths, {len(extra)} new objects, "
    f"{content} content objects, {len(catalog_paths)} catalog entries"
)
transaction.abort()
app._p_jar.db().close()
