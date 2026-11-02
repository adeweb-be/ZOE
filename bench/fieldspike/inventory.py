"""The inventory of the catalog for the spike on field-addressable objects.

Run inside Zope with plonebench/inzope/run.py, on a read-only copy of a
store: python run.py ZOPE_CONF .../inventory.py OUT.json

For each index and each metadata column of the catalog, and for one
object of each stock type, it records where the value of the catalog
comes from: an indexer adapter, a method of the object, a stored
attribute, or an acquired attribute; and which stored attribute of the
object holds an equal value. It also records the merge policy and the
fields of the schemas of each type.
"""

import json
import sys

from Acquisition import aq_base
from plone.dexterity.utils import iterSchemataForType
from plone.indexer.interfaces import IIndexableObject, IIndexer
from zope.component import queryMultiAdapter
from zope.component.hooks import setSite
from zope.schema import getFieldsInOrder

out_path = sys.argv[1]
site = [o for o in app.objectValues() if o.meta_type == "Plone Site"][0]  # noqa: F821
setSite(site)
catalog = site.portal_catalog
zcat = catalog._catalog


def describe(value):
    name = type(value).__name__
    if isinstance(value, (tuple, list)):
        inner = sorted({type(v).__name__ for v in value})
        return f"{name}[{','.join(inner)}]"
    return name


def stored_match(obj, value):
    """The stored attributes of obj whose value equals value."""
    found = []
    for key, stored in aq_base(obj).__dict__.items():
        if key.startswith("_p_") or key.startswith("_v_"):
            continue
        try:
            if stored == value and type(stored) is type(value):
                found.append(key)
        except Exception:
            pass
    return found


def source(obj, wrapper, name):
    indexer = queryMultiAdapter((obj, catalog), IIndexer, name=name)
    record = {}
    if indexer is not None:
        func = getattr(indexer, "callable", None)
        func = getattr(func, "__wrapped__", func)
        record["kind"] = "indexer"
        record["indexer"] = (
            f"{getattr(func, '__module__', '?')}.{getattr(func, '__name__', '?')}"
        )
    elif name in aq_base(obj).__dict__:
        record["kind"] = "stored"
    else:
        klass = type(aq_base(obj))
        attr = getattr(klass, name, None)
        if attr is not None and callable(attr):
            record["kind"] = "method"
            owner = [k for k in klass.__mro__ if name in k.__dict__]
            record["method"] = (
                f"{owner[0].__module__}.{owner[0].__name__}.{name}" if owner else name
            )
        elif attr is not None:
            record["kind"] = "class attribute"
        else:
            record["kind"] = "acquired or missing"
    try:
        value = getattr(wrapper, name)
        if callable(value):
            value = value()
        record["type"] = describe(value)
        record["stored_equal"] = stored_match(obj, value)
        text = repr(value)
        record["sample"] = text[:80]
    except Exception as error:
        record["error"] = type(error).__name__
    return record


samples = {}
for portal_type in ("Document", "News Item", "Folder", "File", "Collection"):
    brains = catalog.unrestrictedSearchResults(portal_type=portal_type, sort_limit=1)
    if brains:
        samples[portal_type] = brains[0]._unrestrictedGetObject()

indexes = []
for index_id in zcat.indexes.keys():
    index = zcat.getIndex(index_id)
    try:
        attrs = list(index.getIndexSourceNames())
    except Exception:
        attrs = [index_id]
    entry = {"id": index_id, "meta_type": index.meta_type, "attrs": attrs, "types": {}}
    for portal_type, obj in samples.items():
        wrapper = queryMultiAdapter((obj, catalog), IIndexableObject)
        entry["types"][portal_type] = {a: source(obj, wrapper, a) for a in attrs}
    indexes.append(entry)

columns = []
for column in zcat.schema.keys():
    entry = {"id": column, "types": {}}
    for portal_type, obj in samples.items():
        wrapper = queryMultiAdapter((obj, catalog), IIndexableObject)
        entry["types"][portal_type] = source(obj, wrapper, column)
    columns.append(entry)

schemas = {}
for portal_type, obj in samples.items():
    fields = []
    for schema in iterSchemataForType(portal_type):
        for name, field in getFieldsInOrder(schema):
            fields.append(
                {
                    "schema": schema.__identifier__,
                    "name": name,
                    "field": type(field).__name__,
                    "stored": name in aq_base(obj).__dict__,
                }
            )
    schemas[portal_type] = {
        "class": f"{type(obj).__module__}.{type(obj).__qualname__}",
        "state_keys": sorted(
            k for k in aq_base(obj).__dict__ if not k.startswith("_v_")
        ),
        "fields": fields,
    }

policy = None
try:
    import zoe.policy

    policy = zoe.policy.read(site._p_jar)
except Exception as error:
    policy = {"error": repr(error)}

with open(out_path, "w") as f:
    json.dump(
        {"indexes": indexes, "columns": columns, "schemas": schemas, "policy": policy},
        f,
        indent=1,
        default=repr,
    )
print("indexes", len(indexes), "columns", len(columns))
