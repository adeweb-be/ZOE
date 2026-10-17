"""The records of plone.registry in the durable namespace of the store.

`portal_registry` keeps its records in two OOBTrees of the site, `_fields`
and `_values`, inside `Registry._records`. `install()` keeps them in the
durable namespace `zoe.plone.registry` of the store of the registry
instead: one row for the field of each record, under `f`, a NUL byte and the
name of the record, and one for its value, under `v`, a NUL byte and the
name. A row holds a pickle of the field or of the value. A field reference
of plone.registry is the name of the record that it points to, as in the
stock tree. The keys are the UTF-8 bytes of the names, so the records come
in the order of the stock trees.

The API of the registry does not change: `KVRecords` is a subclass of the
records of plone.registry whose two trees are views of the namespace. A
new `Registry` gets them. An existing registry moves its records into the
namespace at its first use through `records` when its storage has KV
namespaces: that use writes the namespace and the registry once. Before the
registry has a storage, the views keep the records in the registry itself,
and the first use with a storage moves them. `use_object_graph(registry)`
moves the records back into the trees of the registry, before a copy of
the site through the ZODB iterator or a switch off of the backend.

A read gives the last committed value, and a transaction reads its own
writes first. The values of a store are cached for each storage of the
process: a transaction reads the generation of the namespace once, and
reads all its rows again only when the generation changed. Each read
unpickles a new copy of its value, so a caller that changes a value in
place changes only its copy, as with the trees of a connection.

`uninstall()` puts the stock `Registry.__init__` and `Registry.records`
back. A registry whose records are in the namespace keeps them there.
"""

import pickle
import threading
import weakref

__all__ = ["NAMESPACE", "KVRecords", "install", "installed", "uninstall", "use_object_graph"]

# The name of the namespace of the records.
NAMESPACE = "zoe.plone.registry"

# The prefixes of the keys of the fields and of the values.
FIELD = b"f\x00"
VALUE = b"v\x00"

# The pairs that one scan takes.
_SCAN = 1000

# The stock methods while the backend is installed.
_stock = {}

# The committed rows of each storage: storage -> (generation, {key: bytes}).
_cache = weakref.WeakKeyDictionary()
_cache_lock = threading.Lock()

# The transaction in which each connection last read the generation.
_checked = weakref.WeakKeyDictionary()


def installed():
    """Tell if the backend is installed."""
    return bool(_stock)


def install():
    """Keep the records of the registries in the durable namespace of
    their store. A second call does nothing."""
    if _stock:
        return
    from plone.registry.registry import Records, Registry, _Records

    _stock["__init__"] = Registry.__init__
    _stock["records"] = Registry.records

    kv_records = _records_class()

    def __init__(self):
        self._records = kv_records(self)

    def records(self):
        current = self._records
        if isinstance(current, Records):
            self._migrateRecords()
            current = self._records
        if type(current) is _Records and _namespace(self) is not None:
            self._records = current = kv_records.of(self, current)
        return current

    Registry.__init__ = __init__
    Registry.records = property(records, doc=_stock["records"].__doc__)


def uninstall():
    """Put the stock methods back."""
    if not _stock:
        return
    from plone.registry.registry import Registry

    Registry.__init__ = _stock.pop("__init__")
    Registry.records = _stock.pop("records")


def use_object_graph(registry):
    """Move the records of `registry` back into its own trees, and delete
    their rows of the namespace, in the current transaction."""
    from plone.registry.registry import _Records

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


def _namespace(registry):
    """Return the durable namespace of the store of `registry`, or None
    when it has no storage with namespaces."""
    import zoe.kv

    jar = getattr(registry, "_p_jar", None)
    if jar is None or not zoe.kv.supported(jar):
        return None
    return zoe.kv.Namespace(jar, "durable", NAMESPACE)


def _committed(namespace):
    """Return the committed rows of `namespace`, from the cache of its
    storage. The first call of a transaction of the connection reads the
    generation, and the rows again when it changed."""
    storage = namespace._storage
    key = getattr(storage, "_demo", storage)
    connection = namespace._connection
    txn = connection.transaction_manager.get()
    with _cache_lock:
        cached = _cache.get(key)
        fresh = _checked.get(connection) is txn
    if cached is not None and fresh:
        return cached[1]
    generation = namespace.generation()
    if cached is not None and cached[0] == generation:
        with _cache_lock:
            _checked[connection] = txn
        return cached[1]
    rows = {}
    start = None
    while True:
        pairs = storage.kv_scan("durable", NAMESPACE, start, None, _SCAN)
        rows.update(pairs)
        if len(pairs) < _SCAN:
            break
        start = pairs[-1][0] + b"\x00"
    with _cache_lock:
        _cache[key] = (generation, rows)
        _checked[connection] = txn
    return rows


def _make_records_class():
    from plone.registry.registry import _Records

    class KVRecords(_Records):
        """The records of a registry in the durable namespace of its store.

        Its `_fields` and `_values` are views of the namespace with the
        API of the trees that plone.registry uses.
        """

        def __init__(self, parent):
            self.__parent__ = parent
            self._fields = _Part(self, FIELD)
            self._values = _Part(self, VALUE)

        @classmethod
        def of(cls, registry, records):
            """Return the records of `registry` in the namespace, with the
            records of `records`, the stock records of the registry."""
            moved = cls(registry)
            for name in records._fields:
                moved._fields[name] = records._fields[name]
            for name in records._values:
                moved._values[name] = records._values[name]
            return moved

    # A pickle names the class by its module and its name, and the module
    # gives it through `__getattr__`.
    KVRecords.__module__ = __name__
    KVRecords.__qualname__ = "KVRecords"
    return KVRecords


class _Part:
    """The fields or the values of a `KVRecords`: a view of the rows of one
    prefix of the namespace, keyed by the names of the records."""

    def __init__(self, records, prefix):
        self._records = records
        self._prefix = prefix
        # The rows that the part keeps itself while its registry has no
        # storage with namespaces, by name.
        self._local = {}

    def _namespace(self):
        namespace = _namespace(self._records.__parent__)
        if namespace is not None and self._local:
            # The registry has a storage now: move the rows there.
            local, self._local = self._local, {}
            for name, value in local.items():
                namespace.set(self._key(name), pickle.dumps(value, 3))
            self._records.__parent__._p_changed = True
        return namespace

    def _key(self, name):
        return self._prefix + name.encode("utf-8")

    def _rows(self, namespace):
        """Return the encoded rows of the part by name: the committed rows
        with the writes of the current transaction."""
        rows = {}
        for key, value in _committed(namespace).items():
            if key.startswith(self._prefix):
                rows[key[len(self._prefix) :].decode("utf-8")] = value
        for key, value in namespace.writes().items():
            if key.startswith(self._prefix):
                name = key[len(self._prefix) :].decode("utf-8")
                if value is None:
                    rows.pop(name, None)
                else:
                    rows[name] = value
        return rows

    def _encoded(self, namespace, name):
        key = self._key(name)
        writes = namespace.writes()
        if key in writes:
            return writes[key]
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
        namespace = self._namespace()
        if namespace is None:
            self._local[name] = value
            self._records.__parent__._p_changed = True
            return
        namespace.set(self._key(name), pickle.dumps(value, 3))

    def __delitem__(self, name):
        namespace = self._namespace()
        if namespace is None:
            del self._local[name]
            self._records.__parent__._p_changed = True
            return
        if self._encoded(namespace, name) is None:
            raise KeyError(name)
        namespace.delete(self._key(name))

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


def process_starting(event):
    """Install the backend when Zope starts: the subscriber of
    `registry.zcml`."""
    install()


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
