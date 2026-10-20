"""Write the logical digest of the site, for the equivalence of two stores.

    run.py ZOPE_CONF digest.py OUT_JSON

Read-only: the script never commits, so it frees the object cache itself
as it walks. The digest names each content object with its type, the
hashes of its fields, its workflow state, its UID, its local roles, the
roles that have View on it, its dates, its versions and its workflow
history; the users and groups; the size of the catalog and the results of
a fixed set of queries. Two stores with the same digest hold the same
site, as Plone sees it.
"""

import hashlib
import json
import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from Acquisition import aq_base
from plone import api
from plone.uuid.interfaces import ATTRIBUTE_NAME
from zope.component.hooks import setSite

SITE_ID = "Plone"
# The objects of the walk, and the brains, between two calls of cacheGC.
GC_OBJECTS = 100
GC_BRAINS = 1_000
(out_path,) = sys.argv[1:2]
newSecurityManager(None, system)
site = app[SITE_ID]
setSite(site)
catalog = api.portal.get_tool("portal_catalog")
repository = api.portal.get_tool("portal_repository")
workflow = api.portal.get_tool("portal_workflow")


def sha(value):
    if value is None:
        return None
    if not isinstance(value, bytes):
        value = str(value).encode()
    return hashlib.sha256(value).hexdigest()[:16]


def walk(container, prefix):
    for name, obj in sorted(container.objectItems()):
        path = f"{prefix}/{name}" if prefix else name
        yield path, obj
        if getattr(aq_base(obj), "isPrincipiaFolderish", False):
            yield from walk(obj, path)


objects = {}
for count, (path, obj) in enumerate(walk(site, "")):
    if count % GC_OBJECTS == 0:
        app._p_jar.cacheGC()
    base = aq_base(obj)
    if not hasattr(base, "portal_type") or path.startswith("portal_"):
        continue
    if getattr(base, ATTRIBUTE_NAME, None) is not None:
        text = getattr(base, "text", None)
        blob = getattr(base, "file", None)
        try:
            state = api.content.get_state(obj)
        except Exception:
            state = None
        try:
            versions = len(repository.getHistoryMetadata(obj) or [])
        except Exception:
            versions = 0
        history = getattr(base, "workflow_history", {}) or {}
        objects[path] = {
            "type": base.portal_type,
            "title": sha(obj.Title()),
            "description": sha(obj.Description()),
            "text": sha(text.raw if text is not None else None),
            "file": [blob.getSize(), sha(blob.data)] if blob is not None else None,
            "state": state,
            "uid": api.content.get_uuid(obj),
            "local_roles": sorted(
                (principal, sorted(roles)) for principal, roles in obj.get_local_roles()
            ),
            "view": sorted(
                r["name"] for r in obj.rolesOfPermission("View") if r["selected"]
            ),
            "created": str(obj.created()),
            "modified": str(obj.modified()),
            "versions": versions,
            "workflow_history": sum(len(v) for v in history.values()),
            "subjects": sorted(obj.Subject()),
        }

brains = catalog.unrestrictedSearchResults(path="/" + SITE_ID)


def paths(brains):
    for count, brain in enumerate(brains):
        if count % GC_BRAINS == 0:
            app._p_jar.cacheGC()
        yield brain.getPath()


queries = {
    "documents_published": {"portal_type": "Document", "review_state": "published"},
    "private": {"review_state": "private"},
    "recent": {"sort_on": "modified", "sort_order": "reverse", "sort_limit": 25},
    "titles": {"sort_on": "sortable_title", "sort_limit": 25},
}
results = {}
for name, query in queries.items():
    results[name] = [
        b.getPath()
        for b in catalog.unrestrictedSearchResults(path="/" + SITE_ID, **query)
    ][:200]
users = sorted(u.getId() for u in api.user.get_users())
groups = {
    g.getId(): sorted(m.getId() for m in api.user.get_users(groupname=g.getId()))
    for g in api.group.get_groups()
}
counts = {}
for item in objects.values():
    key = f"{item['type']}:{item['state']}"
    counts[key] = counts.get(key, 0) + 1
digest = {
    "objects": objects,
    "counts": counts,
    "catalog": {
        "size": len(brains),
        "paths": sha("\n".join(sorted(paths(brains)))),
        "queries": results,
    },
    "users": users,
    "groups": groups,
}
with open(out_path, "w") as file:
    json.dump(digest, file, indent=0, sort_keys=True)
print(f"digest: {len(objects)} objects, {len(brains)} catalog entries")
transaction.abort()
app._p_jar.db().close()
