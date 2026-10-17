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
policy, `ENTRY`, gives every field the strategy `append`: two transitions
from one snapshot both add their status, the committed one first, and the
store merges them at the vote (tier 2). The record equals the record of the
same transitions one after the other.

The opt-in behaviour `zoe.plone.history` (`IAppendOnlyHistory`) gives an
object of a type with the behaviour a `WorkflowHistory` when it is added.
`upgrade` moves the stock histories of the objects that have the behaviour.
A type without the behaviour keeps the stock history.

Only `added` and `upgrade` import Plone.
"""

import sys

import transaction
from persistent import Persistent
from zope.interface import Interface

__all__ = [
    "BATCH",
    "ENTRY",
    "IAppendOnlyHistory",
    "WorkflowHistory",
    "added",
    "ensure",
    "policy",
    "upgrade",
]

#: The entry of `WorkflowHistory` in the merge policy.
ENTRY = {"tier2": "on", "default": "append"}

#: The number of objects that `upgrade` moves in one transaction.
BATCH = 100


class IAppendOnlyHistory(Interface):
    """The marker of the behaviour `zoe.plone.history`: the workflow
    history of the object is a `WorkflowHistory`."""


class WorkflowHistory(Persistent):
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

    def __contains__(self, key):
        return key in self._entries

    def __iter__(self):
        return iter(self._entries)

    def __len__(self):
        return len(self._entries)

    def get(self, key, default=None):
        return self._entries.get(key, default)

    def keys(self):
        return self._entries.keys()

    def values(self):
        return self._entries.values()

    def items(self):
        return self._entries.items()

    def __repr__(self):
        return f"<{type(self).__name__} of {sorted(self._entries)}>"


def policy():
    """Return the merge policy of `WorkflowHistory` alone, for
    `zoe.policy.install`."""
    return {"classes": {f"{__name__}.WorkflowHistory": dict(ENTRY)}}


def ensure(obj):
    """Give `obj` a `WorkflowHistory` with the items of its history. Return
    True when the history changed. An object without a history gets an
    empty one."""
    from Acquisition import aq_base

    base = aq_base(obj)
    history = getattr(base, "workflow_history", None)
    if isinstance(history, WorkflowHistory):
        return False
    base.workflow_history = WorkflowHistory(history.items() if history else ())
    return True


def added(obj, event):
    """The subscriber of an added object with the behaviour: the stock
    history that the workflow tool wrote for its first status becomes a
    `WorkflowHistory`, in the same transaction."""
    ensure(obj)


def upgrade(context):
    """The upgrade step: move the stock history of each object of a type
    with the behaviour to a `WorkflowHistory`, `BATCH` objects for each
    transaction. Return the number of objects that changed."""
    from plone.dexterity.interfaces import IDexterityFTI
    from Products.CMFCore.utils import getToolByName

    portal = getToolByName(context, "portal_url").getPortalObject()
    types = getToolByName(portal, "portal_types")
    names = [
        fti.getId()
        for fti in types.objectValues()
        if IDexterityFTI.providedBy(fti)
        and "zoe.plone.history" in getattr(fti, "behaviors", ())
    ]
    if not names:
        return 0
    catalog = getToolByName(portal, "portal_catalog")
    moved = 0
    pending = 0
    for brain in catalog.unrestrictedSearchResults(portal_type=names):
        obj = brain._unrestrictedGetObject()
        if ensure(obj):
            moved += 1
            pending += 1
        if pending >= BATCH:
            transaction.commit()
            pending = 0
    if pending:
        transaction.commit()
    return moved
