"""The records of plone.registry in the durable namespace of the store.

`portal_registry` keeps its records in two OOBTrees of the site, `_fields`
and `_values`, inside `Registry._records`. With this backend, a registry
keeps them in the durable namespace `zoe.plone.registry` of its store
instead: one row for the field of each record, under `f`, a NUL byte and the
name of the record, and one for its value, under `v`, a NUL byte and the
name. A row holds a pickle of the field or of the value. A field reference
of plone.registry is the name of the record that it points to, as in the
stock tree. The keys are the UTF-8 bytes of the names, so the records come
in the order of the stock trees.

The API of the registry does not change: `KVRecords` is a subclass of the
records of plone.registry whose two trees are views of the namespace.
`install()` gives them to each new `Registry`. An existing registry keeps
its trees until `use_namespace(site)` moves its records into the namespace,
in the current transaction: an upgrade step or a script runs it, never a
read. `use_object_graph(site)` moves the records back into the trees of the
registry, before a copy of the site through the ZODB iterator or a switch
off of the backend, and `uninstall(site)` runs it.

A new registry has no connection yet, so its views keep the records in the
registry itself first. Its first write on a connection whose storage has
namespaces moves them all into the namespace, in the transaction of that
write. A read never moves them: until a write, or `use_namespace(site)`,
the registry reads them from itself, on any storage. Once the records are
in the namespace, a connection whose storage has no namespaces, such as a
copy of the site in a Data.fs, raises `Unsupported` at each use of the
records: the rows are not there, and the registry never reads as empty.

Each write of a record also marks the registry as changed, so the commit
writes the small registry object too. plone.protect then sees the write of
a control panel and checks its CSRF token, and two editors who change the
registry at the same moment conflict, as with the stock trees.

A read gives the last committed value, and a transaction reads its own
writes first. The rows of a store are cached for each storage of the
process. Each transaction that reads the records reads the generation of
the namespace once, one round trip with `zoe.zeo`, and it reads all the
rows again only when the generation changed. A transaction keeps the rows
that it read first, so it sees one state of the records to its end. Each
read unpickles a new copy of its value, so a caller that changes a value in
place changes only its copy, as with the trees of a connection.
"""

import pickle
import threading
import weakref

import zoe.kv

__all__ = [
    "NAMESPACE",
    "install",
    "installed",
    "uninstall",
    "use_namespace",
    "use_object_graph",
]

# The name of the namespace of the records.
NAMESPACE = "zoe.plone.registry"

# The prefixes of the keys of the fields and of the values.
FIELD = b"f\x00"
VALUE = b"v\x00"

# The pairs that one scan takes.
_SCAN = 1000

# The scans of all the rows that one read of the generation allows, when
# commits change the namespace during the scans.
_ATTEMPTS = 5

# The stock method while the backend is installed.
_stock = {}

# The committed rows of each storage: storage -> (generation, {key: bytes}).
_cache = weakref.WeakKeyDictionary()
_cache_lock = threading.Lock()

# The rows that the current transaction of each connection read first:
# connection -> (transaction, {key: bytes}).
_views = weakref.WeakKeyDictionary()


def installed():
    """Tell if the backend is installed."""
    return bool(_stock)


def install():
    """Give each new registry its records in the durable namespace of its
    store. A second call does nothing."""
    if _stock:
        return
    from plone.registry.registry import Registry

    _stock["__init__"] = Registry.__init__
    kv_records = _records_class()

    def __init__(self):
        self._records = kv_records(self)

    Registry.__init__ = __init__


def uninstall(site=None):
    """Put the stock method back. With `site`, a Plone site or a registry,
    first move its records back into its own trees, in the current
    transaction. A registry that keeps its records in the namespace keeps
    them there without `site`."""
    if site is not None:
        use_object_graph(site)
    if not _stock:
        return
    from plone.registry.registry import Registry

    Registry.__init__ = _stock.pop("__init__")


def _registry_of(site):
    """Return the registry of `site`, a Plone site or a registry."""
    from plone.registry.registry import Registry

    if isinstance(site, Registry):
        return site
    return site.portal_registry


def use_namespace(site):
    """Move the records of the registry of `site`, a Plone site or a
    registry, into the durable namespace of its store, in the current
    transaction. A registry whose records are there already does not
    change. A storage without namespaces raises `Unsupported`."""
    from plone.registry.registry import _Records

    registry = _registry_of(site)
    # The stock property moves the records of an old registry first.
    current = registry.records
    if type(current) is _Records:
        registry._records = _records_class().of(registry, current)
    elif isinstance(current, _records_class()) and current.local():
        current.move()


def use_object_graph(site):
    """Move the records of the registry of `site`, a Plone site or a
    registry, back into its own trees, and delete their rows of the
    namespace, in the current transaction."""
    from plone.registry.registry import _Records

    registry = _registry_of(site)
    # The attribute loads a ghost; `__dict__` of a ghost is empty.
    current = registry._records
    if not isinstance(current, _records_class()):
        return
    records = _Records(registry)
    for name in list(current._fields):
        records._fields[name] = current._fields.raw(name)
    for name in list(current._values):
        records._values[name] = current._values[name]
    current._fields.clear()
    current._values.clear()
    registry._records = records


def _has_namespace(registry):
    """Tell if `registry` has a connection whose storage has namespaces."""
    jar = getattr(registry, "_p_jar", None)
    return jar is not None and zoe.kv._supported(jar)


def _namespace(registry):
    """Return the durable namespace of the store of `registry`. A registry
    without a connection, or on a storage without namespaces, raises
    `Unsupported`."""
    jar = getattr(registry, "_p_jar", None)
    if jar is None or not zoe.kv._supported(jar):
        raise zoe.kv.Unsupported(
            f"the records of this registry are in the KV namespace {NAMESPACE}"
            " of a ZOE store, and this storage has no KV namespaces. Open the"
            " site on its ZOE store, call"
            " zoe.plone.registry.use_object_graph(site) and commit, then copy"
            " the site."
        )
    return zoe.kv.Namespace(jar, "durable", NAMESPACE)


def _committed(namespace):
    """Return the committed rows of `namespace` for the current transaction
    of its connection. The first call of a transaction reads the generation
    and takes the rows from the cache of the storage, or reads them again
    when the generation changed. The later calls of the transaction give
    the same rows."""
    connection = namespace._connection
    txn = connection.transaction_manager.get()
    with _cache_lock:
        view = _views.get(connection)
    if view is not None and view[0] is txn:
        return view[1]
    storage = namespace._storage
    key = getattr(storage, "_demo", storage)
    generation = namespace.generation()
    with _cache_lock:
        cached = _cache.get(key)
    if cached is not None and cached[0] == generation:
        rows = cached[1]
    else:
        rows = _scan(namespace, generation, key)
    with _cache_lock:
        _views[connection] = (txn, rows)
    return rows


def _scan(namespace, generation, key):
    """Read all the rows of `namespace`, page by page, and cache them for
    `key`. A page can be short, so only an empty page ends the scan. The
    pages come from several reads of the store, so the scan reads the
    generation again after them, and scans again when it moved."""
    storage = namespace._storage
    for _ in range(_ATTEMPTS):
        rows = {}
        start = None
        while True:
            pairs = storage.kv_scan("durable", NAMESPACE, start, None, _SCAN)
            if not pairs:
                break
            rows.update(pairs)
            start = pairs[-1][0] + b"\x00"
        after = namespace.generation()
        if after == generation:
            with _cache_lock:
                _cache[key] = (generation, rows)
            return rows
        generation = after
    # The namespace changed during each scan: use the last one, uncached.
    return rows


def _make_records_class():
    from plone.registry.registry import _Records

    class KVRecords(_Records):
        """The records of a registry in the durable namespace of its store.

        Its `_fields` and `_values` are views of the namespace with the
        API of the trees that plone.registry uses.
        """

        def __init__(self, parent, local=True):
            self.__parent__ = parent
            self._fields = _Part(self, FIELD, local)
            self._values = _Part(self, VALUE, local)

        @classmethod
        def of(cls, registry, records):
            """Return the records of `registry` in the namespace, with the
            records of `records`, the stock records of the registry."""
            moved = cls(registry, local=False)
            for name in records._fields:
                moved._fields[name] = records._fields[name]
            for name in records._values:
                moved._values[name] = records._values[name]
            return moved

        def local(self):
            """Tell if the registry keeps the records itself."""
            return self._fields._local is not None

        def move(self):
            """Move the records that the registry keeps itself into the
            namespace, in the current transaction."""
            self._fields.move()
            self._values.move()

    # A pickle names the class by its module and its name, and the module
    # gives it through `__getattr__`.
    KVRecords.__module__ = __name__
    KVRecords.__qualname__ = "KVRecords"
    return KVRecords


class _Part:
    """The fields or the values of a `KVRecords`: a view of the rows of one
    prefix of the namespace, keyed by the names of the records."""

    def __init__(self, records, prefix, local):
        self._records = records
        self._prefix = prefix
        # The rows that the part keeps itself, by name, until they move into
        # the namespace; None after the move.
        self._local = {} if local else None

    def _namespace(self, write=False):
        """Return the namespace of the rows, or None while the part keeps
        them itself. A `write` with a connection whose storage has
        namespaces first moves the rows of the registry there."""
        registry = self._records.__parent__
        if self._local is not None:
            if not (write and _has_namespace(registry)):
                return None
            self._records.move()
        return _namespace(registry)

    def move(self):
        """Move the rows that the part keeps itself into the namespace."""
        if self._local is None:
            return
        namespace = _namespace(self._records.__parent__)
        for name, value in self._local.items():
            namespace.set(self._key(name), pickle.dumps(value, 3))
        self._local = None
        self._changed()

    def _changed(self):
        """Mark the registry as changed: the commit writes it, and
        plone.protect sees the write."""
        self._records.__parent__._p_changed = True

    def _key(self, name):
        return self._prefix + name.encode("utf-8")

    def _rows(self, namespace):
        """Return the encoded rows of the part by name: the committed rows
        with the writes of the current transaction."""
        rows = {}
        for key, value in _committed(namespace).items():
            if key.startswith(self._prefix):
                rows[key[len(self._prefix) :].decode("utf-8")] = value
        for key, value in namespace._writes().items():
            if key.startswith(self._prefix):
                name = key[len(self._prefix) :].decode("utf-8")
                if value is None:
                    rows.pop(name, None)
                else:
                    rows[name] = value
        return rows

    def _encoded(self, namespace, name):
        key = self._key(name)
        found, value = namespace._find(key)
        if found:
            return value
        return _committed(namespace).get(key)

    def raw(self, name):
        """Return the stored object of `name`."""
        return self[name]

    def __getitem__(self, name):
        if not isinstance(name, str):
            raise KeyError(name)
        namespace = self._namespace()
        if namespace is None:
            return self._local[name]
        encoded = self._encoded(namespace, name)
        if encoded is None:
            raise KeyError(name)
        return pickle.loads(encoded)

    def get(self, name, default=None):
        try:
            return self[name]
        except KeyError:
            return default

    def __setitem__(self, name, value):
        namespace = self._namespace(write=True)
        if namespace is None:
            self._local[name] = value
        else:
            namespace.set(self._key(name), pickle.dumps(value, 3))
        self._changed()

    def __delitem__(self, name):
        namespace = self._namespace(write=True)
        if namespace is None:
            del self._local[name]
        else:
            if self._encoded(namespace, name) is None:
                raise KeyError(name)
            namespace.delete(self._key(name))
        self._changed()

    def _names(self):
        namespace = self._namespace()
        if namespace is None:
            return sorted(self._local)
        return sorted(self._rows(namespace), key=lambda name: name.encode("utf-8"))

    def __contains__(self, name):
        if not isinstance(name, str):
            return False
        namespace = self._namespace()
        if namespace is None:
            return name in self._local
        return self._encoded(namespace, name) is not None

    has_key = __contains__

    def __iter__(self):
        return iter(self._names())

    def __len__(self):
        return len(self._names())

    def __bool__(self):
        return bool(self._names())

    __nonzero__ = __bool__

    def keys(self, min=None, max=None):
        return [
            name
            for name in self._names()
            if (min is None or name >= min) and (max is None or name <= max)
        ]

    def values(self, min=None, max=None):
        return [self[name] for name in self.keys(min, max)]

    def items(self, min=None, max=None):
        return [(name, self[name]) for name in self.keys(min, max)]

    def minKey(self, key=None):
        names = self.keys(min=key)
        if not names:
            raise ValueError("empty tree")
        return names[0]

    def maxKey(self, key=None):
        names = self.keys(max=key)
        if not names:
            raise ValueError("empty tree")
        return names[-1]

    def clear(self):
        for name in self._names():
            del self[name]


def __getattr__(name):
    # The class needs plone.registry, which only the `plone` extra gives.
    if name == "KVRecords":
        return _records_class()
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")


_class = []


def _records_class():
    if not _class:
        _class.append(_make_records_class())
    return _class[0]
