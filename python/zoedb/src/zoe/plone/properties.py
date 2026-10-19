"""The properties of the members and groups of `zoe.plone`: one record for
each principal, whose fields are its properties (ADR 0046, section 9).

The mutable property sheet of PAS (`acl_users/mutable_properties`, a
`ZODBMutablePropertyProvider` of Products.PlonePAS) keeps the properties of
each member and of each group in one plain dict, the value of the id of
the principal in its `OOBTree` `_storage`. A login writes two properties,
`login_time` and `last_login_time`. The resolver of BTrees compares the
values of a key with `<`, and a dict does not support `<`. So two changes
of one bucket of `_storage` conflict, also two changes of two principals.

`PrincipalProperties` is the same mapping, but it is a persistent object
of its own, and its state is the mapping itself, so each property is a
field. The bucket then holds a reference for each principal. A login of a
principal that has one sets the key of the principal to the same
reference again, so the bucket does not change, and tier 1 merges two
such writes of the bucket, as ZODB does. The login changes the record of
the principal. Its entry in the merge policy, `ENTRY`, gives the two times
of a login the strategy `max` and every other property `strict`. So two
logins of one member from one snapshot merge at the vote (tier 2), and two
changes of another property of one principal still conflict.

The import step of the profile `zoe.plone:default` gives each dict of
`_storage` of each mutable property sheet of the site the class
(`move_properties`), groups included: one dict is enough to make the
resolver refuse the bucket. It marks the sheet. Then each write of the
properties of a new principal on a marked sheet stores a
`PrincipalProperties` (`install` patches `setPropertiesForUser`). The read
of the properties of a principal copies the stored properties before it
adds the defaults, so a read writes nothing (`install` patches
`getPropertiesForUser`). The import step of the profile
`zoe.plone:uninstall` gives each value back as a dict and removes the mark
(`restore_properties`).

Only `install`, `uninstall`, the sheets of a site and the import step
import Plone.
"""

import copy
import sys
from collections.abc import MutableMapping

import transaction
from persistent import Persistent

__all__ = [
    "BATCH",
    "ENTRY",
    "MARK",
    "PrincipalProperties",
    "import_properties",
    "install",
    "installed",
    "move",
    "move_properties",
    "policy",
    "restore",
    "restore_properties",
    "uninstall",
]

#: The entry of `PrincipalProperties` in the merge policy.
ENTRY = {
    "tier2": "on",
    "default": "strict",
    "fields": {"login_time": "max", "last_login_time": "max"},
}

#: The number of values that an import step changes in one transaction.
BATCH = 100

#: The attribute of a sheet whose values the import step moved. A write of
#: the properties of a principal on a sheet with this mark stores a
#: `PrincipalProperties`.
MARK = "_zoe_principal_properties"

#: The key that `restore` leaves in a retired `PrincipalProperties`. It is
#: not a str, so tier 2 does not take the state, and no property has it.
RETIRED = 0


class PrincipalProperties(Persistent, MutableMapping):
    """A mapping of property name to value, for one member or one group.

    Its state is the mapping itself. `__setstate__` interns the keys, as
    `Persistent.__setstate__` does, so the record of a client that loads
    the properties and writes them again is the record of the store. A
    change of an item marks the object as changed.
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
        """Return a dict with the same items."""
        return dict(self._entries)

    def __repr__(self):
        return f"<{type(self).__name__} of {sorted(map(repr, self._entries))}>"


def policy():
    """Return the merge policy of `PrincipalProperties` alone, for
    `zoe.policy.install`."""
    return {"classes": {f"{__name__}.PrincipalProperties": copy.deepcopy(ENTRY)}}


def _base(obj):
    """Return `obj` without its acquisition wrapper."""
    return getattr(obj, "aq_base", obj)


def marked(sheet):
    """Tell if the import step moved the values of `sheet`."""
    return bool(getattr(_base(sheet), MARK, False))


def _batches(keys, change):
    """Run `change` on each key of `keys`. Commit after each `BATCH`
    changes. The last changes stay in the current transaction. Return the
    number of changes."""
    changed = 0
    for key in keys:
        if change(key):
            changed += 1
            if changed % BATCH == 0:
                transaction.commit()
    return changed


def move(storage, key):
    """Give the value of `key` in `storage` the class, when it is a dict.
    Return True when the value changed.

    The move changes the bucket of the key. So a write of the dict from the
    same snapshot conflicts with the move, whichever commits first, and its
    retry sees the moved value."""
    value = storage.get(key)
    if not isinstance(value, dict):
        return False
    storage[key] = PrincipalProperties(value)
    return True


def restore(storage, key):
    """Give the value of `key` in `storage` back as a dict, when it has the
    class. Return True when the value changed.

    The restore retires the `PrincipalProperties`: it writes it with the one
    item `{RETIRED: None}`, a state that tier 2 does not take. So a
    transaction that writes the old properties from the same snapshot
    conflicts with the restore, whichever commits first, and its retry sees
    the dict."""
    value = storage.get(key)
    if not isinstance(value, PrincipalProperties):
        return False
    items = dict(value._entries)
    value._entries.clear()
    value._entries[RETIRED] = None
    value._p_changed = True
    storage[key] = items
    return True


def move_properties(sheet):
    """Mark `sheet` and give each dict of its `_storage` the class. Return
    the number of values that changed. A second run changes nothing."""
    base = _base(sheet)
    if not getattr(base, MARK, False):
        setattr(base, MARK, True)
    storage = base._storage
    return _batches(list(storage.keys()), lambda key: move(storage, key))


def restore_properties(sheet):
    """Remove the mark of `sheet` and give each value of its `_storage`
    back as a dict. Return the number of values that changed."""
    base = _base(sheet)
    if getattr(base, MARK, False):
        delattr(base, MARK)
    storage = base._storage
    return _batches(list(storage.keys()), lambda key: restore(storage, key))


def sheets(site):
    """Return the mutable property sheets of the PAS of `site`."""
    from Products.PlonePAS.plugins.property import ZODBMutablePropertyProvider

    pas = getattr(_base(site), "acl_users", None)
    if pas is None:
        return []
    return [
        plugin
        for plugin in pas.objectValues()
        if isinstance(_base(plugin), ZODBMutablePropertyProvider)
    ]


#: The file of the profile `zoe.plone:default` whose import moves the
#: properties.
MOVE = "zoe-plone-properties.txt"
#: The file of the profile `zoe.plone:uninstall` whose import gives them
#: back.
UNINSTALL = "zoe-plone-uninstall.txt"


def import_properties(context):
    """The import step: in the profile `zoe.plone:default`, move the
    properties of each mutable property sheet of the site; in the profile
    `zoe.plone:uninstall`, give them back as dicts. It runs at each import
    of the profile, and changes only what is left to change."""
    logger = context.getLogger("zoe.plone")
    if context.readDataFile(MOVE) is not None:
        moved = sum(move_properties(sheet) for sheet in sheets(context.getSite()))
        logger.info("moved the properties of %d principals", moved)
    if context.readDataFile(UNINSTALL) is not None:
        restored = sum(restore_properties(sheet) for sheet in sheets(context.getSite()))
        logger.info("restored the properties of %d principals", restored)


# The stock methods of the sheet while the patch is installed.
_stock = {}


def installed():
    """Tell if the patch of the sheet is installed."""
    return bool(_stock)


def install():
    """Patch the mutable property sheet of PlonePAS: a read copies the
    stored properties, and a write on a marked sheet stores a
    `PrincipalProperties`. A second call does nothing."""
    if _stock:
        return
    from Products.PlonePAS.plugins.property import ZODBMutablePropertyProvider
    from Products.PlonePAS.sheet import MutablePropertySheet

    _stock["getPropertiesForUser"] = ZODBMutablePropertyProvider.getPropertiesForUser
    _stock["setPropertiesForUser"] = ZODBMutablePropertyProvider.setPropertiesForUser

    def getPropertiesForUser(self, user, request=None):
        stored = self._storage.get(user.getId())
        if not isinstance(stored, PrincipalProperties):
            return _stock["getPropertiesForUser"](self, user, request)
        # The stock method adds the defaults to the stored value. Add them
        # to a copy, so that the read does not change the record. A retired
        # value holds a key that is not a str: leave it out.
        is_group = getattr(user, "isGroup", lambda: None)()
        data = {key: value for key, value in stored.items() if type(key) is str}
        for key, value in self._getDefaultValues(is_group).items():
            data.setdefault(key, value)
        return MutablePropertySheet(self.id, schema=self._getSchema(is_group), **data)

    def setPropertiesForUser(self, user, propertysheet):
        _stock["setPropertiesForUser"](self, user, propertysheet)
        if marked(self):
            move(self._storage, user.getId())

    for name, method in (
        ("getPropertiesForUser", getPropertiesForUser),
        ("setPropertiesForUser", setPropertiesForUser),
    ):
        method.__doc__ = _stock[name].__doc__
        setattr(ZODBMutablePropertyProvider, name, method)


def uninstall():
    """Put the stock methods back."""
    if not _stock:
        return
    from Products.PlonePAS.plugins.property import ZODBMutablePropertyProvider

    for name in ("getPropertiesForUser", "setPropertiesForUser"):
        setattr(ZODBMutablePropertyProvider, name, _stock.pop(name))
