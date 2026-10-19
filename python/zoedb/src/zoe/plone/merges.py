"""The merge policy of a Plone site, from its Dexterity types (ADR 0046,
section 9).

`derive` gives an entry of the merge policy for the class of each
Dexterity type, by the name that its records give it
(`__module__.__qualname__`):

- the class merges field by field (tier 2), in the mode `DEFAULT_MODE`;
- an `@invariant` of the schema of a type, or of one of its behaviours,
  makes one coupled group: every field of the interface that declares the
  invariant, the fields that the code of the invariant reads, and the other
  names under which the factory of a behaviour stores them (`effective` as
  `effective_date`); when both sides change the group, they must agree on
  each of its fields;
- a class with an invariant whose group is empty is blocked: it never
  merges in tier 2;
- `modification_date` takes `max`: each save sets it, so two saves of one
  object both change it; every other field takes `strict`.

A class that several types use takes the groups of all of them. The policy
also holds the entry of `zoe.plone.history.WorkflowHistory`.

The import step `import_merge_policy` runs for each profile that holds the
file `zoe-merge-policy.json`, the overrides of that profile. The site keeps
the overrides of each profile, by profile id, so the import of one profile
keeps those of the others. `update_policy` derives the entries, applies
the overrides of each profile in turn, composes the result with the policy
in the database, and writes it with `zoe.policy.install`. The composition
keeps the classes of other sites of the database: the coupled groups of a
class are the union of both, its fields take both, a blocked class stays
blocked, and the other keys take the new value. The switch
``folder_orders``, the one switch that a profile sets, is the exception:
the database keeps the latest mode that each site asked for, and the
policy takes the lowest of them (``off``, then ``shadow``, then ``on``),
so an ``off`` of one site lasts when another site imports the profile,
and a site that raises its own mode raises the policy when no other site
asks for less (`lowest_mode`). A mode for the whole database goes through
the overrides of a site: an ``off`` that `zoe.policy.install` writes lasts
only until the next `update_policy` of a site that asks for more, which
replaces it with a warning; a rise of the lowest mode of the sites logs at
INFO (`log_rise`). `update_policy` also drops the mode of a site
that is gone (`gone_sites`): a removed site, or the old path of a renamed
or moved site, which gives its mode under its new path at its own next
update. A change of a Dexterity type derives the policy again
(`fti_modified`).

The overrides are a JSON object with these keys, each optional:

- ``tier2``: the mode of the derived entries, ``shadow`` by default, as a
  class merges only after shadow mode showed that its merges are right;
- ``structural_merges``, ``posting_dicts``, ``split_merges`` and
  ``folder_orders``: the switches of tier 1. The overrides of the profile
  ``zoe.plone:default`` set ``folder_orders`` to ``on`` (the order merge of
  ADR 0052); they set no other switch, so the split merge of ADR 0048
  stays off until an override or `zoe.policy.install` sets it. The profile
  alone merges no order: its derived entries stay in shadow mode, the
  record of a folder conflicts before its order, and ``split_merges`` is
  off. A site gets the order merge once its overrides switch on the entries
  of its container classes, and ``split_merges`` where the positions of a
  folder split. An override of ``folder_orders`` in a later profile of a
  site replaces the mode of that site; the policy of the database takes the
  lowest mode of its sites;
- ``classes``: entries by class name; an entry changes the keys that it
  gives, its ``fields`` add to the derived ones, and its ``coupled`` groups
  add to the derived groups; ``null`` removes the entry of a class. An
  override does not unblock a blocked class, through one profile or
  several: it can neither remove it, nor switch it on, nor give it
  ``"blocked": false``; the class stays blocked, and the import logs a
  warning.

Only `derive`, `site_overrides` and `gone_sites` (Acquisition),
`update_policy`, `profile_of`, `fti_modified` (zope.component.hooks) and
the import step import Plone or Zope.
"""

import json
import logging
import os
import types

from zoe import policy as merge_policy
from zoe.plone import history

__all__ = [
    "DEFAULT_MODE",
    "FIELDS",
    "OVERRIDES",
    "apply_overrides",
    "class_entries",
    "compose",
    "derive",
    "fti_modified",
    "import_merge_policy",
    "invariant_fields",
    "lowest_mode",
    "settle",
    "update_policy",
]

logger = logging.getLogger("zoe.plone")

#: The file of the overrides in a profile.
OVERRIDES = "zoe-merge-policy.json"
#: The mode of the derived entries, unless the overrides give another.
DEFAULT_MODE = "shadow"
#: The strategies of the derived entries. The other fields are strict.
FIELDS = {"modification_date": "max"}
#: The modes of a switch, from the lowest.
LOWER = ("off", "shadow", "on")
#: The switch whose mode the sites of a database compose to the lowest.
SITE_SWITCH = "folder_orders"
#: The name of the root of the database that keeps, for `SITE_SWITCH`, the
#: latest mode that each site asked for, by the path of the site.
SITE_MODES = "zoe.plone.folder_orders"
#: The attribute of the site that keeps the overrides of each profile.
SITE_OVERRIDES = "_zoe_merge_overrides"


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


def blocked_entry():
    """Return the entry of a class that never merges in tier 2."""
    return {"tier2": "off", "blocked": True}


def class_entries(typed, mode=DEFAULT_MODE):
    """Return the entry of each class of `typed`, an iterable of
    `(class name, field names, invariants, stored names)`, one item for each
    type: the names of the fields of the schemas of the type; the
    invariants of those schemas, each with the field names of the interface
    that declares it; and the other name under which a field is stored, by
    field."""
    groups = {}
    blocked = set()
    for name, fields, invariants, stored in typed:
        groups.setdefault(name, [])
        for invariant, declared in invariants:
            found = set(declared) | invariant_fields(invariant, set(fields))
            found |= {stored[field] for field in found if field in stored}
            if not found:
                blocked.add(name)
                continue
            group = sorted(found)
            if group not in groups[name]:
                groups[name].append(group)
    entries = {}
    for name, coupled in groups.items():
        if name in blocked:
            entries[name] = blocked_entry()
            continue
        entry = {"tier2": mode, "default": "strict", "fields": dict(FIELDS)}
        if coupled:
            entry["coupled"] = coupled
        entries[name] = entry
    return entries


def _groups(*lists):
    """Return the union of lists of coupled groups, each group once."""
    result = []
    for groups in lists:
        for group in groups or ():
            group = sorted(group)
            if group not in result:
                result.append(group)
    return result


def apply_overrides(policy, overrides, derived=frozenset(), blocked=frozenset()):
    """Return the policy `policy` with `overrides` applied (see the module
    docstring), and the names of the classes that the overrides remove.
    The key ``tier2`` of the overrides sets the mode of the classes of
    `derived`. A class of `blocked`, or one whose entry is blocked, stays
    blocked: an override can neither remove it, nor switch it on, nor give
    it ``"blocked": false``. `policy` and `overrides` stay as they are.
    Raise `ValueError` for overrides of another form."""
    if type(overrides) is not dict:
        raise ValueError("the overrides are a JSON object")
    unknown = set(overrides) - {"tier2", "classes", *merge_policy.SWITCHES}
    if unknown:
        raise ValueError(f"the overrides have no key {sorted(map(str, unknown))[0]!r}")
    classes = overrides.get("classes", {})
    if classes is None:
        classes = {}
    if type(classes) is not dict:
        raise ValueError("the classes of the overrides are a JSON object")
    result = {key: value for key, value in policy.items() if key != "classes"}
    result["classes"] = {name: dict(entry) for name, entry in policy["classes"].items()}
    for switch in merge_policy.SWITCHES:
        if overrides.get(switch) is not None:
            result[switch] = overrides[switch]
    mode = overrides.get("tier2")
    if mode is not None:
        for name in derived:
            entry = result["classes"].get(name)
            if entry is not None and not entry.get("blocked"):
                entry["tier2"] = mode
    removed = set()
    for name, entry in classes.items():
        if entry is not None and type(entry) is not dict:
            raise ValueError(f"{name}: an override is a JSON object or null")
        for key, kind in (("fields", dict), ("coupled", list)):
            if entry is not None and key in entry and type(entry[key]) is not kind:
                raise ValueError(f"{name}: {key} of an override is of another form")
        merged = dict(result["classes"].get(name, {}))
        if name in blocked or merged.get("blocked"):
            # A blocked class stays blocked: the override loses its `tier2`
            # and its `blocked`.
            unblocks = entry is None or (
                entry.get("tier2") not in (None, "off") or entry.get("blocked") is False
            )
            if unblocks:
                logger.warning("%s stays blocked: an override cannot unblock it", name)
            merged.update(blocked_entry())
            entry = {
                key: value
                for key, value in (entry or {}).items()
                if key not in ("tier2", "blocked")
            }
        elif entry is None:
            result["classes"].pop(name, None)
            removed.add(name)
            continue
        removed.discard(name)
        fields = {**merged.get("fields", {}), **entry.get("fields", {})}
        coupled = _groups(merged.get("coupled"), entry.get("coupled"))
        merged.update(
            {
                key: value
                for key, value in entry.items()
                if key not in ("fields", "coupled")
            }
        )
        if fields:
            merged["fields"] = fields
        if coupled:
            merged["coupled"] = coupled
        result["classes"][name] = merged
    return merge_policy.validate(result), removed


def compose(stored, new):
    """Return the policy `new` composed with the policy `stored` of the
    database (see the module docstring). `stored` can be None."""
    if stored is None:
        return merge_policy.validate(new)
    result = {"classes": {}}
    for switch in merge_policy.SWITCHES:
        value = new.get(switch, stored.get(switch))
        if value is not None:
            result[switch] = value
    names = list(stored["classes"])
    names += [name for name in new["classes"] if name not in stored["classes"]]
    for name in names:
        old = stored["classes"].get(name)
        entry = new["classes"].get(name)
        if old is None or entry is None:
            result["classes"][name] = dict(old if entry is None else entry)
            continue
        merged = dict(old)
        merged.update(
            {
                key: value
                for key, value in entry.items()
                if key not in ("fields", "coupled")
            }
        )
        fields = {**old.get("fields", {}), **entry.get("fields", {})}
        if fields:
            merged["fields"] = fields
        coupled = _groups(old.get("coupled"), entry.get("coupled"))
        if coupled:
            merged["coupled"] = coupled
        if old.get("blocked") or entry.get("blocked"):
            merged.update(blocked_entry())
        result["classes"][name] = merged
    return merge_policy.validate(result)


def settle(stored, policy, removed):
    """Return `policy` composed with the policy `stored` of the database,
    without the classes of `removed`. A class that the database blocks,
    for another site maybe, stays blocked: an override of this site
    cannot remove it."""
    composed = compose(stored, policy)
    for name in removed:
        if composed["classes"].get(name, {}).get("blocked"):
            logger.warning("%s stays blocked: an override cannot unblock it", name)
            continue
        composed["classes"].pop(name, None)
    return composed


def derive(portal):
    """Return the merge policy of the Dexterity types of `portal`, without
    overrides: `{"classes": {...}}`."""
    from plone.behavior.registration import (
        BehaviorRegistrationNotFound,
        lookup_behavior_registration,
    )
    from plone.dexterity.interfaces import IDexterityFTI
    from plone.dexterity.utils import iterSchemataForType, resolveDottedName
    from Products.CMFCore.utils import getToolByName
    from zope.schema import getFieldNames

    typed = []
    for fti in getToolByName(portal, "portal_types").objectValues():
        if not IDexterityFTI.providedBy(fti):
            continue
        try:
            klass = resolveDottedName(fti.klass)
        except (ImportError, AttributeError, ValueError):
            logger.warning("%s: the class %r does not import", fti.getId(), fti.klass)
            continue
        name = f"{klass.__module__}.{klass.__qualname__}"
        fields = set()
        invariants = []
        for schema in iterSchemataForType(fti.getId()):
            fields.update(getFieldNames(schema))
            for iface in schema.__iro__:
                for invariant in iface.queryDirectTaggedValue("invariants", ()):
                    if all(invariant is not seen for seen, _ in invariants):
                        invariants.append((invariant, set(getFieldNames(iface))))
        # A factory of a behaviour can store a field under another name, as
        # the `DCFieldProperty` of plone.app.dexterity does.
        stored = {}
        for behavior in fti.behaviors:
            try:
                registration = lookup_behavior_registration(
                    name=behavior, warn_about_fallback=False
                )
            except BehaviorRegistrationNotFound:
                # Plone skips a behaviour that is not registered, and so
                # does the derivation.
                continue
            factory = getattr(registration, "factory", None)
            for field in getFieldNames(registration.interface):
                other = getattr(getattr(factory, field, None), "_get_name", None)
                if isinstance(other, str) and other != field:
                    stored[field] = other
        typed.append((name, fields, invariants, stored))
    return {"classes": class_entries(typed, DEFAULT_MODE)}


def site_overrides(site, create=False):
    """Return the overrides of each profile that the site keeps, or None
    when no profile with overrides was imported and `create` is false."""
    from Acquisition import aq_base
    from persistent.mapping import PersistentMapping

    base = aq_base(site)
    kept = getattr(base, SITE_OVERRIDES, None)
    if kept is None and create:
        kept = PersistentMapping()
        setattr(base, SITE_OVERRIDES, kept)
    return kept


def lowest_mode(modes, site, mode):
    """Record `mode`, the latest mode of `SITE_SWITCH` that the site of the
    path `site` asks for, in `modes`, a mapping of site path to mode, and
    return the lowest mode of every site. `None` for `mode` removes the
    site. Return `None` when no site gives a mode. Log a warning when the
    mode of another site is lower than `mode`. Change `modes` only when the
    mode of the site changes."""
    # Write `modes` only on a change, so that an update that changes no
    # mode does not store the mapping again.
    if mode is None:
        if site in modes:
            del modes[site]
    elif modes.get(site) != mode:
        modes[site] = mode
    if not modes:
        return None
    lowest = min(modes.values(), key=LOWER.index)
    if mode is not None and lowest != mode:
        lower = sorted(path for path, given in modes.items() if given == lowest)
        logger.warning(
            "%s asks for %s %s, and the database keeps %s for %s",
            site,
            SITE_SWITCH,
            mode,
            lowest,
            ", ".join(lower),
        )
    return lowest


def log_rise(site, stored, before, lowest):
    """Log that the update of the site of the path `site` raises
    `SITE_SWITCH` from `stored`, the mode of the policy of the database, to
    `lowest`, the new lowest mode of the sites. `before` is the lowest mode
    of the sites before the update, or None. Log a warning when `stored` is
    lower than `before`: no site asked for it, as when `zoe.policy.install`
    wrote it. Else log at INFO: a site raised its mode, or a site is gone.
    Log nothing when `stored` is None or not lower than `lowest`."""
    if stored is None or LOWER.index(stored) >= LOWER.index(lowest):
        return
    if before is None or LOWER.index(stored) < LOWER.index(before):
        logger.warning(
            "%s raises %s from %s, the mode of the policy of the database "
            "that no site asks for, to %s, the lowest mode of its sites: a "
            "mode for the whole database goes in the overrides of a site",
            site,
            SITE_SWITCH,
            stored,
            lowest,
        )
    else:
        logger.info(
            "%s raises %s from %s to %s, the lowest mode of the sites",
            site,
            SITE_SWITCH,
            stored,
            lowest,
        )


def gone_sites(site, modes, path):
    """Return the paths of `modes`, other than `path`, whose site is gone.
    A site is gone when the walk of its path from the Zope root of `site`
    stops at an object of the database of `site` that holds no item of the
    next id. Keep a path when the walk cannot tell: it stops at an object of
    another database (a mount point), it meets an object that is not a
    container, or it fails."""
    from Acquisition import aq_base
    from ZODB.POSException import ConflictError

    try:
        top = site.getPhysicalRoot()
    except AttributeError:
        return []
    database = site._p_jar.db()
    missing = object()
    gone = []
    for other in modes:
        if other == path or not other.startswith("/"):
            continue
        try:
            current = top
            for name in other.split("/")[1:]:
                found = current._getOb(name, missing)
                if found is missing:
                    jar = getattr(aq_base(current), "_p_jar", None)
                    if jar is not None and jar.db() is database:
                        gone.append(other)
                    break
                current = found
        except ConflictError:
            raise
        except Exception:
            continue
    return gone


def update_policy(site):
    """Derive the policy of `site`, apply the overrides of each profile,
    compose it with the policy of the database and write it. Return its new
    version."""
    derived = derive(site)
    names = set(derived["classes"])
    # The classes that the derivation blocks stay blocked through the
    # overrides of every profile.
    blocked = {
        name for name, entry in derived["classes"].items() if entry.get("blocked")
    }
    policy = {"classes": {**derived["classes"], **history.policy()["classes"]}}
    removed = set()
    for overrides in (site_overrides(site) or {}).values():
        policy, gone = apply_overrides(policy, overrides, names, blocked)
        removed = (removed | gone) - set(policy["classes"])
    connection = site._p_jar
    root = connection.root()
    from persistent.mapping import PersistentMapping

    modes = root.get(SITE_MODES)
    if modes is None:
        modes = root[SITE_MODES] = PersistentMapping()
    path = "/".join(site.getPhysicalPath())
    before = min(modes.values(), key=LOWER.index) if modes else None
    for gone in gone_sites(site, modes, path):
        logger.info("%s is gone: the database drops its mode of %s", gone, SITE_SWITCH)
        del modes[gone]
    lowest = lowest_mode(modes, path, policy.get(SITE_SWITCH))
    stored = merge_policy.read(connection)
    if lowest is not None:
        policy[SITE_SWITCH] = lowest
        log_rise(path, (stored or {}).get(SITE_SWITCH), before, lowest)
    composed = settle(stored, policy, removed)
    version = merge_policy.install(connection, composed)
    logger.info(
        "the merge policy of the database is version %d, with %d classes",
        version,
        len(composed["classes"]),
    )
    return version


def profile_of(context):
    """Return the id of the profile of the import context `context`, from
    the path of its directory, or that path when no profile has it."""
    from Products.GenericSetup.utils import _getProductPath

    path = getattr(context, "_profile_path", None)
    if path is None:
        return "unknown"
    for info in context.getSetupTool().listProfileInfo():
        directory = info["path"]
        if info.get("product"):
            directory = os.path.join(_getProductPath(info["product"]), info["path"])
        if os.path.normpath(directory) == os.path.normpath(path):
            return info["id"]
    return path


def import_merge_policy(context):
    """The import step: when the profile holds `OVERRIDES`, keep its
    overrides in the site under its profile id, then `update_policy`."""
    data = context.readDataFile(OVERRIDES)
    if data is None:
        return
    overrides = json.loads(data)
    # Refuse overrides of another form before the site keeps them.
    apply_overrides({"classes": {}}, overrides)
    site = context.getSite()
    site_overrides(site, create=True)[profile_of(context)] = overrides
    update_policy(site)


def fti_modified(fti, event):
    """The subscriber of a changed Dexterity type: derive the policy of its
    site again, when a profile with overrides was imported there."""
    from zope.component.hooks import getSite

    site = getSite()
    if site is not None and site_overrides(site) is not None:
        update_policy(site)
