"""Name the structure of the site that holds each oid. Read-only.

    run.py ZOPE_CONF oidmap.py OIDS_JSON OUT_JSON

OIDS_JSON is a list of oids in hex, such as the oids of the conflicts that
the probe recorded. The script walks the catalog of the site (its metadata,
its path maps and each index, down to every bucket and tree set) and a few
tools, and writes for each oid the name of the structure that holds it, or
null when the walk does not reach it.
"""

import json
import sys

import transaction
from AccessControl.SecurityManagement import newSecurityManager
from AccessControl.SpecialUsers import system
from Acquisition import aq_base
from persistent import Persistent
from plone import api
from zope.component.hooks import setSite

SITE_ID = "Plone"
oids_path, out_path = sys.argv[1:3]
with open(oids_path) as file:
    wanted = {bytes.fromhex(oid) for oid in json.load(file)}

newSecurityManager(None, system)
site = app[SITE_ID]
setSite(site)
found = {}
seen = set()


def is_btree_like(obj):
    module = type(obj).__module__ or ""
    return module.startswith("BTrees.")


def walk(obj, label, depth=0):
    """Record the oid of `obj` and of the persistent objects under it."""
    if not isinstance(obj, Persistent):
        return
    oid = obj._p_oid
    if oid is not None:
        if oid in seen:
            return
        seen.add(oid)
        if oid in wanted:
            found[oid.hex()] = f"{label} ({type(obj).__module__}.{type(obj).__name__})"
    if is_btree_like(obj):
        obj._p_activate()
        state = obj.__getstate__()
        children = _children(state)
        for child in children:
            walk(child, label, depth)
        # The values of a mapping can be tree sets of their own.
        if hasattr(obj, "values") and type(obj).__name__.endswith(("BTree", "Bucket")):
            try:
                for value in obj.values():
                    walk(value, label + " [value]", depth)
            except Exception:
                pass
        return
    if depth > 3:
        return
    # A ghost has an empty __dict__ until it loads.
    obj._p_activate()
    for name, value in list(getattr(aq_base(obj), "__dict__", {}).items()):
        if isinstance(value, Persistent):
            walk(value, f"{label}.{name}", depth + 1)


def _children(state):
    """The persistent objects in the state of a BTree object."""
    result = []
    stack = [state]
    while stack:
        item = stack.pop()
        if isinstance(item, Persistent):
            result.append(item)
        elif isinstance(item, (tuple, list)):
            stack.extend(item)
    return result


catalog = api.portal.get_tool("portal_catalog")
inner = aq_base(catalog)._catalog
walk(inner.data, "catalog metadata (rid -> metadata tuple)")
walk(inner.uids, "catalog uids (path -> rid)")
walk(inner.paths, "catalog paths (rid -> path)")
walk(getattr(inner, "_length", None), "catalog length")
for name in inner.indexes.keys():
    index = aq_base(inner.getIndex(name))
    walk(index, f"index {name}")
for name in ("plone_lexicon",):
    lexicon = getattr(aq_base(catalog), name, None)
    if lexicon is not None:
        walk(aq_base(lexicon), f"lexicon {name}")
for tool in (
    "portal_uidgenerator",
    "portal_historiesstorage",
    "portal_historyidhandler",
):
    obj = getattr(aq_base(site), tool, None)
    if obj is not None:
        walk(aq_base(obj), tool)

result = {oid.hex(): found.get(oid.hex()) for oid in wanted}
with open(out_path, "w") as file:
    json.dump(result, file, indent=1, sort_keys=True)
named = sum(1 for v in result.values() if v)
print(f"oidmap: {named} of {len(result)} oids named, {len(seen)} objects walked")
transaction.abort()
app._p_jar.db().close()
