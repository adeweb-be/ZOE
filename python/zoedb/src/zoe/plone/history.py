"""The workflow history of `zoe.plone`: an append-only `workflow_history`
(ADR 0046, section 9).

DCWorkflow keeps the history of an object in `workflow_history`, a mapping
of workflow id to the tuple of its status dicts. Each transition reads the
tuple of its workflow, adds one status and writes the tuple again. In stock
Plone the mapping is a `PersistentMapping`, whose state is
`{'data': {...}}`: two transitions of one object from one snapshot change
the one field `data`, and conflict.

`WorkflowHistory` is the same mapping, but its state is the mapping
itself, so each workflow is a field of its own. Its entry in the merge
policy, `ENTRY`, gives every field the strategy `append` with the option
`state` set to `review_state`, the state variable of the workflows of
Plone (ADR 0046, section 12). Two transitions from one snapshot both add
their status, the committed one first, and the store merges them at the
vote (tier 2), when each status of the committed one keeps the state of
the old history: a comment, then a change of state, merge. Two changes of
state from one state, or a status of the old state after a change of
state, conflict. The record equals the record of the same transitions one
after the other.

The opt-in behaviour `zoe.plone.history` (`IAppendOnlyHistory`) gives an
object of a type with the behaviour a `WorkflowHistory` at its next status:
the status adapter of `zoe.plone.status` calls `ensure` before it adds the
status. An object that never gets a status gets no history. The import
step of the profile `zoe.plone:default` moves the stock histories of the
objects with the behaviour (`move_histories`), and leaves an object
without a history as it is; the import step of the profile
`zoe.plone:uninstall` gives each `WorkflowHistory` back as a
`PersistentMapping` (`restore_histories`).

Only `move_histories`, `restore_histories` and the import steps import
Plone.
"""

import copy
import sys
from collections.abc import MutableMapping

import transaction
from persistent import Persistent
from persistent.mapping import PersistentMapping
from zope.interface import Interface

__all__ = [
    "BATCH",
    "ENTRY",
    "IAppendOnlyHistory",
    "WorkflowHistory",
    "ensure",
    "import_histories",
    "move",
    "move_histories",
    "policy",
    "restore",
    "restore_histories",
    "uninstall",
]

#: The entry of `WorkflowHistory` in the merge policy.
ENTRY = {
    "tier2": "on",
    "default": "append",
    "options": {"append": {"state": "review_state"}},
}

#: The number of objects that an import step changes in one transaction.
BATCH = 100


class IAppendOnlyHistory(Interface):
    """The marker of the behaviour `zoe.plone.history`: the workflow
    history of the object is a `WorkflowHistory`."""


class WorkflowHistory(Persistent, MutableMapping):
    """A mapping of workflow id to the tuple of its statuses.

    Its state is the mapping itself. `__setstate__` interns the keys, as
    `Persistent.__setstate__` does, so the record of a client that loads
    the history and writes it again is the record of the store. A change
    of an item marks the object as changed.
    """

    def __init__(self, items=()):
        self._entries = dict(items)

    def __getstate__(self):
        return dict(self._entries)

    def __setstate__(self, state):
        entries = {
            (sys.intern(key) if type(key) is str else key): value
            for key, value in state.items()
        }
        self.__dict__.clear()
        self.__dict__["_entries"] = entries

    def __getitem__(self, key):
        return self._entries[key]

    def __setitem__(self, key, value):
        self._entries[key] = value
        self._p_changed = True

    def __delitem__(self, key):
        del self._entries[key]
        self._p_changed = True

    def __iter__(self):
        return iter(self._entries)

    def __len__(self):
        return len(self._entries)

    def __contains__(self, key):
        return key in self._entries

    def copy(self):
        """Return a new history with the same items."""
        return type(self)(self._entries)

    def __repr__(self):
        return f"<{type(self).__name__} of {sorted(map(repr, self._entries))}>"


def policy():
    """Return the merge policy of `WorkflowHistory` alone, for
    `zoe.policy.install`."""
    return {"classes": {f"{__name__}.WorkflowHistory": copy.deepcopy(ENTRY)}}


def _base(obj):
    """Return `obj` without its acquisition wrapper."""
    return getattr(obj, "aq_base", obj)


def ensure(obj):
    """Give `obj` a `WorkflowHistory` with the items of its history. Return
    True when the history changed. An object without a history gets an
    empty one.

    The move also marks the stock history as changed. So a transaction that
    adds a status to the stock history from the same snapshot conflicts
    with the move, whichever commits first, and its retry sees the moved
    history: no status is lost."""
    base = _base(obj)
    history = getattr(base, "workflow_history", None)
    if isinstance(history, WorkflowHistory):
        return False
    if history is not None and getattr(history, "_p_jar", None) is not None:
        history._p_changed = True
    base.workflow_history = WorkflowHistory(history.items() if history else ())
    return True


def _stock():
    """Return the class of a stock history: the `PersistentMapping` of
    Zope's `Persistence`, which Products.CMFCore uses, or that of
    `persistent` outside Zope."""
    try:
        from Persistence import PersistentMapping as stock
    except ImportError:
        stock = PersistentMapping
    return stock


#: The key that `restore` leaves in a retired `WorkflowHistory`. It is not
#: a str, so tier 2 does not take the state, and no workflow has it. Its
#: value is an empty tuple of statuses, so a stale reader that loops over
#: the values sees an empty workflow.
RETIRED = 0


def restore(obj):
    """Give `obj` its history back as a stock `PersistentMapping`. Return
    True when the history changed.

    The move retires the `WorkflowHistory`: it writes it with the one item
    `{RETIRED: ()}`, a state that tier 2 does not take. So a transaction
    that writes the old history from the same snapshot, whatever it adds,
    conflicts with the move whichever commits first: tier 2 gives no answer,
    and the client raises `ConflictError`, as `WorkflowHistory` has no
    `_p_resolveConflict`. Its retry sees the stock history, and no status
    is lost."""
    base = _base(obj)
    history = getattr(base, "workflow_history", None)
    if not isinstance(history, WorkflowHistory):
        return False
    items = list(history.items())
    history._entries.clear()
    history._entries[RETIRED] = ()
    history._p_changed = True
    base.workflow_history = _stock()(items)
    return True


def _each(site, query, change):
    """Run `change` on each object that the catalog of `site` finds with
    `query`, `BATCH` objects for each transaction. Return the number of
    objects that changed."""
    from Products.CMFCore.utils import getToolByName

    catalog = getToolByName(site, "portal_catalog")
    changed = 0
    pending = 0
    for brain in catalog.unrestrictedSearchResults(**query):
        if change(brain._unrestrictedGetObject()):
            changed += 1
            pending += 1
        if pending >= BATCH:
            transaction.commit()
            pending = 0
    if pending:
        transaction.commit()
    return changed


def history_types(site):
    """Return the ids of the Dexterity types of `site` with the behaviour,
    named by its name or by the identifier of its interface."""
    from plone.behavior.registration import (
        BehaviorRegistrationNotFound,
        lookup_behavior_registration,
    )
    from plone.dexterity.interfaces import IDexterityFTI
    from Products.CMFCore.utils import getToolByName

    names = []
    for fti in getToolByName(site, "portal_types").objectValues():
        if not IDexterityFTI.providedBy(fti):
            continue
        for behavior in fti.behaviors:
            try:
                registration = lookup_behavior_registration(
                    name=behavior, warn_about_fallback=False
                )
            except BehaviorRegistrationNotFound:
                continue
            if IAppendOnlyHistory in (registration.interface, registration.marker):
                names.append(fti.getId())
                break
    return names


def move_histories(site):
    """Give each object of `site` with the behaviour and a stock history a
    `WorkflowHistory`. An object without a history gets none: its status
    adapter gives it one at its first status. Return the number of objects
    that changed. A second run changes nothing.

    The objects come from the catalog by their type: the index
    `object_provides` of an object that the catalog indexed before the
    behaviour does not hold the marker yet."""
    names = history_types(site)
    if not names:
        return 0
    return _each(site, {"portal_type": names}, move)


def move(obj):
    """Give `obj` a `WorkflowHistory` when it has the behaviour and a stock
    history. Return True when the history changed. An object without a
    history, such as one of a type without a workflow, stays as it is."""
    if getattr(_base(obj), "workflow_history", None) is None:
        return False
    return IAppendOnlyHistory.providedBy(obj) and ensure(obj)


def restore_histories(site):
    """Give each object of `site` its history back as a
    `PersistentMapping`. Return the number of objects that changed."""
    return _each(site, {"path": "/".join(site.getPhysicalPath())}, restore)


#: The file of the profile `zoe.plone:default` whose import moves the
#: histories.
MOVE = "zoe-plone-history.txt"
#: The file of the profile `zoe.plone:uninstall` whose import gives them
#: back.
UNINSTALL = "zoe-plone-uninstall.txt"


def import_histories(context):
    """The import step of the profile `zoe.plone:default`: move the stock
    histories of the objects with the behaviour. It runs at each import of
    the profile, and changes only what is left to move."""
    if context.readDataFile(MOVE) is None:
        return
    moved = move_histories(context.getSite())
    context.getLogger("zoe.plone").info("moved %d workflow histories", moved)


def uninstall(context):
    """The import step of the profile `zoe.plone:uninstall`: remove the
    behaviour from each Dexterity type, then give each `WorkflowHistory`
    back as a `PersistentMapping`."""
    if context.readDataFile(UNINSTALL) is None:
        return
    from plone.dexterity.interfaces import IDexterityFTI
    from Products.CMFCore.utils import getToolByName

    site = context.getSite()
    names = set(history_types(site))
    marker = IAppendOnlyHistory.__identifier__
    for fti in getToolByName(site, "portal_types").objectValues():
        if IDexterityFTI.providedBy(fti) and fti.getId() in names:
            kept = tuple(
                behavior
                for behavior in fti.behaviors
                if behavior not in ("zoe.plone.history", marker)
            )
            fti.manage_changeProperties(behaviors=kept)
    restored = restore_histories(site)
    context.getLogger("zoe.plone").info("restored %d workflow histories", restored)
