"""The merge policy of a Plone site, from its Dexterity types (ADR 0046,
section 9).

`derive` gives an entry of the merge policy for the class of each
Dexterity type:

- the class merges field by field (tier 2), in the mode `DEFAULT_MODE`;
- the fields that an `@invariant` of the schema of a type, or of one of its
  behaviours, reads are one coupled group: when both sides change the
  group, they must agree on each of its fields; a field that the factory
  of its behaviour stores under another name (`effective` as
  `effective_date`) takes both names;
- a class with an invariant whose fields `derive` cannot find gets no
  entry, so it never merges in tier 2;
- `modification_date` takes `max`: each save sets it, so two saves of one
  object both change it; every other field takes `strict`.

A class that several types use takes the groups of all of them. The policy
also holds the entry of `zoe.plone.history.WorkflowHistory`.

The import step `import_merge_policy` runs for each profile that holds the
file `zoe-merge-policy.json`, the overrides. It derives the policy, applies
the overrides and writes the policy with `zoe.policy.install`. The file of
the profile of `zoe.plone` holds no override. The overrides are a JSON
object with these keys, each optional:

- ``tier2``: the mode of the derived entries, ``shadow`` by default, as a
  class merges only after shadow mode showed that its merges are right;
- ``structural_merges`` and ``posting_dicts``: the switches of tier 1;
- ``classes``: entries by class name; an entry changes the keys that it
  gives, and its ``fields`` add to the derived ones; ``null`` removes the
  entry of a class.

Only `derive` and `import_merge_policy` import Plone.
"""

import json
import logging
import types

from zoe import policy as merge_policy
from zoe.plone import history

__all__ = [
    "DEFAULT_MODE",
    "FIELDS",
    "OVERRIDES",
    "apply_overrides",
    "class_entries",
    "derive",
    "import_merge_policy",
    "invariant_fields",
]

logger = logging.getLogger("zoe.plone")

#: The file of the overrides in a profile.
OVERRIDES = "zoe-merge-policy.json"
#: The mode of the derived entries, unless the overrides give another.
DEFAULT_MODE = "shadow"
#: The strategies of the derived entries. The other fields are strict.
FIELDS = {"modification_date": "max"}


def invariant_fields(invariant, names):
    """Return the fields of `names` that the code of `invariant` reads, as
    attributes or as names, also in the functions that it defines."""
    code = getattr(invariant, "__code__", None)
    found = set()
    todo = [code] if code is not None else []
    while todo:
        code = todo.pop()
        found.update(name for name in code.co_names if name in names)
        found.update(
            const
            for const in code.co_consts
            if isinstance(const, str) and const in names
        )
        todo.extend(
            const for const in code.co_consts if isinstance(const, types.CodeType)
        )
    return found


def class_entries(typed, mode=DEFAULT_MODE):
    """Return the entry of each class of `typed`, an iterable of
    `(class name, field names, invariants, stored names)`, one item for each
    type: the names of the fields of the schemas of the type, the
    invariants of those schemas, and the other name under which a field is
    stored, by field. A class with an invariant whose fields are not found
    gets no entry."""
    groups = {}
    blocked = set()
    for name, fields, invariants, stored in typed:
        groups.setdefault(name, [])
        for invariant in invariants:
            found = invariant_fields(invariant, set(fields))
            if not found:
                blocked.add(name)
                continue
            found |= {stored[field] for field in found if field in stored}
            group = sorted(found)
            if group not in groups[name]:
                groups[name].append(group)
    entries = {}
    for name, coupled in groups.items():
        if name in blocked:
            continue
        entry = {"tier2": mode, "default": "strict", "fields": dict(FIELDS)}
        if coupled:
            entry["coupled"] = coupled
        entries[name] = entry
    return entries


def apply_overrides(derived, overrides):
    """Return the policy `derived` with `overrides` applied (see the module
    docstring). `derived` and `overrides` stay as they are."""
    if not isinstance(overrides, dict):
        raise ValueError("the overrides are a JSON object")
    unknown = set(overrides) - {
        "tier2",
        "structural_merges",
        "posting_dicts",
        "classes",
    }
    if unknown:
        raise ValueError(f"the overrides have no key {sorted(unknown)[0]!r}")
    result = {
        "classes": {name: dict(entry) for name, entry in derived["classes"].items()}
    }
    for switch in ("structural_merges", "posting_dicts"):
        if overrides.get(switch) is not None:
            result[switch] = overrides[switch]
    for name, entry in (overrides.get("classes") or {}).items():
        if entry is None:
            result["classes"].pop(name, None)
            continue
        merged = dict(result["classes"].get(name, {}))
        fields = {**merged.get("fields", {}), **entry.get("fields", {})}
        merged.update({key: value for key, value in entry.items() if key != "fields"})
        if fields:
            merged["fields"] = fields
        result["classes"][name] = merged
    return merge_policy.validate(result)


def derive(portal, overrides=None):
    """Return the merge policy of the Dexterity types of `portal`, with
    `overrides` applied."""
    from plone.behavior.registration import lookup_behavior_registration
    from plone.dexterity.interfaces import IDexterityFTI
    from plone.dexterity.utils import iterSchemataForType
    from Products.CMFCore.utils import getToolByName
    from zope.schema import getFieldNames

    overrides = overrides or {}
    mode = overrides.get("tier2") or DEFAULT_MODE
    typed = []
    for fti in getToolByName(portal, "portal_types").objectValues():
        if not IDexterityFTI.providedBy(fti):
            continue
        fields = set()
        invariants = []
        for schema in iterSchemataForType(fti.getId()):
            fields.update(getFieldNames(schema))
            for iface in schema.__iro__:
                for invariant in iface.queryDirectTaggedValue("invariants", ()):
                    if invariant not in invariants:
                        invariants.append(invariant)
        # A factory of a behaviour can store a field under another name, as
        # the `DCFieldProperty` of plone.app.dexterity does.
        stored = {}
        for behavior in fti.behaviors:
            registration = lookup_behavior_registration(
                name=behavior, warn_about_fallback=False
            )
            factory = getattr(registration, "factory", None)
            for field in getFieldNames(registration.interface):
                other = getattr(getattr(factory, field, None), "_get_name", None)
                if isinstance(other, str) and other != field:
                    stored[field] = other
        typed.append((fti.klass, fields, invariants, stored))
    derived = {"classes": class_entries(typed, mode)}
    derived["classes"].update(history.policy()["classes"])
    return apply_overrides(derived, overrides)


def import_merge_policy(context):
    """The import step: when the profile holds `OVERRIDES`, derive the merge
    policy of the site, apply the overrides of the file and write the policy
    into the database of the site."""
    data = context.readDataFile(OVERRIDES)
    if data is None:
        return
    overrides = json.loads(data)
    portal = context.getSite()
    policy = derive(portal, overrides)
    version = merge_policy.install(portal._p_jar, policy)
    logger.info(
        "the merge policy of the site is version %d, with %d classes",
        version,
        len(policy["classes"]),
    )
