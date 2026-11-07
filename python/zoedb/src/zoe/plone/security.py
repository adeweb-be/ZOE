"""The security index of `zoe.plone`: `allowedRolesAndUsers` with inherited inputs.

`ZoeSecurityIndex` takes the place of the `KeywordIndex`
`allowedRolesAndUsers` of a Plone catalog. The indexer of Plone computes the
value of an object from two inputs: the roles of the permission "Access
contents information" (`rolesForPermissionOn`), and the local roles of the
object and of its containers (`_getAllLocalRoles`). Each input reads the
object and then its containers.

The index keeps, for each entry, only what the object holds itself: its
permission setting, its own local roles and its block flag. A query resolves
the value of each entry through the paths of the catalog, as the indexer
computes it. So a change of security on a folder changes the entry of the
folder alone, and `reindexObjectSecurity` reindexes the folder alone.

An entry is opaque when its object or one of its containers has an input
that the index does not read. An opaque entry keeps the value of the
indexer whole, and `reindexObjectSecurity` of a container reindexes it.
"""

from Acquisition import aq_base, aq_inner, aq_parent
from BTrees.IIBTree import IISet, IITreeSet
from BTrees.IOBTree import IOBTree
from BTrees.Length import Length
from BTrees.OOBTree import OOBTree
from Products.PluginIndexes.KeywordIndex.KeywordIndex import KeywordIndex

PERMISSION = "Access contents information"
ATTRIBUTE = "_Access_contents_information_Permission"
INDEX = "allowedRolesAndUsers"
# The roles of rolesForPermissionOn when no object of the chain sets them.
DEFAULT_ROLES = frozenset({"Manager"})

# The kinds of a permission setting.
MISSING = ""  # no setting: the roles of the container
TUPLE = "t"  # these roles only
LIST = "l"  # these roles and the roles of the container
NONE = "a"  # the setting None: Anonymous
OPAQUE = "v"  # the whole value, from a provider that the index cannot read

ANONYMOUS = "anonymous"


def _default_adapter():
    from borg.localrole.default_adapter import DefaultLocalRoleAdapter

    return DefaultLocalRoleAdapter


def inputs_of(obj):
    """Return the inputs that `obj` holds itself, or None when the index
    does not read them.

    The inputs are a tuple `(kind, roles, local, block)`: the kind of the
    permission setting, its roles, the local roles of the object as sorted
    pairs `(principal, roles)`, and the block flag. The setting follows
    `rolesForPermissionOn`: a str names another permission, and the index
    does not read it.
    """
    from borg.localrole.bbb.interfaces import IGroupAwareWorkspace, IWorkspace
    from borg.localrole.interfaces import ILocalRoleProvider
    from zope.component import getAdapters

    base = aq_base(obj)
    roles = getattr(base, ATTRIBUTE, None)
    if not hasattr(base, ATTRIBUTE):
        kind, roles = MISSING, ()
    elif roles is None:
        kind, roles = NONE, ()
    elif type(roles) is tuple:
        kind, roles = TUPLE, tuple(sorted(roles))
    elif isinstance(roles, str):
        return None
    else:
        try:
            kind, roles = LIST, tuple(sorted(roles))
        except TypeError:
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
            local.setdefault(principal, set()).update(granted)
    pairs = tuple(sorted((p, tuple(sorted(r))) for p, r in local.items() if r))
    block = bool(getattr(obj, "__ac_local_roles_block__", None))
    return (kind, roles, pairs, block)


def _stock_entry(obj, catalog_tool):
    """Return True when the indexer of Plone and its stock local role
    plugin give the value of the entry of `obj`.

    `obj` is the object that the catalog indexes, which may be the wrapper
    of plone.indexer.
    """
    from plone.indexer.interfaces import IIndexer
    from Products.CMFCore.utils import getToolByName
    from Products.CMFPlone.CatalogTool import allowedRolesAndUsers
    from Products.PlonePAS.interfaces.plugins import ILocalRolesPlugin
    from zope.component import queryMultiAdapter

    unwrap = getattr(obj, "_getWrappedObject", None)
    if unwrap is None:
        return False
    target = unwrap()
    found = queryMultiAdapter((target, catalog_tool), IIndexer, name=INDEX)
    if getattr(found, "callable", None) is not allowedRolesAndUsers.callable:
        return False
    acl_users = getToolByName(target, "acl_users", None)
    plugins = getattr(aq_base(acl_users), "plugins", None)
    if plugins is None:
        return False
    from borg.localrole.workspace import WorkspaceLocalRoleManager

    active = acl_users.plugins.listPlugins(ILocalRolesPlugin)
    return len(active) == 1 and isinstance(
        aq_base(active[0][1]), WorkspaceLocalRoleManager
    )


def stock_value(obj):
    """Return the value of the indexer of Plone for `obj`, sorted."""
    from Products.CMFPlone.CatalogTool import allowedRolesAndUsers

    return tuple(sorted(allowedRolesAndUsers(obj)()))


def _parent(path):
    if not path:
        return None
    return path.rpartition("/")[0]


def _path(obj):
    return "/".join(obj.getPhysicalPath())


class Resolver:
    """Resolve the values of the entries of one index, with a memo."""

    def __init__(self, index, paths, uids):
        self._inputs = index._inputs
        self._outer = index._outer
        self._paths = paths
        self._uids = uids
        self._roles = {}
        self._local = {}
        self._closed = {}

    def inputs(self, path):
        """Return the inputs at `path`, or None for a path that is neither
        an entry nor a recorded container."""
        rid = self._uids.get(path)
        if rid is not None:
            found = self._inputs.get(rid)
            if found is not None:
                return found
        return self._outer.get(path)

    def closed(self, path):
        """Return True when `path` or a container of it is opaque or
        unknown. An entry below such a path gives no token."""
        if path is None:
            return False
        memo = self._closed
        if path in memo:
            return memo[path]
        found = self.inputs(path)
        if found is None or found[0] == OPAQUE:
            result = True
        else:
            result = self.closed(_parent(path))
        memo[path] = result
        return result

    def roles(self, path):
        """Return the roles of the permission at `path`: a frozenset,
        ANONYMOUS, or None when no object of the chain sets them."""
        if path is None:
            return None
        memo = self._roles
        if path in memo:
            return memo[path]
        kind, roles, _local, _block = self.inputs(path)
        if kind == TUPLE:
            found = frozenset(roles)
        elif kind == NONE:
            found = ANONYMOUS
        elif kind == MISSING:
            found = self.roles(_parent(path))
        else:
            above = self.roles(_parent(path))
            if above == ANONYMOUS:
                found = ANONYMOUS
            elif above is None:
                found = frozenset(roles) if roles else None
            else:
                found = frozenset(roles) | above
        memo[path] = found
        return found

    def local(self, path):
        """Return the local roles at `path`: a dict of frozensets."""
        if path is None:
            return {}
        memo = self._local
        if path in memo:
            return memo[path]
        _kind, _roles, local, block = self.inputs(path)
        own = {principal: frozenset(roles) for principal, roles in local}
        if block:
            found = own
        else:
            above = self.local(_parent(path))
            if not own:
                found = above
            else:
                found = dict(above)
                for principal, roles in own.items():
                    found[principal] = found.get(principal, frozenset()) | roles
        memo[path] = found
        return found

    def value_at(self, path):
        """Return the value at `path` as the indexer of Plone computes it."""
        found = self.inputs(path)
        if found is None:
            return frozenset()
        kind, roles, _local, _block = found
        if kind == OPAQUE:
            return frozenset(roles)
        if self.closed(_parent(path)):
            return frozenset()
        found = self.roles(path)
        if found == ANONYMOUS:
            allowed = {"Anonymous"}
        elif found is None:
            allowed = set(DEFAULT_ROLES)
        else:
            allowed = set(found)
        if "Anonymous" in allowed:
            return frozenset({"Anonymous"})
        if "Authenticated" in allowed:
            return frozenset({"Authenticated"})
        for principal, granted in self.local(path).items():
            if not allowed.isdisjoint(granted):
                allowed.add("user:" + principal)
        allowed.discard("Owner")
        return frozenset(allowed)

    def value(self, rid):
        return self.value_at(self._paths[rid])


class ZoeSecurityIndex(KeywordIndex):
    """`allowedRolesAndUsers` with the inputs of each entry, resolved at
    query time through the paths of the catalog."""

    meta_type = "ZoeSecurityIndex"
    operators = ("or", "and")
    useOperator = "or"

    def clear(self):
        self._length = Length()
        self._index = OOBTree()
        self._unindex = IOBTree()
        # rid -> inputs of the object of the entry.
        self._inputs = IOBTree()
        # path -> inputs of a container outside the catalog.
        self._outer = OOBTree()
        # The rids whose inputs are their whole value.
        self._opaque = IITreeSet()
        if self._counter is None:
            self._counter = Length()
        else:
            self._increment_counter()

    def _catalog(self):
        return aq_parent(aq_inner(self))

    def resolver(self):
        catalog = self._catalog()
        return Resolver(self, catalog.paths, catalog.uids)

    def index_object(self, documentId, obj, threshold=None):
        target = obj
        unwrap = getattr(obj, "_getWrappedObject", None)
        if unwrap is not None:
            target = unwrap()
        self.record_outer(target)
        inputs = None
        if _stock_entry(obj, self._tool()):
            if not self._below_opaque(_path(target)):
                inputs = inputs_of(target)
        if inputs is None:
            value = tuple(sorted(self._get_object_keywords(obj, INDEX)))
            inputs = (OPAQUE, value, (), False)
        changed = self._inputs.get(documentId) != inputs
        if changed:
            self._inputs[documentId] = inputs
            if inputs[0] == OPAQUE:
                self._opaque.insert(documentId)
            elif documentId in self._opaque:
                self._opaque.remove(documentId)
            self._increment_counter()
        return changed

    def _tool(self):
        return aq_parent(aq_inner(self._catalog()))

    def opaque(self, obj):
        """Return True when `obj` gives an opaque entry. The answer reads
        the object and the entries of its containers, not the entry of the
        object, which a queue may not have applied yet."""
        from plone.indexer.interfaces import IIndexableObject
        from zope.component import queryMultiAdapter

        tool = self._tool()
        wrapper = queryMultiAdapter((obj, tool), IIndexableObject, default=obj)
        if not _stock_entry(wrapper, tool):
            return True
        if self._below_opaque(_path(obj)):
            return True
        return inputs_of(obj) is None

    def _below_opaque(self, path):
        """Return True when a container of `path` is opaque."""
        uids = self._catalog().uids
        above = _parent(path)
        while above is not None:
            rid = uids.get(above)
            if rid is not None:
                if rid in self._opaque:
                    return True
            else:
                found = self._outer.get(above)
                if found is not None and found[0] == OPAQUE:
                    return True
            above = _parent(above)
        return False

    def record_outer(self, obj):
        """Keep the inputs of the containers of `obj` that are outside the
        catalog, up to the first container in the catalog."""
        uids = self._catalog().uids
        current = aq_parent(aq_inner(obj))
        while current is not None and hasattr(aq_base(current), "getPhysicalPath"):
            path = _path(current)
            if uids.get(path) is not None:
                return
            self.record(path, current)
            current = aq_parent(aq_inner(current))

    def record(self, path, obj):
        """Keep the inputs of `obj`, which is outside the catalog."""
        inputs = inputs_of(obj) or (OPAQUE, (), (), False)
        if self._outer.get(path) != inputs:
            self._outer[path] = inputs
            self._increment_counter()

    def unindex_object(self, documentId):
        if self._inputs.get(documentId) is None:
            return None
        del self._inputs[documentId]
        if documentId in self._opaque:
            self._opaque.remove(documentId)
        self._increment_counter()

    def getEntryForObject(self, documentId, default=None):
        if self._inputs.get(documentId) is None:
            return default
        return sorted(self.resolver().value(documentId))

    def numObjects(self):
        return len(self._inputs)

    def indexSize(self):
        # The query plan of ZCatalog asks it of an index outside the
        # catalog, so it cannot resolve.
        return len(self._inputs)

    def _values(self):
        resolver = self.resolver()
        values = {}
        for rid in self._inputs.keys():
            for token in resolver.value(rid):
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
        tokens = frozenset(record.keys or ())
        excluded = frozenset(record.get("not", None) or ())
        if not tokens and not excluded:
            return IISet()
        resolver = self.resolver()
        candidates = self._inputs.keys() if resultset is None else resultset
        every = record.operator == "and"
        found = []
        for rid in candidates:
            if self._inputs.get(rid) is None:
                continue
            value = resolver.value(rid)
            if tokens:
                if every:
                    if not tokens <= value:
                        continue
                elif value.isdisjoint(tokens):
                    continue
            if excluded and not value.isdisjoint(excluded):
                continue
            found.append(rid)
        return IISet(found)


def security_index(catalog):
    """Return the `ZoeSecurityIndex` of `catalog`, or None."""
    inner = getattr(aq_base(catalog), "_catalog", None)
    if inner is None:
        return None
    index = inner.indexes.get(INDEX)
    if not isinstance(index, ZoeSecurityIndex):
        return None
    return index.__of__(catalog._catalog)


def replace_index(catalog):
    """Put a `ZoeSecurityIndex` in place of the index `allowedRolesAndUsers`
    of `catalog`, and index every entry."""
    if INDEX in catalog.indexes():
        catalog.delIndex(INDEX)
    catalog.addIndex(INDEX, ZoeSecurityIndex(INDEX))
    catalog.reindexIndex(INDEX, None)


def restore_index(catalog):
    """Put the stock `KeywordIndex` back, and index every entry."""
    catalog.delIndex(INDEX)
    catalog.addIndex(INDEX, "KeywordIndex")
    catalog.reindexIndex(INDEX, None)


_stock = None


def reindexObjectSecurity(self, skip_self=False):
    """Reindex the security of this object.

    With a `ZoeSecurityIndex`, the entries below this object resolve
    through it, so only this object and the opaque entries below it get a
    reindex. When the entry of this object is opaque, every entry below it
    gets a reindex. Otherwise the stock method of CMFCore runs.
    """
    catalog = self._getCatalogTool()
    if catalog is None:
        return None
    index = security_index(catalog)
    if index is None or tuple(self._cmf_security_indexes) != (INDEX,):
        return _stock(self, skip_self)
    path = _path(self)
    if _is_site(self) and catalog._catalog.uids.get(path) is None:
        index.record(path, self)
        every = index._outer[path][0] == OPAQUE
    else:
        if not skip_self:
            self.reindexObject(idxs=[INDEX], update_metadata=0)
        every = index.opaque(self)
    if every or index._opaque:
        below = catalog.unrestrictedSearchResults(path=path)
        for brain in below:
            if brain.getPath() == path:
                continue
            if every or brain.getRID() in index._opaque:
                obj = brain._unrestrictedGetObject()
                obj.reindexObject(idxs=[INDEX], update_metadata=0)
    return None


_stock_delete = None


def deleteLocalRoles(self, obj, member_ids, reindex=1, recursive=0, REQUEST=None):
    """Delete the local roles of `member_ids` on `obj`, and below it when
    `recursive` is true.

    The stock method of CMFCore relies on the walk of
    `reindexObjectSecurity` of `obj`. With a `ZoeSecurityIndex`, this method
    reindexes each object whose local roles it changes.
    """
    from Products.CMFCore.utils import getToolByName

    catalog = getToolByName(obj, "portal_catalog", None)
    if catalog is None or security_index(catalog) is None:
        return _stock_delete(self, obj, member_ids, reindex, recursive, REQUEST)
    held = any(obj.get_local_roles_for_userid(userid=m) for m in member_ids)
    _stock_delete(self, obj, member_ids, 0, 0, REQUEST)
    if held and hasattr(aq_base(obj), "reindexObjectSecurity"):
        obj.reindexObjectSecurity()
    if recursive and hasattr(aq_base(obj), "contentValues"):
        for child in obj.contentValues():
            deleteLocalRoles(self, child, member_ids, 0, 1, REQUEST)
    return None


def _is_site(obj):
    from Products.CMFCore.interfaces import ISiteRoot

    return ISiteRoot.providedBy(obj)


def install():
    """Use `reindexObjectSecurity` and `deleteLocalRoles` of this module in
    CMFCore."""
    global _stock
    from Products.CMFCore.CMFCatalogAware import CatalogAware

    global _stock_delete
    from Products.CMFCore.MembershipTool import MembershipTool

    if _stock is None:
        _stock = CatalogAware.reindexObjectSecurity
        CatalogAware.reindexObjectSecurity = reindexObjectSecurity
    if _stock_delete is None:
        _stock_delete = MembershipTool.deleteLocalRoles
        MembershipTool.deleteLocalRoles = deleteLocalRoles


def uninstall():
    """Give CMFCore its own methods back."""
    global _stock
    from Products.CMFCore.CMFCatalogAware import CatalogAware

    global _stock_delete
    from Products.CMFCore.MembershipTool import MembershipTool

    if _stock is not None:
        CatalogAware.reindexObjectSecurity = _stock
        _stock = None
    if _stock_delete is not None:
        MembershipTool.deleteLocalRoles = _stock_delete
        _stock_delete = None
