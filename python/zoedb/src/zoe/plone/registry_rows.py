"""The rows of the registry backend of `zoe.plone` (ADR 0050).

This module needs no Plone. `zoe.plone.registry` builds the backend on it.

A moved registry keeps its records in the durable namespace
`zoe.plone.registry:` followed by its id, 32 hex digits. The namespace
holds these rows:

- `f` followed by the name of a record: the pickle of its field;
- `v` followed by the name of a record: the pickle of its value;
- `m`: the marker. It holds the version of the layout (1 byte), the oid of
  the registry (8 bytes) and the write counter (8 bytes, big-endian).

The pickles are protocol 3, by value. A persistent object that is not a
field of plone.registry is refused. A broken persistent object, whose class
is gone, is written as ZODB writes a broken object: `ZODB.broken.rebuild`
with the module and the name of its class. A read finds each class with
`ZODB.broken.find_global`, so a class that is gone gives a broken object.

The cache keeps the rows of each namespace as bytes for each storage, with
a label: the counter of the marker of the rows. A scan reads the generation
of the namespace before and after it, so it never keeps the rows of two
commits.
"""

import collections
import io
import pickle
import secrets
import threading
import weakref

import persistent
from ZODB.broken import PersistentBroken, find_global, rebuild
from ZODB.POSException import ConflictError

__all__ = [
    "LAYOUT",
    "MARKER",
    "PREFIX",
    "Cache",
    "decode_marker",
    "dumps",
    "encode_marker",
    "loads",
    "namespace_name",
    "new_id",
    "record_key",
    "scan_rows",
]

# The start of the name of the namespace of each registry.
PREFIX = "zoe.plone.registry:"

# The version of the layout of the rows.
LAYOUT = 1

# The key of the marker.
MARKER = b"m"

# The kinds of rows: the fields and the values.
FIELD = b"f"
VALUE = b"v"

# The longest key of a namespace, in bytes.
MAX_KEY = 1024

# The number of namespaces that the cache keeps for each storage.
BOUND = 16

# The number of scans of a namespace before a `ConflictError`.
SCAN_TRIES = 3

# The most pairs that one call of `kv_scan` gives.
_PAGE = 1000


def new_id():
    """Return a new id of a registry: 16 random bytes as 32 hex digits."""
    return secrets.token_hex(16)


def namespace_name(registry_id):
    """Return the name of the namespace of the registry `registry_id`."""
    return PREFIX + registry_id


def record_key(kind, name):
    """Return the key of the row of `kind` (`FIELD` or `VALUE`) of the
    record `name`. A key longer than 1,024 bytes raises `ValueError`."""
    key = kind + name.encode("utf-8")
    if len(key) > MAX_KEY:
        raise ValueError(
            f"the name of a record holds at most {MAX_KEY - 1} bytes: {name[:40]!r}..."
        )
    return key


def encode_marker(oid, counter):
    """Return the marker of the registry `oid` at the write `counter`."""
    if len(oid) != 8:
        raise ValueError("an oid holds 8 bytes")
    return bytes([LAYOUT]) + oid + counter.to_bytes(8, "big")


def decode_marker(data):
    """Return `(oid, counter)` of a marker, or None when `data` is None or a
    marker of another layout."""
    if data is None or len(data) != 17 or data[0] != LAYOUT:
        return None
    return data[1:9], int.from_bytes(data[9:], "big")


def _is_field(obj):
    """Tell if `obj` is a field of plone.registry."""
    from plone.registry.interfaces import IPersistentField

    return IPersistentField.providedBy(obj)


class _Pickler(pickle.Pickler):
    """A pickler that writes each object by value. It refuses a persistent
    object that `is_field` refuses, and writes a broken persistent object as
    `ZODB.broken.rebuild`."""

    def __init__(self, file, is_field):
        super().__init__(file, 3)
        self._is_field = is_field

    def reducer_override(self, obj):
        if isinstance(obj, PersistentBroken):
            obj._p_activate()
            cls = type(obj)
            args = (cls.__module__, cls.__name__, *obj.__Broken_newargs__)
            return rebuild, args, obj.__Broken_state__
        if isinstance(obj, persistent.Persistent):
            if not self._is_field(obj):
                raise TypeError(
                    "a record of a moved registry holds no persistent object"
                    f" but a field: {type(obj).__module__}.{type(obj).__name__}"
                )
            # A ghost gives its state only when it is loaded.
            obj._p_activate()
        return NotImplemented


def dumps(obj, is_field=_is_field):
    """Return the pickle of `obj`, by value, with protocol 3."""
    file = io.BytesIO()
    _Pickler(file, is_field).dump(obj)
    return file.getvalue()


class _Unpickler(pickle.Unpickler):
    """An unpickler that gives a broken class for a class that is gone."""

    def find_class(self, module, name):
        return find_global(module, name)


def loads(data):
    """Return the object of the pickle `data`."""
    return _Unpickler(io.BytesIO(data)).load()


def scan_rows(storage, name, tries=SCAN_TRIES):
    """Return `(rows, label)`: every row of the durable namespace `name` of
    `storage`, as a dict of bytes, and the counter of its marker, or 0.

    The scan reads the generation of the namespace before and after it.
    When a commit wrote the namespace in between, it scans again. After
    `tries` scans, it raises `ConflictError`.
    """
    for _ in range(tries):
        before = storage.kv_generation(name)
        rows = {}
        cursor = None
        while True:
            page = storage.kv_scan("durable", name, cursor, None, _PAGE)
            if not page:
                break
            rows.update((bytes(key), bytes(value)) for key, value in page)
            cursor = bytes(page[-1][0]) + b"\x00"
        if storage.kv_generation(name) == before:
            marker = decode_marker(rows.get(MARKER))
            return rows, 0 if marker is None else marker[1]
    raise ConflictError(
        f"a commit wrote the namespace {name} during each of {tries} scans"
    )


class Cache:
    """The rows of the namespaces of moved registries, for each storage.

    A storage is held through a weak reference. For each storage, the cache
    keeps at most `bound` namespaces; the one that was read least recently
    goes first. A lock guards it. A scan runs outside the lock.
    """

    def __init__(self, bound=BOUND):
        self._bound = bound
        self._lock = threading.Lock()
        # storage -> OrderedDict(name -> (label, rows))
        self._entries = weakref.WeakKeyDictionary()

    def rows(self, storage, name, counter, scan):
        """Return the rows of the namespace `name` of `storage` for a
        registry at the write `counter`.

        The cached rows serve when their label is at least `counter`. Else
        `scan()` gives `(rows, label)`, and the cache keeps them when the
        label is above the label that it holds.
        """
        with self._lock:
            names = self._entries.get(storage)
            entry = None if names is None else names.get(name)
            if entry is not None and entry[0] >= counter:
                names.move_to_end(name)
                return entry[1]
        rows, label = scan()
        with self._lock:
            names = self._entries.get(storage)
            if names is None:
                names = collections.OrderedDict()
                try:
                    self._entries[storage] = names
                except TypeError:
                    # A storage without weak references is not cached.
                    return rows
            entry = names.get(name)
            if entry is None or label > entry[0]:
                names[name] = (label, rows)
            names.move_to_end(name)
            while len(names) > self._bound:
                names.popitem(last=False)
        return rows

    def label(self, storage, name):
        """Return the label of the cached rows, or None."""
        with self._lock:
            names = self._entries.get(storage)
            entry = None if names is None else names.get(name)
            return None if entry is None else entry[0]

    def names(self, storage):
        """Return the names of the namespaces that the cache holds for
        `storage`, the one that was read least recently first."""
        with self._lock:
            return list(self._entries.get(storage, ()))

    def clear(self):
        """Drop every row."""
        with self._lock:
            self._entries.clear()
