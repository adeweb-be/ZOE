# 0057. The reader of ZOE: a record decodes in Rust in place of ObjectReader

Status: accepted. Card ZOE-276. The spike ZOE-274 built the prototype (finding: `docs/spikes/ghost-to-cache.md`). This record adds a public Python API: the module `zoe.reader` (`Reader`, `Connection`, `install(db)`, `counts()`), the key `native-reader` of `<zoe>` and of `<zoeclient>`, the argument `native_reader` of `zoe.Storage` and the attribute `native_reader` of the storages. It changes no wire message and no on-disk format.

## Context

A load from the client cache spends most of its time in the Python frames of ZODB and of the client, and little in the C of `persistent` (`docs/spikes/load-path.md`). In a hit at 1 KB, `Connection.setstate` calls `ObjectReader.setGhostState`, which makes an unpickler for each record (a `BytesIO`, a `find_global` closure, a `PersistentUnpickler`), finds each class through the class factory of the database, and calls back into Python for each persistent reference. That is 2.9 to 3.4 µs of a hit of 6.3 to 6.7 µs on CPython 3.13 and 3.14t. N4 asks for 3× ZEO in the `read_hot` rows of phase 3.

The spike decoded the same records in Rust in 0.39 µs, with the same state for every record of the `small` fixture of B5 and of the conformance suites, and a hit at 1 KB fell to 3.2 to 3.4 µs, 2.5× to 2.7× ZEO. A Plone document view took 14 % less time.

ZODB gives no attribute for the reader alone: `Connection.__init__` makes `ObjectReader(self, self._cache, self._db.classFactory)` by name, and `Connection.setstate` calls `self._reader.setGhostState(obj, p)`. The one class hook of ZODB is `DB.klass`, the class that `DB.open` makes.

## Decision

The reader of ZOE decodes a record in Rust, in place of ZODB's `ObjectReader`. It gives the state that `ObjectReader.getState` gives, or it gives the record to `ObjectReader` whole. `Connection.setstate` stays ZODB's.

### 1. The classes

`zoe._zoe.Reader`, in `crates/zoe-py/src/reader.rs`, has `setGhostState(obj, pickle)` and `getState(pickle)`. `zoe.reader.Reader` derives from it and from ZODB's `ObjectReader`, in that order. `ObjectReader` gives the rest: its `__init__`, which sets `_conn`, `_cache` and `_factory`, and `getGhost`, `getClassName`, `_persistent_load` and the loaders, which `Connection.get`, the export and the import call. `Connection._resetCache` sets `_reader._cache`, and the reader reads `_cache` at each decode, so a new cache reaches it.

`zoe.reader.Connection` is ZODB's `Connection` with a `Reader`, made in its `__init__`.

### 2. The decode

`getState` runs one decoder over the record: the class pickle, then the state pickle, with one memo for both, as one unpickler loads them. It builds the objects as the C unpickler of zodbpickle builds them, for the opcodes that ZODB writes at protocol 3:

- the ints (`BININT`, `BININT1`, `BININT2`, `LONG1` up to 16 bytes), `BINFLOAT`, `NONE`, `NEWTRUE`, `NEWFALSE`;
- `BINUNICODE`, decoded from UTF-8 with `surrogatepass`; `BINBYTES` and `SHORT_BINBYTES`; `BINSTRING` and `SHORT_BINSTRING`, a Python 2 str, decoded as ASCII, strict, as the unpickler of ZODB decodes it;
- the tuples (`EMPTY_TUPLE`, `TUPLE`, `TUPLE1` to `TUPLE3`), the lists and the dicts (`EMPTY_LIST`, `EMPTY_DICT`, `APPEND`, `APPENDS`, `SETITEM`, `SETITEMS`): a list or a subclass of list takes the items directly, another object through its `append`, and a dict through `__setitem__`;
- `GLOBAL`, through the table of classes (section 4); `REDUCE`; `NEWOBJ`, the `tp_new` of the class; `BUILD`, the `__setstate__` of the object, else the instance dict with interned keys and the slot state;
- `BINPERSID` (section 3);
- `MARK`, `BINPUT`, `LONG_BINPUT`, `BINGET`, `LONG_BINGET`, `DUP`, `POP`, `POP_MARK`, `PROTO` up to 3 and `STOP`.

The constants of the opcodes come from `zoe_pickle::opcode`.

### 3. The persistent references

`BINPERSID` does what `ObjectReader._persistent_load` does, with the C calls of `cPickleCache` and of the class:

- `(oid, klass)`: `cache.get(oid, None)`; else the class, through the table when `klass` is a pair of names; `klass.__new__(klass)`, or `conn.get(oid)` when `__new__` raises `TypeError`; then `cache.new_ghost(oid, obj)`;
- a bare oid, bytes or an ASCII str: `cache.get(oid, None)`, else `conn.get(oid)`.

The reader keeps no cache of objects of its own. A `persistent` in Rust is not needed (`docs/spikes/ghost-to-cache.md`, "Verdict on a `persistent` in Rust").

### 4. The table of classes

A `GLOBAL` looks up its module line and name line in a table of the reader. A miss calls `_factory(_conn, module, name)`, as the `find_global` closure of `ObjectReader` does, and keeps the result in the table. So a class stays in the table for the life of the reader: a module that binds a name to another class while a connection lives is not seen by the reader of that connection. The table keeps no subclass of `ZODB.broken.Broken`: the record goes to `ObjectReader` (section 5), and a class that imports later, the "repaired global" of `ZODB.broken`, is found then.

### 5. The rule: as `ObjectReader` decodes, or `ObjectReader` whole

A record goes to the Python path, `ObjectReader.getState`, whole, for any construct of this list:

| Reason in the counts | Construct |
| --- | --- |
| `opcode` | another opcode, such as `LONG4`, `EXT1` or a text opcode of protocol 0 |
| `protocol` | `PROTO` above 3 |
| `malformed` | a short read, a missing mark, a memo index that is not set or is above 2²⁰, a stack that is too short, bytes that do not decode, a value of the wrong type for its opcode |
| `broken` | a class that the class factory gives as a subclass of `Broken` |
| `weak reference` | a weak reference: `[oid]` or `["w", ...]` |
| `other database` | a reference into another database: `["m", ...]` or `["n", ...]` |
| `reference` | a persistent id of another shape |
| `big int` | an int of more than 16 bytes |
| `python 2 str` | a Python 2 str that is not ASCII |
| `not bytes` | a pickle that is not `bytes` |
| `error` | an `Exception` that a call into Python raised during the decode |

The Python path then gives the state, or raises what it raises: with ZODB 6.4 or later, a `StateLoadError` for a record that does not decode. An exception that is not an `Exception`, such as `KeyboardInterrupt`, leaves the reader at once, as it leaves `ObjectReader.getState`. The Python path calls the class factory, the constructors and `__setstate__` of the values again: the calls of a decode build values, so a second decode builds the same values. `setGhostState` then calls `obj.__setstate__(state)`, and gives an error of it as `ObjectReader.setGhostState` does: with a ZODB that has `StateLoadError`, an `Exception` that is not a `POSError` or a `TransientError` becomes a `StateLoadError` whose cause is the error.

The reader counts, in the process, the records that the Rust path decoded, the records of the Python path by reason, and the opcodes that sent a record there. `zoe.reader.counts()` gives them.

### 6. The hook

`zoe.reader.install(db)` sets `db.klass` to `zoe.reader.Connection` when it is ZODB's `Connection`, and gives a `Reader` to each connection that the pools of `db` already hold, with the class factory of the reader of that connection. That writes `_reader` of a Connection, a private attribute that ZODB itself writes in `__init__` and whose `_cache` it writes in `_resetCache`. ZODB is not monkeypatched: `Connection.setstate` and `ObjectReader` stay as they are. A database whose `klass` is another class keeps it.

Two other places do not reach every connection:

- **The storage cannot.** `DB()` wraps a storage that is not an `IMVCCStorage` in an `MVCCAdapter`, which calls `registerDB` with itself, so the storage never sees the database.
- **Zope's `connection-class`** sets `DB.klass` when Zope opens the database, but `DB.__init__` already opened and pooled one stock `Connection` to make the root, and `DB.open` gives the pooled connection first. A Zope of one thread uses that connection for each request.

So `zoe.plone` has a subscriber of `IDatabaseOpenedWithRoot`, which Zope notifies once at its start with the main database. When the storage of that database is `zoe.Storage` or `zoe.zeo.ClientStorage`, it calls `zoe.reader.install(db)`. It needs no line in `zope.conf`, only the `plone` extra. A database on another storage keeps ZODB's `ObjectReader`. An application outside Zope calls `zoe.reader.install(db)` after `DB()`, on any storage.

### 7. The switch

The key `native-reader` of `<zoe>` and of `<zoeclient>`, a boolean, true by default, is the switch. `<zoe>` gives it a twin, `ZOE_NATIVE_READER`, as it gives one to each key; `<zoeclient>` gives it none, as its other keys but `prefetch-references`. `zoe.Storage` takes it as `native_reader`, and `<zoeclient>` gives it to `zoe.zeo.ClientStorage` as the private argument `_native_reader`, which the fall back to the client of ZEO keeps. Each storage keeps it as the attribute `native_reader`. `zoe.reader.install(db)` reads `db.storage.native_reader` and gives nothing when it is false, so a Plone instance with `native-reader off` makes the calls of ZODB as before this record. A storage without the attribute counts as true.

### 8. Threads

The reader adds no lock to the pickle cache: a connection and its reader serve one thread at a time, as ZODB requires. The table of classes has a mutex, held for a lookup or an insert only; no Python code runs while it is held, and a value that the table replaces drops outside it. The reader works on CPython 3.12, 3.13, 3.14 and 3.14t, with the GIL off on 3.14t.

### 9. The tests

- `python/zoedb/tests/test_zoe_reader.py`, the acceptance test: for each construct of a record, the state of the reader pickles to the bytes of the state of `ObjectReader.getState`, with each persistent object as its oid and its class, and each reference is the object of the pickle cache for its oid; a weak reference, a reference into another database, a `Broken` class and a truncated record take the Python path, with the same state or the same error, and the counts name the reason; `install(db)` gives the reader to a pooled connection and to the next one.
- `python/zoedb/src/zoe/plone/tests/test_reader_install.py`, the acceptance test of the subscriber: a Zope start on a `<zoe>` storage gives every connection of the main database the reader, the pooled one included; with `native-reader off`, none. `just plone zoe backends` runs it.
- The switch of the tests: with `ZOE_TEST_READER=on`, `python/zoedb/tests/conftest.py` sets the name `ObjectReader` of `ZODB.Connection` to `zoe.reader.Reader`, so each Connection of the run has the reader, also the Connections that the tests of ZODB make on their own storages. `just conformance` runs with it on.
- `just bench profile oracle`: every current record of the FileStorage of the `small` fixture of B5, with the class factory of Zope, through the reader and through `ObjectReader`, must compare equal.

## Consequences

- A load in a Zope process on a ZOE storage decodes its record in Rust by default. The decode no longer makes an unpickler, and calls Python only for the class factory at the first sight of a class, for `REDUCE`, `NEWOBJ` and `BUILD` of the values, and for the calls of the pickle cache.
- `Connection.setstate`, `MVCCAdapterInstance.load` and the frames of `zoe.zeo` stay. A `setstate` of `zoe.reader.Connection` in Rust and a load through the client cache from the reader are later cards; each overrides more of ZODB, and amends this record.
- A record of a construct that the list of section 2 does not name costs one decode more: the Rust path stops at the construct, and `ObjectReader` decodes the record again. The fixture and the Plone requests of the spike had no such record.
- The rule of section 4 differs from ZODB in one case: a module that rebinds a class name while a connection lives. The conformance suites have no such case.
- A new opcode that ZODB writes, such as a protocol above 3, goes to the Python path until the reader learns it.

## Amendment (proposed): `setstate` of `zoe.reader.Connection` in Rust

Status: proposed. Card ZOE-278. It replaces the sentence "`Connection.setstate` stays ZODB's" of the decision, section 1, the first sentence of the second item of the consequences for `Connection.setstate`, and adds to sections 6 and 9. The rest of the record stays. It changes no wire message, no on-disk format and no name of `zoe.reader`.

### A. The method

`zoe.reader.Connection.setstate` overrides ZODB's `Connection.setstate`. It is `zoe._zoe._setstate`, a function in Rust (`crates/zoe-py/src/reader.rs`) in an `instancemethod` of CPython, so the class attribute binds to the connection as a function of Python does, and a load runs no Python frame of `setstate`.

For an ordinary record it does the lines of ZODB's method, in their order: `obj._p_oid`; the check of `self.opened`; `p, serial = self._storage.load(oid)`, so the MVCC load stays the load of the storage of the connection; `self._load_count += 1`; the decode and `__setstate__` of the reader; `obj._p_serial = serial`; `self._cache.update_object_size_estimation(oid, len(p))`; `obj._p_estimated_size = len(p)`. When `_reader` is exactly a `zoe.reader.Reader`, the decode is the Rust path of `setGhostState` without a call through Python; another reader gets `self._reader.setGhostState(obj, p)`.

### B. The versions of ZODB

The method follows `Connection.setstate` of ZODB 6.0, 6.1, 6.2, 6.3 and 6.4, which have the same lines. A version of ZODB that changes them needs a change of the method and of its acceptance test.

### C. What goes to the method of ZODB

These loads call the `setstate` that ZODB's `Connection` has at the call, with the connection and the object:

- a closed connection: ZODB's method logs and raises its `ConnectionStateError`;
- an instance of `ZODB.blob.Blob`: ZODB's method loads it and its blob file;
- a connection whose `setstate` of ZODB's `Connection` is not the function of ZODB. The early locks of `zoe.zodb` put the `setstate` of `LockingConnection` there once per process; for a storage whose `early_lock_classes` is not empty, the load goes to it, and for another storage it stays in Rust, as `LockingConnection.setstate` gives such a load to the function of ZODB. Another function, such as a patch of another package, gets every load.

The method reads the function of ZODB at its first call, by its module and its name.

### D. The exceptions

A `ConflictError` of a line of section A leaves as it is. Another exception, `BaseException` included, is logged as the bare `except` of ZODB logs it, `self._log.exception("Couldn't load state for %s %s", className(obj), oid_repr(oid))`, with the exception as `exc_info`, then raised as it is. An exception of the log takes its place, with the first one as its context. Two facts of the log record differ from those of ZODB's method: its function, file and line are those of the caller of `setstate`, and `sys.exc_info()` gives no exception while a handler of the log runs.

### E. The hook

`zoe.reader.install(db)` also sets the class of each connection of the pools of `db` whose class is ZODB's `Connection` to `zoe.reader.Connection`. That class adds no slot and no instance attribute, so CPython accepts the assignment and the connection keeps its state. The connection that `DB()` pools to make the root, which a Zope of one thread uses for each request, then loads with the method of section A. A connection of another subclass of `Connection` keeps its class.

### F. The tests

- `python/zoedb/tests/test_zoe_reader_setstate.py`, the acceptance test: a `zoe.reader.Connection` and a ZODB `Connection` with ZODB's `ObjectReader` load the same ghost of one database. For an ordinary record (with the reader of ZOE and with `ObjectReader`), a blob, a closed connection, a `ConflictError` of the storage, another error of the storage (`POSKeyError` and `KeyboardInterrupt`) and a record that does not decode, the state of the object, `_p_serial`, `_p_estimated_size`, `_load_count`, the `total_estimated_size` of the pickle cache, the log records (logger, level, message, and the type and the text of their exception) and the exception (the same object for an error of the storage; else the same type, text and cause) are equal. The load of an ordinary record has no Python frame between the access and the storage. A replaced `setstate` of ZODB's `Connection` gets the load; the `setstate` of the early locks gets it for a storage with hot classes only. `install(db)` makes the pooled connection a `zoe.reader.Connection`.
- The switch of the tests: with `ZOE_TEST_READER=on`, `conftest.py` also sets `DB.klass` to `zoe.reader.Connection`, so each connection that a database opens has the method of section A. The class then has the module name of ZODB's `Connection`, because the doctests of ZODB print a connection with it. `just conformance` runs the suites of ZODB with it.

### G. Consequences

- A load in a Zope process on a ZOE storage no longer runs the Python frame of `Connection.setstate` nor the call of `setGhostState`. The `terms` rows of `just bench profile` and the Plone request of `just bench profile plone` measure it against the reader alone.
- The method copies the lines of a method of ZODB. A new version of ZODB that changes `Connection.setstate` needs this record and the method to follow it, as section B says.
- A load result of the storage that is not a tuple of two items is unpacked by iteration; it raises `ValueError`, or the error of the iteration, as the unpack of Python does, with a text that can differ.
