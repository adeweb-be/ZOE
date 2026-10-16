"""Build the synthetic site of a spec in the storage of the stack.

    run.py ZOPE_CONF build_site.py SPEC_JSON CREDENTIALS_JSON MANIFEST_JSON

SPEC_JSON holds a `SiteSpec`. CREDENTIALS_JSON holds the password of the
accounts of the benchmark (`{"password": ...}`); it never goes on a command
line. The script adds the Plone site `Plone`, its users and groups, and the
items of the plan, with their workflow states, local roles, files and
collections. It commits every BATCH items. It writes MANIFEST_JSON: the
plan without the text, with the UID, the state and the versions of each
item after the build, and the accounts.

The UIDs and the dates are a function of the spec, so two builds of one
spec give the same content, the same ids and the same catalog results.
"""

import hashlib
import json
import sys
import time

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from DateTime import DateTime
from plone import api
from plone.app.textfield.value import RichTextValue
from plone.namedfile.file import NamedBlobFile
from plone.uuid.interfaces import ATTRIBUTE_NAME
from Products.CMFPlone.factory import addPloneSite
from zope.component.hooks import setSite

from plonebench import corpus

SITE_ID = "Plone"
BATCH = 50
# The dates of the items start here and step by one minute.
EPOCH = DateTime("2024/01/01 08:00:00 UTC")

spec_path, credentials_path, manifest_path = sys.argv[1:4]
with open(spec_path) as file:
    spec = corpus.SiteSpec.from_json(json.load(file))
with open(credentials_path) as file:
    password = json.load(file)["password"]

plan = corpus.plan_site(spec)
started = time.time()
newSecurityManager(None, system)
if SITE_ID in app.objectIds():
    sys.exit("build_site: the storage already holds a site; build into a new store")
site = addPloneSite(
    app,
    SITE_ID,
    title="Synthetic benchmark site",
    extension_ids=(
        "plone.app.contenttypes:default",
        "plonetheme.barceloneta:default",
        "plone.restapi:default",
    ),
    setup_content=False,
)
setSite(site)
transaction.commit()
print(f"build_site: site added in {time.time() - started:.1f} s", flush=True)


def uid_of(path):
    return hashlib.sha256(f"{spec.seed}:uid:{path}".encode()).hexdigest()[:32]


# Users and groups.
users = corpus.user_ids(spec)
for ids in users.values():
    for userid in ids:
        api.user.create(
            email=f"{userid}@example.invalid",
            username=userid,
            password=password,
            properties={"fullname": userid},
        )
api.user.grant_roles(username=corpus.MAINTAINER, roles=["Manager"])
for s in range(spec.sections):
    api.group.create(groupname=f"section-{s:02d}-editors")
api.group.create(groupname="bench-writers")
for index, userid in enumerate(users[corpus.EDITOR]):
    section = corpus.editor_section(spec, index)
    api.group.add_user(groupname=f"section-{section:02d}-editors", username=userid)
    api.group.add_user(groupname="bench-writers", username=userid)
transaction.commit()
print(f"build_site: {sum(len(v) for v in users.values())} users", flush=True)

ROLES = ["Contributor", "Editor", "Reviewer"]
TRANSITIONS = {
    corpus.PUBLISHED: "publish",
    corpus.PENDING: "submit",
}


def create(item, index):
    parent_path, _, name = item.path.rpartition("/")
    container = site.unrestrictedTraverse(parent_path) if parent_path else site
    fields = {"title": item.title, "description": item.description}
    if item.portal_type in ("Document", "News Item"):
        fields["text"] = RichTextValue(item.text, "text/html", "text/x-html-safe")
        fields["subject"] = list(item.subjects)
    if item.portal_type == "File":
        fields["file"] = NamedBlobFile(
            data=corpus.file_bytes(spec, item.path, item.size),
            filename=f"{name}.bin",
            contentType="application/octet-stream",
        )
    if item.portal_type == "Collection":
        fields["query"] = [
            {
                "i": "portal_type",
                "o": "plone.app.querystring.operation.selection.any",
                "v": ["Document", "News Item"],
            },
            {
                "i": "path",
                "o": "plone.app.querystring.operation.string.relativePath",
                "v": "..",
            },
        ]
        fields["sort_on"] = "modified"
        fields["sort_reversed"] = True
        fields["item_count"] = 20
    container.invokeFactory(item.portal_type, name, **fields)
    obj = container[name]
    setattr(obj, ATTRIBUTE_NAME, uid_of(item.path))
    if item.section >= 0:
        editors = users[corpus.EDITOR]
        owner = editors[item.section % len(editors)]
        obj.setCreators([owner])
    if item.portal_type == "Folder" and "/" not in item.path:
        if item.section >= 0:
            obj.manage_setLocalRoles(f"section-{item.section:02d}-editors", ROLES)
        else:
            obj.manage_setLocalRoles("bench-writers", ROLES)
    if item.path == f"{corpus.BENCH}/{corpus.IMPORT}":
        obj.manage_setLocalRoles(corpus.IMPORTER, ROLES)
    state = (
        item.state
        if item.portal_type in ("Document", "News Item")
        else corpus.PUBLISHED
    )
    workflow = api.portal.get_tool("portal_workflow")
    if workflow.getChainFor(obj):
        transition = TRANSITIONS.get(state)
        if transition:
            api.content.transition(obj=obj, transition=transition)
    date = EPOCH + index / 1440.0
    obj.creation_date = date
    obj.setModificationDate(date)
    obj.reindexObject()


for index, item in enumerate(plan.items):
    create(item, index)
    if index % BATCH == BATCH - 1:
        transaction.commit()
        print(f"build_site: {index + 1}/{len(plan.items)} items", flush=True)
transaction.commit()

# The manifest: what Locust and the verification need, without the text.
catalog = api.portal.get_tool("portal_catalog")
repository = api.portal.get_tool("portal_repository")
entries = []
for item in plan.items:
    obj = site.unrestrictedTraverse(item.path)
    try:
        versions = len(repository.getHistoryMetadata(obj) or [])
    except Exception:
        versions = 0
    try:
        state = api.content.get_state(obj)
    except Exception:
        state = None
    entries.append(
        {
            "path": item.path,
            "portal_type": item.portal_type,
            "title": item.title,
            "state": state,
            "pool": item.pool,
            "section": item.section,
            "size": item.size,
            "uid": api.content.get_uuid(obj),
            "versions": versions,
            "workflow_history": sum(
                len(v) for v in (getattr(obj, "workflow_history", {}) or {}).values()
            ),
            "description": obj.Description(),
        }
    )
manifest = {
    "spec": spec.to_json(),
    "site": SITE_ID,
    "items": entries,
    "users": users,
    "search_terms": corpus.search_terms(plan),
    "subjects": plan.words[50:90],
    "catalog_size": len(catalog.unrestrictedSearchResults(path="/" + SITE_ID)),
    "build_seconds": round(time.time() - started, 1),
}
with open(manifest_path, "w") as file:
    json.dump(manifest, file, indent=1)
print(
    f"build_site: {len(entries)} items, {manifest['catalog_size']} catalog entries, "
    f"{manifest['build_seconds']} s",
    flush=True,
)
transaction.abort()
app._p_jar.db().close()
