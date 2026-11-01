"""The guard of a stock registry of plone.registry (ADR 0050).

In a store with namespaces, `install_guard()` makes each stock registry
call `readCurrent` on the roots of its trees when code reads its records.
The move of `zoe.plone.registry_ops` empties those roots, so a write that
began before a move conflicts, and a write of an attribute of the registry
object does not. `configure.zcml` installs the guard whenever `zoe.plone`
loads. `zoe.plone.registry` gives these names.
"""

import weakref

import transaction.interfaces
from plone.registry.registry import Registry

from zoe.plone._bases import has_namespaces

__all__ = ["install_guard", "uninstall_guard"]

# The stock property `records` while the guard is installed.
_guard = {}

# Whether the storage of each database has namespaces.
_namespaces = weakref.WeakKeyDictionary()

# The checks of the guard in the current transaction of each connection:
# `(transaction, {registry oid: (oid of _fields, oid of _values, whole)})`.
# `whole` tells if the guard read the registry object current too.
_guarded = weakref.WeakKeyDictionary()


def _has_namespaces(jar):
    """Tell if the storage of the database of `jar` has namespaces."""
    db = jar.db()
    found = _namespaces.get(db)
    if found is None:
        found = _namespaces[db] = has_namespaces(jar)
    return found


def install_guard():
    """Make each stock registry in a store with namespaces call
    `readCurrent` on the roots of its trees when code reads its records. A
    second call does nothing."""
    if _guard:
        return
    from zoe.plone.registry_ops import MovedRecords

    stock = Registry.__dict__["records"]

    def records(self):
        found = stock.fget(self)
        if not isinstance(found, MovedRecords):
            _read_current(self, found)
        return found

    _guard["records"] = stock
    Registry.records = property(records, doc=stock.__doc__)


def uninstall_guard():
    """Put the stock property `records` back."""
    if _guard:
        Registry.records = _guard.pop("records")


def _holds_its_bucket(tree):
    """Tell if the loaded `tree` keeps its keys in its own state: an empty
    tree, or a tree of one bucket. A write of a record then writes the
    root itself."""
    state = tree.__getstate__()
    return state is None or len(state) == 1


def _read_current(registry, records):
    """Call `readCurrent` on the roots of `_fields` and `_values` of the
    stock `records` of `registry`, when it is in a store with namespaces.
    A move empties these roots, so a write that began before the move
    conflicts. A write of an attribute of the registry object does not.

    A root that keeps its own keys is written by each write of a record,
    and a resolution of a conflict could then merge the write into the
    trees that the move left. So for such a root, the guard calls
    `readCurrent` on the registry object too, which the move writes.

    The guard returns at once when it checked the same roots in the
    current transaction, the roots are still read current and unchanged,
    and the registry object is still read current or changed, if the check
    read it. It then loads no tree and reads no state. A savepoint that
    stores an object takes it out of the objects read current, and a
    rollback does not put it back. So after such a rollback, the guard
    checks again."""
    jar = registry._p_jar
    oid = registry._p_oid
    if jar is None or oid is None:
        return
    trees = (records._fields, records._values)
    read = jar._readCurrent
    try:
        txn = jar.transaction_manager.get()
    except transaction.interfaces.NoTransaction:
        txn = None
    guarded = _guarded.get(jar)
    if guarded is not None and guarded[0] is txn and txn is not None:
        checked = guarded[1].get(oid)
        if (
            checked is not None
            and all(
                tree._p_jar is jar
                and tree._p_oid == checked[index]
                and tree._p_oid in read
                and not tree._p_changed
                for index, tree in enumerate(trees)
            )
            and (not checked[2] or oid in read or registry._p_changed)
        ):
            return
    if not _has_namespaces(jar):
        return
    whole = False
    for tree in trees:
        if tree._p_jar is not jar or tree._p_oid is None:
            # A tree that is not stored yet.
            whole = True
            continue
        # A ghost has no serial yet.
        tree._p_activate()
        if tree._p_oid not in jar._readCurrent:
            jar.readCurrent(tree)
        if _holds_its_bucket(tree):
            whole = True
    if whole and registry._p_oid not in jar._readCurrent:
        jar.readCurrent(registry)
    if txn is None:
        return
    if guarded is None or guarded[0] is not txn:
        guarded = _guarded[jar] = (txn, {})
    guarded[1][oid] = (trees[0]._p_oid, trees[1]._p_oid, whole)
