"""Check the security index of zoe.plone against the indexer of Plone.

    run.py ZOPE_CONF security_check.py OUT_DIR FOLDER MEMBER OTHER

Run it on a copy of a fixture: it commits. FOLDER is the path of a folder
below the site, MEMBER and OTHER are two user ids. The script:

1. reads the facts of the site: the local role plugins and providers;
2. computes the value of the indexer of Plone for every entry (the value
   that `reindexObjectSecurity` writes), and compares it with the stock
   index;
3. gives the share of entries whose value equals the value of their
   parent, and the shapes of the others;
4. puts a `ZoeSecurityIndex` in place of the stock index;
5. compares the resolved value of every entry with the value of the
   indexer, and the results of four queries;
6. makes four changes of security below FOLDER, then deletes the local
   roles of MEMBER on the whole site, as the deletion of a user does, and
   after each change compares again;
7. writes OUT_DIR/summary.json, and OUT_DIR/entries.tsv for the check of
   the mirror.

Each change also gives what the option "own value or inherit" would write:
the entries whose value equals the value of their parent inherit, the
others keep their value, and an entry that resolves to a wrong value after
the change needs a write.
"""

import collections
import json
import os
import sys
import time

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from borg.localrole.interfaces import ILocalRoleProvider
from Products.PlonePAS.interfaces.plugins import ILocalRolesPlugin
from Products.ZCatalog.query import IndexQuery
from zoe.plone import security
from zope.component import getGlobalSiteManager, getSiteManager
from zope.component.hooks import setSite

out_dir, folder_path, member, other = sys.argv[1:5]
os.makedirs(out_dir, exist_ok=True)
newSecurityManager(None, system)
site = app.Plone
setSite(site)
catalog = site.portal_catalog
inner = catalog._catalog
db = app._p_jar.db()
storage = db.storage
summary = {}


def log(*args):
    print("security_check:", *args, flush=True)


def gc(count):
    if count % 2000 == 0:
        app._p_jar.cacheGC()


# 1. The facts of the site.

providers = sorted(
    {
        f"{r.factory.__module__}.{getattr(r.factory, '__name__', r.factory)}"
        for manager in (getGlobalSiteManager(), getSiteManager(site))
        for r in manager.registeredAdapters()
        if r.provided is ILocalRoleProvider
    }
)
plugins = [pid for pid, _ in site.acl_users.plugins.listPlugins(ILocalRolesPlugin)]
summary["facts"] = {
    "local_role_providers": providers,
    "local_roles_plugins": plugins,
    "site_in_catalog": inner.uids.get("/".join(site.getPhysicalPath())) is not None,
    "entries": len(inner.paths),
}
log("facts", summary["facts"])

paths = dict(inner.paths.items())
uids = {path: rid for rid, path in paths.items()}


def parent_rid(path):
    return uids.get(path.rpartition("/")[0])


def reference():
    """The value of the indexer of Plone for each entry."""
    values = {}
    for count, (rid, path) in enumerate(paths.items()):
        gc(count)
        obj = app.unrestrictedTraverse(path)
        values[rid] = frozenset(security.stock_value(obj))
    return values


# 2. The indexer against the stock index.
started = time.monotonic()
ref = reference()
stock = inner.getIndex(security.INDEX)
stale = [
    rid for rid, value in ref.items() if frozenset(stock._unindex.get(rid, ())) != value
]
summary["stock"] = {
    "seconds_to_compute_every_value": round(time.monotonic() - started, 1),
    "stock_index_differs": len(stale),
}
log("stock", summary["stock"])


def size_of(obj):
    """The records and the bytes of the persistent objects of `obj`."""
    from ZODB.serialize import referencesf

    seen = set()
    todo = [obj._p_oid]
    total = 0
    while todo:
        oid = todo.pop()
        if oid in seen:
            continue
        seen.add(oid)
        data, _tid = storage.load(oid, "")
        total += len(data)
        refs = []
        referencesf(data, refs)
        todo.extend(refs)
    return len(seen), total


from Acquisition import aq_base  # noqa: E402


def index_size(index):
    """Records and bytes below the index, the index itself included."""
    base = aq_base(index)
    records = 0
    total = 0
    for name in ("_index", "_unindex", "_inputs", "_outer", "_opaque"):
        tree = getattr(base, name, None)
        if tree is not None and getattr(tree, "_p_oid", None) is not None:
            r, b = size_of(tree)
            records += r
            total += b
    return records, total


summary["stock"]["records"], summary["stock"]["bytes"] = index_size(stock)

# 3. The share of entries whose value equals the value of their parent.
metadata = {}
for count, rid in enumerate(paths):
    gc(count)
    data = inner.getMetadataForRID(rid)
    metadata[rid] = (data.get("portal_type"), data.get("review_state"))
equal = 0
top = 0
shapes = collections.Counter()
for rid, path in paths.items():
    prid = parent_rid(path)
    if prid is None:
        top += 1
        continue
    if ref[rid] == ref[prid]:
        equal += 1
        continue
    own_type, own_state = metadata[rid]
    parent_type, parent_state = metadata[prid]
    shapes[f"{own_type} {own_state} in {parent_type} {parent_state}"] += 1
values = collections.Counter(ref.values())
summary["corpus"] = {
    "entries": len(ref),
    "below_a_container_outside_the_catalog": top,
    "equal_to_parent": equal,
    "share_equal": round(equal / max(1, len(ref) - top), 4),
    "shapes_of_the_others": dict(shapes.most_common()),
    "distinct_values": len(values),
}
log("corpus", json.dumps(summary["corpus"]))


def commit_size(tid):
    records = 0
    total = 0
    for txn in storage.iterator(tid, tid):
        for record in txn:
            records += 1
            total += len(record.data or b"")
    return records, total


# 4. The index of zoe.plone in place of the stock index.
started = time.monotonic()
security.replace_index(catalog)
transaction.commit()
index = security.security_index(catalog)
inputs = collections.Counter()
for found in index._inputs.values():
    kind, _roles, local, block = found
    inputs[f"kind {kind or 'missing'}"] += 1
    inputs[f"local roles {len(local)}"] += 1
    if block:
        inputs["block"] += 1
summary["zoe_index"] = {
    "seconds_to_build": round(time.monotonic() - started, 1),
    "inputs": dict(sorted(inputs.items())),
    "distinct_inputs": len(set(index._inputs.values())),
    "outer": {path: list(map(list, v[:2])) for path, v in index._outer.items()},
    "opaque": len(index._opaque),
}
summary["zoe_index"]["records"], summary["zoe_index"]["bytes"] = index_size(index)
log("zoe index", json.dumps(summary["zoe_index"]))

security.install()


def compare(label):
    resolver = index.resolver()
    started = time.monotonic()
    resolved = {rid: resolver.value(rid) for rid in paths}
    seconds = time.monotonic() - started
    wrong = [rid for rid in paths if resolved[rid] != ref[rid]]
    for rid in wrong[:5]:
        log(label, "differs", paths[rid], sorted(ref[rid]), sorted(resolved[rid]))
    return {"differ": len(wrong), "seconds_to_resolve_every_entry": round(seconds, 3)}


def queries():
    """Compare four queries with the values of the indexer."""
    acl = site.acl_users
    sets = {"anonymous": ["Anonymous"]}
    for name in (member, other):
        user = acl.getUserById(name)
        sets[name] = catalog._listAllowedRolesAndUsers(user)
    sets["editor of section 00"] = [
        "Anonymous",
        "Authenticated",
        "Member",
        "user:section-00-editors",
    ]
    results = {}
    for name, tokens in sets.items():
        record = IndexQuery(
            {security.INDEX: tokens}, security.INDEX, (), ("or", "and"), "or"
        )
        started = time.monotonic()
        found = set(index.query_index(record))
        seconds = time.monotonic() - started
        wanted = {rid for rid, value in ref.items() if not value.isdisjoint(tokens)}
        results[name] = {
            "matches": len(wanted),
            "differ": len(found ^ wanted),
            "seconds": round(seconds, 3),
        }
    return results


summary["check"] = {"initial": compare("initial")}
summary["queries"] = queries()
log("initial", summary["check"]["initial"], summary["queries"])


def inherit_cost(before, after, changed_path):
    """The writes of the option "own value or inherit" for a change."""
    inherits = {}
    for rid, path in paths.items():
        prid = parent_rid(path)
        inherits[rid] = prid is not None and before[rid] == before[prid]
    memo = {}

    def resolved(rid):
        if rid in memo:
            return memo[rid]
        path = paths[rid]
        if path == changed_path:
            value = after[rid]
        elif inherits[rid]:
            value = resolved(parent_rid(path))
        else:
            value = before[rid]
        memo[rid] = value
        return value

    sys.setrecursionlimit(10000)
    return sum(
        1
        for rid, path in paths.items()
        if path != changed_path and resolved(rid) != after[rid]
    )


def change(label, path, action):
    global ref
    before = ref
    obj = app.unrestrictedTraverse(path)
    started = time.monotonic()
    action(obj)
    obj.reindexObjectSecurity()
    transaction.commit()
    seconds = time.monotonic() - started
    records, total = commit_size(storage.lastTransaction())
    ref = reference()
    changed = sum(1 for rid in paths if before[rid] != ref[rid])
    result = {
        "path": path,
        "seconds": round(seconds, 3),
        "records": records,
        "bytes": total,
        "values_changed": changed,
        "inherit_option_writes": 1 + inherit_cost(before, ref, path),
    }
    result.update(compare(label))
    summary["check"][label] = result
    log(label, result)


folder = site.unrestrictedTraverse(folder_path)
site_path = "/".join(site.getPhysicalPath())
full = f"{site_path}/{folder_path}"
leaves = [
    path
    for rid, path in paths.items()
    if path.startswith(full + "/")
    and path.count("/") == full.count("/") + 2
    and metadata[rid][0] == "Folder"
]
leaf = sorted(leaves)[len(leaves) // 2]
private_docs = sorted(
    paths[rid]
    for rid in paths
    if paths[rid].startswith(full + "/") and metadata[rid] == ("Document", "private")
)
private_doc = private_docs[len(private_docs) // 2]

change("sharing", full, lambda obj: obj.manage_setLocalRoles(member, ["Reader"]))
change(
    "transition",
    full,
    lambda obj: site.portal_workflow.doActionFor(obj, "retract"),
)
change("block", leaf, lambda obj: setattr(obj, "__ac_local_roles_block__", True))
change(
    "document local role",
    private_doc,
    lambda obj: obj.manage_setLocalRoles(other, ["Reader"]),
)
# A user deletion of the control panel: the local roles of MEMBER go,
# on every object of the site.
before_delete = ref
started = time.monotonic()
site.portal_membership.deleteLocalRoles(site, [member], reindex=1, recursive=1)
transaction.commit()
seconds = time.monotonic() - started
records, total = commit_size(storage.lastTransaction())
ref = reference()
result = {
    "seconds": round(seconds, 3),
    "records": records,
    "bytes": total,
    "values_changed": sum(1 for rid in paths if before_delete[rid] != ref[rid]),
}
result.update(compare("delete local roles"))
summary["check"]["delete local roles"] = result
log("delete local roles", result)
summary["queries_after"] = queries()
log("queries after", summary["queries_after"])

# The entries for the check of the mirror.
with open(os.path.join(out_dir, "entries.tsv"), "w") as file:

    def text(found):
        kind, roles, local, block = found
        pairs = ";".join(f"{p}={'|'.join(r)}" for p, r in local)
        return f"{kind or '-'}\t{','.join(roles)}\t{pairs}\t{int(block)}"

    for path, found in index._outer.items():
        file.write(f"O\t{path}\t{text(found)}\t\n")
    for rid, path in paths.items():
        found = index._inputs[rid]
        file.write(f"E\t{path}\t{text(found)}\t{','.join(sorted(ref[rid]))}\n")

with open(os.path.join(out_dir, "summary.json"), "w") as file:
    json.dump(summary, file, indent=1, sort_keys=True)
log("done")
transaction.abort()
app._p_jar.db().close()
