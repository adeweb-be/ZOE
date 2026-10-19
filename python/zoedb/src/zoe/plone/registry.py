"""The registry backend of `zoe.plone` (ADR 0050).

`move(registry)` moves the records of a registry of plone.registry, its
`_fields` and `_values`, to a durable namespace of its own in the store:
`zoe.plone.registry:` followed by a random id. The registry then keeps a
`MovedRecords` in `_records`: the id, the write counter, and views of the
rows. Its stock `OOBTree`s stay in the store, out of its reach. The rows
are in `zoe.plone.registry_rows`.

The option is opt-in: `ZOE_PLONE_BACKENDS=registry`, or the ZCML file
`zoe.plone:registry.zcml`, calls `install()`. `install()` lets `move`,
`copy_rows` and `reset` run. A moved registry reads and writes in every
process, with or without the option.

- **A read** checks the rows first, once in each transaction: a registry
  in a storage without namespaces, without the marker of its rows, with
  the marker of another oid, or with a marker of a lower counter is a copy
  without its rows, and raises `Unsupported`. When the store holds a newer
  serial of the registry, a move back committed after the snapshot, and
  the read raises `ReadConflictError`. Then a read takes the writes of the
  current transaction, then the rows of a cache of the process. The cache
  is labelled with the counter of the marker. Each transaction decodes a
  row once.
- **A write** stages the row. The first write of a transaction sets the
  counter of the registry and of the marker to 1 more than the higher of
  the counter of the registry and the counter of the stored marker. So it
  writes the registry object: plone.protect sees the write, and two writes
  of one registry conflict. After an undo, a counter does not repeat.
- **A field changed in place** is staged again before the commit and
  before each savepoint. A rollback to a savepoint drops the decoded rows.
  A rollback can keep the staged rows and give back the registry with its
  old counter. So before the commit, a transaction with staged rows, the
  old counter and a staged marker of another counter raises the counter.
- **The guard** of a stock registry in a store with namespaces calls
  `readCurrent` on the roots of its trees when code reads its records, and
  the move empties those roots. So a write that began before a move
  conflicts, and a write of an attribute of the registry object does not.
  `configure.zcml` installs the guard whenever `zoe.plone` loads.

`move_back(registry)` puts the records back into new `OOBTree`s and deletes
the rows. `copy_rows(source, copy)` gives a copy of a moved registry the
rows of its source, under a new id. `reset(registry)` gives a copy whose
source is gone empty `OOBTree`s: the import of the profiles of the site
then gives back the records with their default values. On a ZEO client
without namespaces, `reset` raises `Unsupported`: the client cannot read
the rows that the server can hold.
"""

import bisect
import weakref

import transaction.interfaces
import zope.interface
from Acquisition import aq_base
from BTrees.OOBTree import OOBTree
from plone.registry.registry import Registry, _Records
from ZODB.POSException import ReadConflictError, Unsupported
from ZODB.utils import oid_repr

import zoe.kv
from zoe.plone import registry_rows
from zoe.plone.registry_rows import (
    FIELD,
    MARKER,
    VALUE,
    decode_marker,
    dumps,
    encode_marker,
    loads,
    namespace_name,
    record_key,
)

__all__ = [
    "MovedRecords",
    "copy_rows",
    "install",
    "install_guard",
    "installed",
    "move",
    "move_back",
    "reset",
    "uninstall",
    "uninstall_guard",
]

# The flag of the option, and the stock property `records` while the guard
# is installed.
_installed = {}
_guard = {}

# The rows of the namespaces of moved registries, for each storage.
_cache = registry_rows.Cache()

# The state of the current transaction of each connection.
_transactions = weakref.WeakKeyDictionary()

# Whether the storage of each database has namespaces.
_namespaces = weakref.WeakKeyDictionary()


def installed():
    """Tell if the backend is installed."""
    return bool(_installed)


def install():
    """Let `move`, `copy_rows` and `reset` run in this process. A second
    call does nothing."""
    _installed["on"] = True


def uninstall():
    """Stop `move`, `copy_rows` and `reset`, and drop the cache of rows. A
    second call does nothing."""
    _installed.clear()
    _cache.clear()


def _need_install(what):
    if not _installed:
        raise RuntimeError(
            f"{what} needs the registry backend of zoe.plone:"
            " ZOE_PLONE_BACKENDS=registry, or zoe.plone:registry.zcml"
        )


def _has_namespaces(jar):
    """Tell if the storage of the database of `jar` has namespaces."""
    db = jar.db()
    found = _namespaces.get(db)
    if found is None:
        found = _namespaces[db] = zoe.kv._supported(jar)
    return found


# The guard of a stock registry.


def install_guard():
    """Make each stock registry in a store with namespaces call
    `readCurrent` on the roots of its trees when code reads its records. A
    second call does nothing."""
    if _guard:
        return
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

    When the transaction read both roots current already and changed
    neither, the guard returns at once. It then loads no tree and reads no
    state. Only a change of a root in the transaction can make a root keep
    its own keys."""
    jar = registry._p_jar
    if jar is None or registry._p_oid is None:
        return
    trees = (records._fields, records._values)
    read = jar._readCurrent
    if all(
        tree._p_jar is jar and tree._p_oid in read and not tree._p_changed
        for tree in trees
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


# The moved records.


class MovedRecords(_Records):
    """The records of a moved registry: views of the rows of its namespace.

    Its state is the registry (`__parent__`, without an acquisition
    wrapper), the id of its namespace and the write counter.
    """

    def __init__(self, parent, registry_id, counter):
        self.__parent__ = aq_base(parent)
        self.id = registry_id
        self.counter = counter

    def __getstate__(self):
        return {"__parent__": self.__parent__, "id": self.id, "counter": self.counter}

    def __setstate__(self, state):
        self.__dict__.update(state)

    def __repr__(self):
        return f"<MovedRecords {self.id} at write {self.counter}>"

    @property
    def name(self):
        """The name of the namespace of the rows."""
        return namespace_name(self.id)

    @property
    def _fields(self):
        return _View(self, FIELD)

    @property
    def _values(self):
        return _View(self, VALUE)


def _describe(registry):
    """Return a name of `registry` for a message."""
    try:
        return "/".join(registry.getPhysicalPath())
    except Exception:
        oid = getattr(registry, "_p_oid", None)
        return "a registry" if oid is None else f"the registry {oid_repr(oid)}"


def _check(records):
    """Return `(rows, None)` for the moved `records` whose rows pass the
    check, else `(None, reason)`. `rows` are the committed rows that the
    transaction reads. Raise `Unsupported` for a registry without
    namespaces or without a connection."""
    registry = records.__parent__
    jar = registry._p_jar
    if jar is None or registry._p_oid is None:
        raise Unsupported(
            f"{_describe(registry)} is moved and is in no database: it has no rows"
        )
    namespace = zoe.kv.Namespace(jar, "durable", records.name)

    def scan():
        return registry_rows.scan_rows(zoe.kv._storage_of(jar), records.name)

    rows = _cache.rows(jar.db().storage, records.name, records.counter, scan)
    writes = namespace._staged(create=False)
    found, marker = (
        (False, None)
        if writes is None
        else writes.find("durable", records.name, MARKER)
    )
    if not found:
        marker = rows.get(MARKER)
    marker = decode_marker(marker)
    if marker is None:
        return None, "its namespace has no marker"
    oid, counter = marker
    if oid != registry._p_oid:
        return None, "its rows belong to the registry " + oid_repr(oid)
    if counter < records.counter:
        return None, "its rows are older than the registry"
    return rows, None


def _changed_in_store(registry):
    """Tell if the store holds a newer serial of `registry` than the
    connection read. `history` of a client of a server asks the server, not
    the cache of the client."""
    storage = registry._p_jar.db().storage
    try:
        found = storage.history(registry._p_oid, 1)
    except KeyError:
        return False
    return bool(found) and found[0]["tid"] != registry._p_serial


def _fail(records, reason):
    registry = records.__parent__
    if _changed_in_store(registry):
        raise ReadConflictError(oid=registry._p_oid)
    raise Unsupported(
        f"{_describe(registry)} is moved, but {reason}: it is a copy without"
        " its rows. Run zoe.plone.registry.copy_rows from its source, move it"
        " back in its source before a copy, or run zoe.plone.registry.reset"
    )


def _state(records):
    """Return the state of the moved `records` in the current transaction
    of its connection. The first call of a transaction runs the check."""
    registry = records.__parent__
    jar = registry._p_jar
    if jar is None:
        _fail_unconnected(registry)
    txn = jar.transaction_manager.get()
    current = _transactions.get(jar)
    if current is None or current.transaction is not txn:
        current = None
    key = (registry._p_oid, records.id)
    if current is not None:
        state = current.states.get(key)
        if state is not None:
            return state
    rows, reason = _check(records)
    if rows is None:
        _fail(records, reason)
    if current is None:
        current = _Transaction(jar, txn)
        if current.joined:
            _transactions[jar] = current
    state = current.states[key] = _State(current, records, rows)
    return state


def _fail_unconnected(registry):
    raise Unsupported(
        f"{_describe(registry)} is moved and is in no database: it has no rows"
    )


class _State:
    """One moved registry in one transaction: the rows that it reads, the
    objects that it decoded, and the bytes of the decoded fields."""

    def __init__(self, owner, records, rows):
        registry = records.__parent__
        # The `_Transaction` that holds this state.
        self.owner = owner
        self.registry = registry
        self.id = records.id
        # The counter of the registry at the check.
        self.counter = records.counter
        self.namespace = zoe.kv.Namespace(registry._p_jar, "durable", records.name)
        self.rows = rows
        # key -> object
        self.decoded = {}
        # key -> the bytes of a decoded field: the row at the decode, or the
        # pickle of the last write
        self.fields = {}
        # kind -> the sorted names, or absent
        self.names = {}

    def records(self):
        """Return the `MovedRecords` of the registry now: a rollback can
        give the registry a new one."""
        return self.registry._records

    def raw(self, key):
        """Return the bytes of `key`: a write of the transaction first, then
        the rows. None when it has none."""
        writes = self.namespace._staged(create=False)
        if writes is not None:
            found, value = writes.find("durable", self.namespace.name, key)
            if found:
                return value
        return self.rows.get(key)

    def get(self, key):
        """Return the object of `key`, decoded once. Raise `KeyError`."""
        try:
            return self.decoded[key]
        except KeyError:
            pass
        data = self.raw(key)
        if data is None:
            raise KeyError(key)
        obj = loads(data)
        self.decoded[key] = obj
        if key[:1] == FIELD and not isinstance(obj, str):
            self.fields[key] = data
            self.owner.watch_fields()
        return obj

    def contains(self, key):
        return key in self.decoded or self.raw(key) is not None

    def staged(self):
        """Return the writes of the namespace in the transaction, as a dict
        of key to value, None for a delete."""
        writes = self.namespace._staged(create=False)
        if writes is None:
            return {}
        return writes.of("durable", self.namespace.name)

    def sorted_names(self, kind):
        """Return the names of the records of `kind`, sorted."""
        names = self.names.get(kind)
        if names is None:
            keys = {key for key in self.rows if key[:1] == kind}
            writes = self.namespace._staged(create=False)
            if writes is not None:
                for key, value in writes.of("durable", self.namespace.name).items():
                    if key[:1] != kind:
                        continue
                    if value is None:
                        keys.discard(key)
                    else:
                        keys.add(key)
            names = self.names[kind] = sorted(key[1:].decode("utf-8") for key in keys)
        return names

    def bump(self):
        """Count the first write of the transaction: stage the marker and
        the counter of the registry with 1 more than the highest of the
        counter of the registry, the counter of the staged marker and the
        counter of the stored marker. After an undo the stored marker is
        higher, so a counter never repeats. After a rollback to a savepoint
        the staged marker can be higher than the registry."""
        records = self.records()
        if records.counter > self.counter:
            return
        name = self.namespace.name
        markers = [self.namespace._storage.kv_get("durable", name, MARKER)]
        writes = self.namespace._staged(create=False)
        if writes is not None:
            found, staged = writes.find("durable", name, MARKER)
            if found:
                markers.append(staged)
        counter = self.counter
        for marker in map(decode_marker, markers):
            if marker is not None:
                counter = max(counter, marker[1])
        records.counter = counter + 1
        self.registry._p_changed = True
        self.namespace.set(MARKER, encode_marker(self.registry._p_oid, records.counter))

    def put(self, key, obj):
        """Stage `obj` for `key`."""
        data = dumps(obj)
        self.bump()
        self.namespace.set(key, data)
        self.decoded[key] = obj
        if key[:1] == FIELD and not isinstance(obj, str):
            self.fields[key] = data
            self.owner.watch_fields()
        else:
            self.fields.pop(key, None)
        self.names.pop(key[:1], None)

    def delete(self, key):
        """Stage the delete of `key`."""
        self.bump()
        self.namespace.delete(key)
        self.decoded.pop(key, None)
        self.fields.pop(key, None)
        self.names.pop(key[:1], None)

    def write_back(self):
        """Stage each decoded field that changed in place.

        A pickle of the field equal to its bytes is no change. A row can
        hold a pickle that a new pickle of its object does not give byte
        for byte: a pickle of another writer, or one that shares a str in
        another way. So when the two differ, the bytes are decoded again
        and that new object is pickled. A pickle equal to that one is no
        change either, and the row stays as it is. Only a pickle that
        differs from both is a change."""
        for key, stored in list(self.fields.items()):
            obj = self.decoded.get(key)
            if obj is None:
                continue
            data = dumps(obj)
            if data == stored:
                continue
            if data == dumps(loads(stored)):
                # The next check compares with the new pickle.
                self.fields[key] = data
                continue
            self.bump()
            self.namespace.set(key, data)
            self.fields[key] = data

    def count_staged(self):
        """Raise the counter when the transaction staged rows, the counter
        of the registry is not raised, and the staged marker holds another
        counter. A rollback to a savepoint can keep the rows that the
        savepoint staged and give back the registry with its old counter. A
        move or `copy_rows` stages a marker with the counter of the
        registry, so its counter stays."""
        records = self.records()
        if records.counter > self.counter or not self.staged():
            return
        writes = self.namespace._staged(create=False)
        found, data = writes.find("durable", self.namespace.name, MARKER)
        marker = decode_marker(data) if found else None
        if marker is not None and marker[1] == records.counter:
            return
        self.bump()

    def forget(self):
        """Drop the decoded rows, as a rollback does."""
        self.decoded.clear()
        self.fields.clear()
        self.names.clear()


@zope.interface.implementer(transaction.interfaces.IDataManager)
class _Transaction:
    """The moved registries of one transaction of one connection.

    It joins the transaction as a data manager that writes nothing itself.
    A rollback to a savepoint drops the decoded rows. Before the commit and
    before each savepoint, it stages the fields that changed in place.
    Before the commit, it also raises the counter of a registry with staged
    rows and the old counter.
    """

    def __init__(self, jar, txn):
        self.transaction_manager = jar.transaction_manager
        self.transaction = txn
        self._jar = jar
        # (oid, id) -> _State
        self.states = {}
        self._watching = False
        # A transaction that is not active, as in an after-commit hook,
        # takes no data manager. Its reads then keep no state.
        try:
            txn.join(self)
        except ValueError:
            self.joined = False
        else:
            self.joined = True

    def watch_fields(self):
        """Stage the changed fields before the commit and before each
        savepoint of the namespace writes."""
        if self._watching or not self.joined:
            return
        self._watching = True
        state = next(iter(self.states.values()), None)
        if state is not None:
            writes = state.namespace._staged(create=True)
            writes.flush_before_savepoint(self.write_back)
        self.transaction.addBeforeCommitHook(self.before_commit)

    def write_back(self):
        for state in list(self.states.values()):
            state.write_back()

    def before_commit(self):
        for state in list(self.states.values()):
            state.write_back()
            state.count_staged()

    def _end(self):
        self.states.clear()
        if _transactions.get(self._jar) is self:
            del _transactions[self._jar]

    # IDataManager

    def abort(self, txn):
        self._end()

    def tpc_begin(self, txn):
        pass

    def commit(self, txn):
        pass

    def tpc_vote(self, txn):
        pass

    def tpc_finish(self, txn):
        self._end()

    def tpc_abort(self, txn):
        self._end()

    def sortKey(self):
        return f"zoe.plone.registry:{id(self)}"

    def savepoint(self):
        self.write_back()
        return _Savepoint(self)


@zope.interface.implementer(transaction.interfaces.IDataManagerSavepoint)
class _Savepoint:
    """A savepoint of the moved registries: its rollback drops the decoded
    rows, so the next read decodes them from the rows that the savepoint
    kept."""

    def __init__(self, owner):
        self._owner = owner

    def rollback(self):
        for state in self._owner.states.values():
            state.forget()


class _View:
    """The fields (`FIELD`) or the values (`VALUE`) of a moved registry, with
    the methods of an `OOBTree` that plone.registry calls."""

    def __init__(self, records, kind):
        self._records = records
        self._kind = kind

    def _state(self):
        return _state(self._records)

    def _key(self, name):
        if not isinstance(name, str):
            raise TypeError(f"the name of a record is a str, not {type(name).__name__}")
        return record_key(self._kind, name)

    def __getitem__(self, name):
        state = self._state()
        try:
            return state.get(self._key(name))
        except KeyError:
            raise KeyError(name) from None

    def get(self, name, default=None):
        try:
            return self[name]
        except KeyError:
            return default

    def __setitem__(self, name, value):
        state = self._state()
        state.put(self._key(name), value)

    def __delitem__(self, name):
        state = self._state()
        key = self._key(name)
        if not state.contains(key):
            raise KeyError(name)
        state.delete(key)

    def __contains__(self, name):
        if not isinstance(name, str):
            return False
        return self._state().contains(self._key(name))

    has_key = __contains__

    def __len__(self):
        return len(self._state().sorted_names(self._kind))

    def __bool__(self):
        return bool(len(self))

    def __iter__(self):
        return iter(list(self._state().sorted_names(self._kind)))

    def _range(self, min, max):
        """Return the sorted names and the slice of the names from `min` to
        `max`, both included. A bisect finds each end."""
        names = self._state().sorted_names(self._kind)
        start = 0 if min is None else bisect.bisect_left(names, min)
        stop = len(names) if max is None else bisect.bisect_right(names, max)
        return names, start, stop

    def keys(self, min=None, max=None):
        names, start, stop = self._range(min, max)
        return names[start:stop]

    def values(self, min=None, max=None):
        return [self[name] for name in self.keys(min, max)]

    def items(self, min=None, max=None):
        return [(name, self[name]) for name in self.keys(min, max)]

    def minKey(self, key=None):
        names, start, stop = self._range(key, None)
        if start >= stop:
            raise ValueError("no key satisfies the conditions")
        return names[start]

    def maxKey(self, key=None):
        names, start, stop = self._range(None, key)
        if start >= stop:
            raise ValueError("no key satisfies the conditions")
        return names[stop - 1]

    def clear(self):
        state = self._state()
        for name in list(state.sorted_names(self._kind)):
            state.delete(self._key(name))


# The move, the move back, the copy of the rows and the reset.


def _bare(registry):
    """Return `registry` without its acquisition wrapper."""
    return aq_base(registry)


def _read_current_tree(jar, tree):
    """Call `readCurrent` on `tree` and on each node and bucket below it."""
    todo = [tree]
    while todo:
        node = todo.pop()
        if getattr(node, "_p_oid", None) is None or node._p_jar is not jar:
            continue
        node._p_activate()
        jar.readCurrent(node)
        if not isinstance(node, OOBTree):
            continue
        state = node.__getstate__()
        # An empty tree has no state, and a tree of one bucket keeps the
        # bucket in its state. Else the state holds the children, with a
        # key between two children.
        if state is None or len(state) == 1:
            continue
        todo.extend(state[0][::2])


def move(registry):
    """Move the records of `registry` to a durable namespace of its own.
    The caller commits. A moved registry stays as it is."""
    _need_install("move")
    registry = _bare(registry)
    records = registry.records
    if isinstance(records, MovedRecords):
        return
    jar = registry._p_jar
    if jar is None or registry._p_oid is None:
        raise Unsupported(f"{_describe(registry)} is in no database: it cannot move")
    if not zoe.kv._supported(jar):
        raise Unsupported(
            f"the storage of {_describe(registry)} has no KV namespaces:"
            " its registry stays in the object graph"
        )
    registry_id = registry_rows.new_id()
    namespace = zoe.kv.Namespace(jar, "durable", namespace_name(registry_id))
    trees = (records._fields, records._values)
    for tree in trees:
        _read_current_tree(jar, tree)

    def seen(obj):
        if obj._p_jar is jar and obj._p_oid is not None:
            jar.readCurrent(obj)

    # A field is a persistent object of its own: a change of it in place
    # during the move makes the commit of the move fail.
    for name, field in records._fields.items():
        namespace.set(record_key(FIELD, name), dumps(field, seen=seen))
    for name, value in records._values.items():
        namespace.set(record_key(VALUE, name), dumps(value))
    namespace.set(MARKER, encode_marker(registry._p_oid, 1))
    # The guard calls readCurrent on a root with children, so the move
    # empties it. An empty root is not a change by splits, so the split
    # merge lets no write through it. The buckets stay as they are. A root
    # that keeps its own keys stays: the guard reads the registry object
    # then, and the readCurrent of the move on the root finds a write of a
    # record.
    for tree in trees:
        if tree._p_jar is jar and not _holds_its_bucket(tree):
            tree.clear()
    registry._records = MovedRecords(registry, registry_id, 1)


def _all_keys(state):
    """Return every key of the namespace of `state` with the writes of its
    transaction: the rows, then the staged writes over them."""
    keys = set(state.rows)
    writes = state.namespace._staged(create=False)
    if writes is not None:
        for key, value in writes.of("durable", state.namespace.name).items():
            if value is None:
                keys.discard(key)
            else:
                keys.add(key)
    return keys


def move_back(registry):
    """Put the records of the moved `registry` back into new `OOBTree`s, and
    delete its rows. The caller commits. A stock registry stays as it is."""
    registry = _bare(registry)
    records = registry._records
    if not isinstance(records, MovedRecords):
        return
    state = _state(records)
    stock = _Records(registry)
    for name in state.sorted_names(FIELD):
        stock._fields[name] = state.get(record_key(FIELD, name))
    for name in state.sorted_names(VALUE):
        stock._values[name] = state.get(record_key(VALUE, name))
    for key in sorted(_all_keys(state)):
        state.namespace.delete(key)
    registry._records = stock
    state.owner.states.pop((registry._p_oid, records.id), None)


def _passes(records):
    """Tell if the moved `records` pass the check."""
    try:
        rows, _reason = _check(records)
    except Unsupported:
        return False
    return rows is not None


def copy_rows(source, copy):
    """Give `copy`, a copy of the moved registry `source` that fails the
    check, the rows of `source` under a new id. The caller commits the
    transaction of `copy`."""
    _need_install("copy_rows")
    source = _bare(source)
    copy = _bare(copy)
    records = source._records
    copied = copy._records
    if not isinstance(records, MovedRecords) or not isinstance(copied, MovedRecords):
        raise ValueError("copy_rows takes two moved registries")
    if copied.id != records.id:
        raise ValueError(
            f"{_describe(copy)} is not a copy of {_describe(source)}: its id differs"
        )
    if copy._p_jar is None or copy._p_oid is None:
        raise Unsupported(f"{_describe(copy)} is in no database")
    if _passes(copied):
        raise ValueError(f"{_describe(copy)} reads its own rows: it needs no copy")
    state = _state(records)
    source._p_jar.readCurrent(source)
    jar = copy._p_jar
    rows = {key: state.raw(key) for key in _all_keys(state)}
    marker = decode_marker(rows.pop(MARKER, None))
    counter = records.counter if marker is None else max(marker[1], records.counter)
    registry_id = registry_rows.new_id()
    target = zoe.kv.Namespace(jar, "durable", namespace_name(registry_id))
    for key in sorted(rows):
        target.set(key, rows[key])
    target.set(MARKER, encode_marker(copy._p_oid, counter))
    # Stale rows of the copy itself go.
    old = zoe.kv.Namespace(jar, "durable", copied.name)
    found = decode_marker(old.get(MARKER))
    if found is not None and found[0] == copy._p_oid:
        cursor = None
        while True:
            page = old.scan(cursor)
            if not page:
                break
            for key, _value in page:
                old.delete(key)
            cursor = page[-1][0] + b"\x00"
    copy._records = MovedRecords(copy, registry_id, counter)


# The attributes that hold the storage under a wrapper: `base` of a
# `DemoStorage` and of zc.zlibstorage, `storage` of zc.beforestorage, the
# storage of `ZODB.blob.BlobStorage`, and `_storage` of other wrappers.
_WRAPPED = ("base", "storage", "_storage", "_BlobStorage__storage")


def _layers(storage):
    """Yield `storage` and each storage that a wrapper over it holds in an
    attribute of `_WRAPPED`."""
    todo = [storage]
    seen = set()
    while todo:
        layer = todo.pop()
        if id(layer) in seen:
            continue
        seen.add(id(layer))
        yield layer
        for name in _WRAPPED:
            try:
                inner = getattr(layer, name, None)
            except Exception:
                continue
            # A storage loads records. A name or a path does not.
            if inner is not None and callable(getattr(inner, "load", None)):
                todo.append(inner)


def _zeo_client_without_kv(storage):
    """Tell if `storage`, or a storage under a wrapper of it, is a ZEO
    client without KV namespaces."""
    try:
        from ZEO.ClientStorage import ClientStorage
    except ImportError:
        return False
    return any(
        isinstance(layer, ClientStorage) and not zoe.kv._has_namespaces(layer)
        for layer in _layers(storage)
    )


def reset(registry):
    """Give the moved `registry`, a copy whose source is gone, new empty
    `OOBTree`s. The caller commits, then imports the registry step of each
    profile of the site. A registry that passes the check raises
    `ValueError`: it moves back instead. A registry on a ZEO client without
    namespaces, also under a wrapper, raises `Unsupported`: the server can
    hold its rows."""
    _need_install("reset")
    registry = _bare(registry)
    records = registry._records
    if not isinstance(records, MovedRecords):
        raise ValueError(f"{_describe(registry)} is not moved")
    if _passes(records):
        raise ValueError(f"{_describe(registry)} reads its rows: move it back")
    jar = registry._p_jar
    if jar is not None and _zeo_client_without_kv(jar.db().storage):
        raise Unsupported(
            f"{_describe(registry)} is on a ZEO client without KV namespaces:"
            " it cannot tell if the server holds the rows. To reset it, stop"
            " the ZEO server and open the storage directly (a FileStorage, or"
            " a zoe.Storage). To read it, use zoe.zeo.ClientStorage with a"
            " zoe serve that accepts kv"
        )
    registry._records = _Records(registry)
