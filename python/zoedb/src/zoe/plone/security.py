"""The security backend of `zoe.plone` (ADR 0061): `allowedRolesAndUsers`
with inherited inputs.

The `allowedRolesAndUsers` indexer of CMFPlone computes the value of an
object from two inputs: the roles of the permission "Access contents
information" (`rolesForPermissionOn`), and the local roles
(`_getAllLocalRoles`). Each input reads the object, then its containers.

`ZoeSecurityIndex` takes the place of the `KeywordIndex`
`allowedRolesAndUsers`. Its tree `_inputs` keeps, for each rid, what the
object holds itself: the kind and the roles of its setting of the
permission, its own local roles, its block flag, and the chain: the same
inputs of each container outside the catalog above it, up to the first
catalog entry, nearest first. A query resolves the value of each entry
through the paths of the catalog, as the indexer computes it. So a share
or a transition of a folder writes the entry of the folder alone.

An entry is opaque when the index cannot read one of its inputs, or when
it is below an opaque entry. An opaque entry keeps the value of the
indexer. The mirror of the store resolves the same inputs the same way
(`crates/zoe-catalog/src/security.rs`).

The backend: `install()` replaces `reindexObjectSecurity` of
`CatalogAware` and `deleteLocalRoles` of `MembershipTool` in the process,
and puts the index into each catalog of the databases that the process has
open and of each database that opens later, in a thread under the lease of
the site. A site created later gets the index in the transaction that
creates it. `uninstall()` gives the stock methods and the stock index back.
A catalog without a `ZoeSecurityIndex` runs the stock methods.

Only the functions that Plone calls import Plone.
"""

import logging
import os
import threading
import time

import transaction
from Acquisition import aq_base, aq_inner, aq_parent
from BTrees.IIBTree import IISet, IITreeSet
from BTrees.IOBTree import IOBTree
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex
from Products.ZCatalog.Catalog import Catalog
from ZODB.POSException import ConflictError

import zoe.kv

__all__ = ["ZoeSecurityIndex", "install", "installed", "uninstall"]

logger = logging.getLogger("zoe.plone.security")

PERMISSION = "Access contents information"
ATTRIBUTE = "_Access_contents_information_Permission"
INDEX = "allowedRolesAndUsers"

# The kinds of a permission setting, as `_inputs` keeps them.
MISSING = ""  # no setting: the roles of the container
TUPLE = "t"  # these roles only
LIST = "l"  # these roles and the roles of the container
NONE = "a"  # the setting None: Anonymous
OPAQUE = "v"  # the value of the indexer, for an opaque entry

# The roles of a node that no node up to the root sets.
_UNSET = object()
# The roles of a node at or below the setting None.
_ANONYMOUS = object()
# The state of a node whose entries below give no token.
_CLOSED = object()

# The time between two writes of the lease while the index goes in.
LEASE_EVERY = 10.0
# The tries of the transaction that puts the index in or out.
TRIES = 5


def _default_adapter():
    from borg.localrole.default_adapter import DefaultLocalRoleAdapter

    return DefaultLocalRoleAdapter


def _node_inputs(obj):
    """Return the inputs that `obj` holds itself, as the tuple `(kind,
    roles, local, block)`, or None when the index does not read them.

    `kind` and `roles` follow `rolesForPermissionOn`: a str names another
    permission (the empty str gives no role), so the index does not read
    it. `local` holds the local roles of the default adapter of
    borg.localrole, as sorted pairs `(principal, roles)`. A local role
    provider of another class, an `IWorkspace` adapter or a callable
    `__ac_local_roles__` makes the inputs unreadable.
    """
    from borg.localrole.bbb.interfaces import IGroupAwareWorkspace, IWorkspace
    from borg.localrole.interfaces import ILocalRoleProvider
    from zope.component import getAdapters

    base = aq_base(obj)
    if not hasattr(base, ATTRIBUTE):
        kind, roles = MISSING, ()
    else:
        setting = getattr(base, ATTRIBUTE)
        if setting is None:
            kind, roles = NONE, ()
        elif type(setting) is tuple:
            kind, roles = TUPLE, setting
        elif isinstance(setting, str):
            return None
        elif not setting:
            # An empty sequence counts as no setting.
            kind, roles = LIST, ()
        else:
            kind, roles = LIST, setting
        try:
            roles = tuple(sorted(set(roles)))
        except TypeError:
            return None
        if not all(isinstance(role, str) for role in roles):
            return None
    if callable(getattr(base, "__ac_local_roles__", None)):
        return None
    if IGroupAwareWorkspace(obj, IWorkspace(obj, None)) is not None:
        return None
    default = _default_adapter()
    local = {}
    for _name, adapter in getAdapters((obj,), ILocalRoleProvider):
        if type(adapter) is not default:
            return None
        for principal, granted in adapter.getAllRoles():
            if not isinstance(principal, str):
                return None
            local.setdefault(principal, set()).update(granted)
    try:
        pairs = tuple(sorted((p, tuple(sorted(r))) for p, r in local.items() if r))
    except TypeError:
        return None
    block = bool(getattr(obj, "__ac_local_roles_block__", None))
    return (kind, roles, pairs, block)


def _stock_entry(obj, tool):
    """Return True when the indexer of CMFPlone and the stock local role
    plugin give the value of the entry of `obj`, the object that the
    catalog indexes: the wrapper of plone.indexer, or the object."""
    from borg.localrole.workspace import WorkspaceLocalRoleManager
    from plone.indexer.interfaces import IIndexer
    from Products.CMFCore.utils import getToolByName
    from Products.CMFPlone.CatalogTool import allowedRolesAndUsers
    from Products.PlonePAS.interfaces.plugins import ILocalRolesPlugin
    from zope.component import queryMultiAdapter

    unwrap = getattr(obj, "_getWrappedObject", None)
    if unwrap is None:
        return False
    target = unwrap()
    found = queryMultiAdapter((target, tool), IIndexer, name=INDEX)
    if getattr(found, "callable", None) is not allowedRolesAndUsers.callable:
        return False
    acl_users = getToolByName(target, "acl_users", None)
    if getattr(aq_base(acl_users), "plugins", None) is None:
        return False
    try:
        active = acl_users.plugins.listPlugins(ILocalRolesPlugin)
    except Exception:
        return False
    return len(active) == 1 and isinstance(
        aq_base(active[0][1]), WorkspaceLocalRoleManager
    )


def _parent(path):
    if not path:
        return None
    return path.rpartition("/")[0]


def _path(obj):
    return "/".join(obj.getPhysicalPath())


def _apply(node, roles, local):
    """Return the roles and the local roles below `node`, the inputs
    `(kind, roles, local, block)` of one object, from those above it."""
    kind, own, pairs, block = node
    if kind == TUPLE:
        roles = frozenset(own)
    elif kind == NONE:
        roles = _ANONYMOUS
    elif kind == LIST:
        if roles is _ANONYMOUS:
            pass
        elif roles is _UNSET:
            roles = frozenset(own) if own else _UNSET
        elif own:
            roles = roles | frozenset(own)
    if block:
        local = {principal: frozenset(granted) for principal, granted in pairs}
    elif pairs:
        local = dict(local)
        for principal, granted in pairs:
            local[principal] = local.get(principal, frozenset()) | frozenset(granted)
    return roles, local


class _Values(dict):
    """The values of the entries of one snapshot: a dict of rid to value.

    `distinct` maps each distinct value to the one frozenset that the
    entries with that value share."""

    __slots__ = ("distinct",)

    def __init__(self):
        super().__init__()
        self.distinct = {}


class Resolver:
    """Resolve the values of the entries of one index, with a memo.

    `values` is a `_Values` of the values that the resolver gives and that
    it reads first: the cache of a snapshot, or a new one."""

    def __init__(self, inputs, paths, uids, values=None):
        self._inputs = inputs
        self._paths = paths
        self._uids = uids
        self._states = {}
        self.values = _Values() if values is None else values

    def _entry(self, path):
        """Return the rid and the inputs of the entry at `path`, or None
        when the catalog has no entry with inputs there."""
        rid = self._uids.get(path)
        if rid is None:
            return None
        found = self._inputs.get(rid)
        if found is None:
            return None
        return rid, found

    def _state(self, rid, inputs, path):
        """Return the roles and the local roles at the entry `rid`, or
        `_CLOSED` when the entries below it give no token."""
        memo = self._states
        if rid in memo:
            return memo[rid]
        state = self._compute(inputs, path)
        memo[rid] = state
        return state

    def _compute(self, inputs, path):
        if inputs[0] == OPAQUE:
            return _CLOSED
        at = _parent(path)
        used = []
        for item in inputs[4]:
            if at is None or item[0] != at:
                break
            if self._entry(at) is not None:
                # An entry of the catalog takes the place of the item.
                break
            if item[1] == OPAQUE:
                return _CLOSED
            used.append(item[1:])
            at = _parent(at)
        if at is None:
            roles, local = _UNSET, {}
        else:
            found = self._entry(at)
            if found is None:
                return _CLOSED
            above = self._state(found[0], found[1], at)
            if above is _CLOSED:
                return _CLOSED
            roles, local = above
        for node in reversed(used):
            roles, local = _apply(node, roles, local)
        return _apply(inputs[:4], roles, local)

    def value(self, rid):
        """Return the value of the entry `rid` as a frozenset, or None for
        a rid without inputs."""
        values = self.values
        found = values.get(rid)
        if found is not None:
            return found
        inputs = self._inputs.get(rid)
        if inputs is None:
            return None
        if inputs[0] == OPAQUE:
            found = frozenset(inputs[1])
        else:
            path = self._paths.get(rid)
            state = _CLOSED if path is None else self._state(rid, inputs, path)
            found = frozenset() if state is _CLOSED else self._value(*state)
        found = values.distinct.setdefault(found, found)
        values[rid] = found
        return found

    def _value(self, roles, local):
        if roles is _ANONYMOUS:
            return frozenset({"Anonymous"})
        allowed = {"Manager"} if roles is _UNSET else set(roles)
        if "Anonymous" in allowed:
            return frozenset({"Anonymous"})
        if "Authenticated" in allowed:
            return frozenset({"Authenticated"})
        held = frozenset(allowed)
        for principal, granted in local.items():
            if not held.isdisjoint(granted):
                allowed.add("user:" + principal)
        allowed.discard("Owner")
        return frozenset(allowed)


# The values of the snapshots that the process resolved: the key of a
# snapshot to its `_Values`. Only the newest snapshots stay, by the tid of
# their start.
_cache = {}
_cache_lock = threading.Lock()
_CACHE_SNAPSHOTS = 2


def _savepoint_records(jar):
    savepoint = getattr(jar, "_savepoint_storage", None)
    if savepoint is None:
        return False
    try:
        return len(savepoint) > 0
    except TypeError:
        return True


def _snapshot_values(index):
    """Return the `_Values` of the snapshot of the connection of `index`,
    or None when the values cannot come from the cache: the connection
    holds a change of its transaction, has no snapshot, or has a snapshot
    older than those that the cache keeps."""
    jar = getattr(aq_base(index), "_p_jar", None)
    oid = getattr(aq_base(index), "_p_oid", None)
    if jar is None or oid is None:
        return None
    if jar._registered_objects or _savepoint_records(jar):
        return None
    start = getattr(getattr(jar, "_storage", None), "_start", None)
    if start is None:
        return None
    key = (jar.db(), oid, start)
    with _cache_lock:
        values = _cache.get(key)
        if values is not None:
            return values
        if len(_cache) >= _CACHE_SNAPSHOTS and start < min(k[2] for k in _cache):
            return None
        values = _cache[key] = _Values()
        while len(_cache) > _CACHE_SNAPSHOTS:
            del _cache[min(_cache, key=lambda k: k[2])]
    return values


class ZoeSecurityIndex(KeywordIndex):
    """`allowedRolesAndUsers` with the inherited inputs of each entry,
    resolved at query time through the paths of the catalog."""

    meta_type = "ZoeSecurityIndex"
    operators = ("or", "and")
    useOperator = "or"

    def clear(self):
        self._length = Length()
        self._index = OOBTree()
        self._unindex = IOBTree()
        # rid -> (kind, roles, local, block, chain)
        self._inputs = IOBTree()
        # The rids of the opaque entries.
        self._opaque = IITreeSet()
        if self._counter is None:
            self._counter = Length()
        else:
            self._increment_counter()

    def _catalog(self):
        """Return the `Catalog` that holds this index, wrapped in its tool.
        The ZMI wraps the index in the `Indexes` of the tool, not in the
        `Catalog`."""
        parent = aq_parent(aq_inner(self))
        while parent is not None and not isinstance(aq_base(parent), Catalog):
            inner = getattr(aq_base(parent), "_catalog", None)
            if isinstance(inner, Catalog):
                return inner.__of__(parent)
            parent = aq_parent(aq_inner(parent))
        return parent

    def _tool(self):
        return aq_parent(aq_inner(self._catalog()))

    def resolver(self):
        """Return a resolver of this index, with the cache of the snapshot
        of its connection when it has one."""
        catalog = self._catalog()
        return Resolver(
            self._inputs, catalog.paths, catalog.uids, _snapshot_values(self)
        )

    def _chain(self, obj, uids):
        """Return the inputs of the containers of `obj` outside the
        catalog, up to the first catalog entry, nearest first, each with
        its path; None when one of them is unreadable."""
        chain = []
        current = aq_parent(aq_inner(obj))
        while current is not None and hasattr(aq_base(current), "getPhysicalPath"):
            path = _path(current)
            if uids.get(path) is not None:
                break
            found = _node_inputs(current)
            if found is None:
                return None
            chain.append((path, *found))
            current = aq_parent(aq_inner(current))
        return tuple(chain)

    def _below_opaque(self, path):
        """Return True when an entry of a container of `path` is opaque."""
        if not self._opaque:
            return False
        uids = self._catalog().uids
        above = _parent(path)
        while above is not None:
            rid = uids.get(above)
            if rid is not None and rid in self._opaque:
                return True
            above = _parent(above)
        return False

    def entry_inputs(self, obj):
        """Return the inputs of the entry of `obj`, the object that the
        catalog indexes, and tell if the entry is opaque."""
        target = obj
        unwrap = getattr(obj, "_getWrappedObject", None)
        if unwrap is not None:
            target = unwrap()
        inputs = None
        if _stock_entry(obj, self._tool()) and not self._below_opaque(_path(target)):
            own = _node_inputs(target)
            if own is not None:
                chain = self._chain(target, self._catalog().uids)
                if chain is not None:
                    inputs = (*own, chain)
        if inputs is None:
            value = tuple(sorted(self._get_object_keywords(obj, INDEX)))
            return (OPAQUE, value, (), False, ()), True
        return inputs, False

    def index_object(self, documentId, obj, threshold=None):
        inputs, opaque = self.entry_inputs(obj)
        if self._inputs.get(documentId) == inputs:
            return 0
        self._inputs[documentId] = inputs
        if opaque:
            self._opaque.insert(documentId)
        elif documentId in self._opaque:
            self._opaque.remove(documentId)
        self._increment_counter()
        return 1

    def unindex_object(self, documentId):
        if self._inputs.get(documentId) is None:
            return None
        del self._inputs[documentId]
        if documentId in self._opaque:
            self._opaque.remove(documentId)
        self._increment_counter()
        return None

    def opaque(self, obj):
        """Tell if `obj` gives an opaque entry. The answer reads the object
        and the entries of its containers, not the entry of the object,
        which a queue may not have applied yet."""
        from plone.indexer.interfaces import IIndexableObject
        from zope.component import queryMultiAdapter

        wrapper = queryMultiAdapter((obj, self._tool()), IIndexableObject, default=obj)
        return self.entry_inputs(wrapper)[1]

    def getEntryForObject(self, documentId, default=None):
        value = self.resolver().value(documentId)
        if value is None:
            return default
        return sorted(value)

    def keyForDocument(self, documentId):
        return self.getEntryForObject(documentId)

    def numObjects(self):
        return len(self._inputs)

    def indexSize(self):
        # The plan of ZCatalog asks it of the index outside the catalog,
        # where it cannot resolve.
        return len(self._inputs)

    def _values(self):
        resolver = self.resolver()
        values = {}
        for rid in self._inputs.keys():
            for token in resolver.value(rid) or ():
                values.setdefault(token, []).append(rid)
        return values

    def uniqueValues(self, name=None, withLengths=0):
        if name is not None and name != self.id:
            return
        for token, rids in sorted(self._values().items()):
            yield (token, len(rids)) if withLengths else token

    def documentToKeyMap(self):
        resolver = self.resolver()
        return {rid: sorted(resolver.value(rid)) for rid in self._inputs.keys()}

    def items(self):
        return [(token, IISet(rids)) for token, rids in sorted(self._values().items())]

    def query_index(self, record, resultset=None):
        keys = record.keys
        if (
            not keys
            or record.get("not", None)
            or record.get("range", None)
            or record.get("usage", None)
            or not all(k is None or isinstance(k, str) for k in keys)
        ):
            return self._stock_query(record, resultset)
        tokens = frozenset(k for k in keys if k is not None)
        every = record.operator == "and"
        if not tokens:
            # `and` with no key gives the result set, `or` no entry.
            return resultset if every else IISet()
        resolver = self.resolver()
        candidates = self._inputs.keys() if resultset is None else resultset
        found = []
        for rid in candidates:
            value = resolver.value(rid)
            if value is None:
                continue
            if every:
                if tokens <= value:
                    found.append(rid)
            elif not value.isdisjoint(tokens):
                found.append(rid)
        return IISet(found)

    def _stock_query(self, record, resultset):
        """Answer `record` as a `KeywordIndex` of the values of every entry
        answers it."""
        stock = KeywordIndex(self.id)
        resolver = self.resolver()
        for rid in self._inputs.keys():
            value = resolver.value(rid)
            if value:
                for token in value:
                    stock.insertForwardIndexEntry(token, rid)
                stock._unindex[rid] = list(value)
        # No request cache: the values belong to this snapshot only.
        stock.getRequestCache = lambda: None
        return stock.__of__(self._catalog()).query_index(record, resultset)


def security_index(catalog):
    """Return the `ZoeSecurityIndex` of the catalog tool `catalog`, wrapped
    in its catalog, or None."""
    inner = getattr(aq_base(catalog), "_catalog", None)
    if inner is None:
        return None
    index = inner.indexes.get(INDEX)
    if not isinstance(index, ZoeSecurityIndex):
        return None
    return index.__of__(catalog._catalog)


def _read_current(catalog):
    """Make the commit of the transaction fail when a change of the index
    `allowedRolesAndUsers` of `catalog` commits first."""
    index = aq_base(catalog._catalog.indexes.get(INDEX))
    counter = getattr(index, "_counter", None)
    jar = getattr(counter, "_p_jar", None)
    if jar is not None:
        jar.readCurrent(counter)


def replace_index(catalog):
    """Put a `ZoeSecurityIndex` in the place of the index
    `allowedRolesAndUsers` of the catalog tool `catalog`, and index every
    entry. Return False when the catalog holds one already."""
    if security_index(catalog) is not None:
        return False
    if INDEX in catalog.indexes():
        _read_current(catalog)
        catalog.delIndex(INDEX)
    catalog.addIndex(INDEX, ZoeSecurityIndex(INDEX))
    catalog.reindexIndex(INDEX, None)
    return True


def _restore_index(catalog):
    """Put the stock `KeywordIndex` back in the place of the
    `ZoeSecurityIndex` of the catalog tool `catalog`, and index every
    entry. Return False when the catalog holds no `ZoeSecurityIndex`."""
    if security_index(catalog) is None:
        return False
    _read_current(catalog)
    catalog.delIndex(INDEX)
    catalog.addIndex(INDEX, "KeywordIndex")
    catalog.reindexIndex(INDEX, None)
    return True


# The stock methods of CMFCore while the backend is installed.
_stock = {}
# The stock methods of CMFCore, kept after `uninstall()`: a request that
# runs a replaced method while `uninstall()` runs calls them.
_originals = {}
# The lock of `install()` and `uninstall()`.
_install_lock = threading.Lock()


def _uses_index(obj, catalog):
    """Return the `ZoeSecurityIndex` of `catalog` when the security
    reindex of `obj` can use it, else None."""
    if catalog is None:
        return None
    index = security_index(catalog)
    if index is None:
        return None
    if tuple(getattr(obj, "_cmf_security_indexes", ())) != (INDEX,):
        return None
    return index


def reindexObjectSecurity(self, skip_self=False):
    """Reindex the security of this object.

    With a `ZoeSecurityIndex`, the entries below this object resolve
    through its entry, so the object alone gets a reindex, and each opaque
    entry below it. When the object gives an opaque entry, every entry
    below it gets a reindex. The stock method runs when the catalog has no
    `ZoeSecurityIndex`, when `_cmf_security_indexes` names another index,
    and for an object that the catalog does not hold: the entries below it
    hold its inputs in their chains.
    """
    catalog = self._getCatalogTool()
    index = _uses_index(self, catalog)
    path = _path(self)
    if index is None or catalog._catalog.uids.get(path) is None:
        return _originals["reindexObjectSecurity"](self, skip_self)
    if not skip_self:
        self.reindexObject(idxs=[INDEX], update_metadata=0)
    every = index.opaque(self)
    if not every and not index._opaque:
        return None
    paths = catalog._catalog.paths
    prefix = path + "/"
    if every:
        below = [b.getPath() for b in catalog.unrestrictedSearchResults(path=path)]
    else:
        below = [paths.get(rid) for rid in index._opaque.keys()]
    for found in sorted(p for p in below if p and p.startswith(prefix)):
        try:
            obj = self.unrestrictedTraverse(found[len(prefix) :])
        except (AttributeError, KeyError):
            # The stock method skips an inconsistent entry too.
            continue
        if "/".join(obj.getPhysicalPath()) != found:
            continue
        changed = getattr(obj, "_p_changed", 0)
        obj.reindexObject(idxs=[INDEX], update_metadata=0)
        if changed is None:
            obj._p_deactivate()
    return None


def deleteLocalRoles(self, obj, member_ids, reindex=1, recursive=0, REQUEST=None):
    """Delete the local roles of `member_ids` on `obj`, and below it when
    `recursive` is true.

    The stock method of CMFCore relies on the walk of
    `reindexObjectSecurity` of `obj`. With a `ZoeSecurityIndex`, this method
    reindexes, when `reindex` is true, each object whose local roles it
    changes.
    """
    from Products.CMFCore.utils import getToolByName

    stock = _originals["deleteLocalRoles"]
    catalog = getToolByName(obj, "portal_catalog", None)
    if catalog is None or security_index(catalog) is None:
        return stock(self, obj, member_ids, reindex, recursive, REQUEST)
    _delete(self, stock, obj, member_ids, reindex, recursive, REQUEST)
    return None


def _delete(tool, stock, obj, member_ids, reindex, recursive, REQUEST):
    def held():
        return [m for m in member_ids if obj.get_local_roles_for_userid(userid=m)]

    before = held()
    stock(tool, obj, member_ids, 0, 0, REQUEST)
    if reindex and before != held() and hasattr(aq_base(obj), "reindexObjectSecurity"):
        obj.reindexObjectSecurity()
    if recursive and hasattr(aq_base(obj), "contentValues"):
        for child in obj.contentValues():
            _delete(tool, stock, child, member_ids, reindex, 1, REQUEST)


# The root database of the process, from `IDatabaseOpenedWithRoot`.
_root = {}
# The threads that put the index in, and their lock.
_threads = []
_threads_lock = threading.Lock()


def installed():
    """Tell if the backend is installed."""
    return bool(_stock)


def _handlers():
    """Return the handlers of the backend, as pairs `(handler, required)`
    for the global registry."""
    from Products.CMFCore.interfaces import ISiteRoot
    from zope.lifecycleevent.interfaces import IObjectAddedEvent
    from zope.processlifetime import IDatabaseOpenedWithRoot

    return [
        (database_opened, (IDatabaseOpenedWithRoot,)),
        (site_added, (ISiteRoot, IObjectAddedEvent)),
    ]


def install():
    """Replace `reindexObjectSecurity` and `deleteLocalRoles` of CMFCore
    in the process, and put a `ZoeSecurityIndex` into each catalog of the
    databases that the process has open, in a thread. A second call does
    nothing."""
    from AccessControl.requestmethod import postonly
    from Products.CMFCore.CMFCatalogAware import CatalogAware
    from Products.CMFCore.MembershipTool import MembershipTool
    from zope.component import getGlobalSiteManager

    with _install_lock:
        if _stock:
            return
        stock = {
            "reindexObjectSecurity": CatalogAware.reindexObjectSecurity,
            "deleteLocalRoles": MembershipTool.deleteLocalRoles,
        }
        _originals.update(stock)
        _stock.update(stock)
        CatalogAware.reindexObjectSecurity = reindexObjectSecurity
        # The stock method takes POST only, through the web.
        MembershipTool.deleteLocalRoles = postonly(deleteLocalRoles)
        registry = getGlobalSiteManager()
        for handler, required in _handlers():
            registry.registerHandler(handler, required)
    db = _root_db()
    if db is not None:
        _start(db)


def uninstall():
    """Give CMFCore its own methods back in the process, then put the stock
    `KeywordIndex` back into each catalog of the databases that the process
    has open, each in a transaction of its own. A second call does
    nothing. A request that runs a replaced method meanwhile ends with
    the stock method."""
    from Products.CMFCore.CMFCatalogAware import CatalogAware
    from Products.CMFCore.MembershipTool import MembershipTool
    from zope.component import getGlobalSiteManager

    with _install_lock:
        if not _stock:
            return
        registry = getGlobalSiteManager()
        for handler, required in _handlers():
            registry.unregisterHandler(handler, required)
        CatalogAware.reindexObjectSecurity = _stock["reindexObjectSecurity"]
        MembershipTool.deleteLocalRoles = _stock["deleteLocalRoles"]
        # `_originals` keeps the stock methods for the calls in flight.
        _stock.clear()
    _join()
    db = _root_db()
    if db is None:
        return
    for path in _sites(db):
        _swap(db, path, _restore_index)


def _root_db():
    """The root database of the process: from the event of this module, or
    from the events of the other backends."""
    from zoe.plone import catalog, indexing

    for each in (_root, catalog._root, indexing._root):
        db = each.get("db")
        if db is not None:
            return db
    return None


def database_opened(event):
    """Put the index into each catalog of the databases of the event, in a
    thread, when the backend is installed."""
    _root["db"] = event.database
    if _stock:
        _start(event.database)


def _start(db):
    thread = threading.Thread(
        target=_install_all,
        args=(db,),
        name="zoe.plone.security install",
        daemon=True,
    )
    with _threads_lock:
        _threads.append(thread)
    thread.start()


def _join(timeout=None):
    """Wait for the threads that put the index in."""
    with _threads_lock:
        threads = list(_threads)
    for thread in threads:
        thread.join(timeout)
    with _threads_lock:
        _threads[:] = [t for t in _threads if t.is_alive()]


def _install_all(db):
    try:
        paths = _sites(db)
    except Exception:
        logger.exception("security index: the search of the sites failed")
        return
    for path in paths:
        if not _stock:
            return
        _swap(db, path, replace_index)


def _sites(db):
    """Return the paths of the sites of the root database `db` whose
    catalog is in a writable database with KV namespaces: at the top of
    the Zope root, and at or just below each mount point."""
    from Products.CMFCore.interfaces import ISiteRoot

    from zoe.plone.indexing import _mount_paths

    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        app = connection.root()["Application"]
        candidates = list(app.objectValues())
        for path, _ in _mount_paths():
            try:
                mounted = app.unrestrictedTraverse(path)
            except (KeyError, AttributeError):
                continue
            candidates.append(mounted)
            values = getattr(mounted, "objectValues", None)
            if values is not None:
                candidates.extend(values())
        out = []
        for obj in candidates:
            base = aq_base(obj)
            if not ISiteRoot.providedBy(base):
                continue
            tool = getattr(base, "portal_catalog", None)
            if tool is None or not _writable(aq_base(tool)._p_jar):
                continue
            path = obj.getPhysicalPath()
            if path not in out:
                out.append(path)
        return out
    finally:
        manager.abort()
        connection.close()


def _writable(jar):
    """Tell if the connection `jar` is on a writable storage with KV
    namespaces."""
    if jar is None or jar.db().storage.isReadOnly():
        return False
    return zoe.kv._supported(jar)


class _Lease:
    """The lease of a site of queued indexing (ADR 0047), which the thread
    that puts the index in holds. A thread of its own writes it again at
    least every `LEASE_EVERY` seconds, in a commit of its own."""

    def __init__(self, catalog_db, oid):
        from zoe.plone.indexing import LEASE, LEASE_TTL

        self._db = catalog_db
        self._oid = oid
        self._name = LEASE
        self._ttl = LEASE_TTL
        self._token = os.urandom(16)
        self._stop = threading.Event()
        self._thread = None

    def _write(self, delete=False):
        manager = transaction.TransactionManager()
        connection = self._db.open(transaction_manager=manager)
        try:
            manager.begin()
            lease = zoe.kv.Namespace(connection, "ephemeral", self._name)
            current = lease.get(self._oid)
            if delete:
                if current == self._token:
                    lease.delete(self._oid)
                    manager.commit()
                return True
            if current is not None and current != self._token:
                return False
            lease.set(self._oid, self._token, ttl=self._ttl)
            manager.commit()
            # The last writer wins: read the lease again.
            manager.begin()
            return lease.get(self._oid) == self._token
        finally:
            manager.abort()
            connection.close()

    def take(self, wait=600.0):
        """Take the lease, waiting while another thread holds it. Return
        True when this lease holds it."""
        deadline = time.monotonic() + wait
        while not self._write():
            if time.monotonic() > deadline:
                return False
            time.sleep(0.5)
        self._thread = threading.Thread(
            target=self._renew, name="zoe.plone.security lease", daemon=True
        )
        self._thread.start()
        return True

    def _renew(self):
        while not self._stop.wait(LEASE_EVERY):
            try:
                self._write()
            except Exception:
                logger.exception("security index: the lease was not written")

    def give_back(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join()
        try:
            self._write(delete=True)
        except Exception:
            logger.debug("security index: no lease given back", exc_info=True)


def _swap(db, path, change):
    """Run `change` (`replace_index` or `_restore_index`) on the catalog of
    the site at `path` of the root database `db`, in a transaction of its
    own, under the lease of the site. Retry it after a conflict. Take no
    lease when the catalog needs no change."""
    from zoe.plone.indexing import _Context

    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    context = _Context()
    lease = None
    try:
        app = context.enter(connection)
        site = app.unrestrictedTraverse(path)
        context.site(site)
        tool = aq_base(site.portal_catalog)
        jar = tool._p_jar
        if not _writable(jar):
            return
        holds = security_index(site.portal_catalog) is not None
        if holds == (change is replace_index):
            # The catalog holds the index that `change` gives it already.
            return
        lease = _Lease(jar.db(), tool._p_oid)
        if not lease.take():
            logger.warning(
                "security index: the lease of %s stays held; the index of"
                " the site does not change",
                "/".join(path),
            )
            lease = None
            return
        for attempt in range(TRIES):
            manager.begin()
            try:
                site = app.unrestrictedTraverse(path)
                if not change(site.portal_catalog):
                    manager.abort()
                    return
                txn = manager.get()
                txn.note(f"zoe.plone: security index of {'/'.join(path)}")
                txn.setUser("System Processes", "")
                manager.commit()
                return
            except ConflictError:
                manager.abort()
                if attempt == TRIES - 1:
                    raise
    except Exception:
        manager.abort()
        logger.exception(
            "security index: the index of %s did not change", "/".join(path)
        )
    finally:
        manager.abort()
        if lease is not None:
            lease.give_back()
        context.exit()
        connection.close()


def site_added(site, event):
    """Put the index into the catalog of a new site, in the transaction
    that creates it, before its commit."""
    if not _stock or event.oldParent is not None:
        return
    transaction.get().addBeforeCommitHook(_new_site, (site,))


def _new_site(site):
    tool = getattr(site, "portal_catalog", None)
    if tool is None:
        return
    jar = aq_base(tool)._p_jar or aq_base(site)._p_jar
    if jar is not None and not _writable(jar):
        return
    if jar is None:
        # The site is in no database yet: the storage of its parent decides.
        parent = aq_parent(aq_inner(site))
        jar = getattr(aq_base(parent), "_p_jar", None)
        if not _writable(jar):
            return
    replace_index(tool)
