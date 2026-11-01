# Finding: a reader in Rust in place of `Connection._reader`

Card ZOE-274, requirements N4 (the `read_hot` rows) and F28. The input is `docs/spikes/load-path.md` (ZOE-275), whose harness this finding reuses. The card asked for a path in Rust from the ghost to the client cache; the decision on the card narrowed the spike to option (b): a reader of ZOE in place of `Connection._reader`, which keeps `Connection.setstate` and the load through the client. The prototype is on the branch `spike/ZOE-274` at [`020ae125`](https://github.com/adeweb-be/ZOE/tree/020ae125), not for merge.

**Verdict.** Go for (b), with two conditions.

- **A hit at 1 KB** costs 3.38 µs with the reader on CPython 3.13 and 3.23 µs on 3.14t, against 6.32 µs and 6.69 µs without it, and 8.55 µs and 8.59 µs for `ZEO.ClientStorage` in the same runs: from 1.35× to 2.53× ZEO on 3.13, and from 1.28× to 2.66× on 3.14t. The reader removes more than ZOE-275 estimated (39 %): 47 % and 52 % of the hit, because the decode in Rust also costs less than the C unpickler. A whole `getState` costs 0.39 µs in Rust, against 2.9 to 3.4 µs through `ObjectReader`, of which the decode of the C unpickler alone is 1.0 µs. N4 asks for 3× on 3.14t, so (b) alone does not reach it.
- **The first condition** is the two fixes of ZOE-275 that need no new API: `loadBefore` without `_with_server` and its lambda (part of 0.26 to 0.30 µs), and the check of the process without `getpid` (about 0.2 µs). With them, a hit at 1 KB is about 2.8 µs on 3.14t: about 3.1× ZEO, with no margin. **The second condition** is option (a) after (b): a `setstate` of the ZOE Connection class that calls the reader in Rust, about 0.8 µs less. With the fixes and (a), a hit at 1 KB is about 2.0 µs on 3.14t, about 4.3×. The hook of (b) is already that Connection class, so (a) needs no other hook. The full path, through the client cache without `MVCCAdapterInstance.load` and `zoe.zeo`, would give about 1.5 µs, about 5.7×. These three numbers are estimates from the shares of py-spy, not measures.
- **A hit at 100 KB** costs 25.1 µs on 3.13 and 27.6 µs on 3.14t with the reader, against 30.9 and 33.5 µs without it, and 29.4 and 34.1 µs for ZEO: 1.17× and 1.24×. The copy of the entry into a `bytes`, its checksum and the copy of the data into the state stay. The full path removes the first copy, about 6 µs: about 1.7×. The hot reads at 100 KB stay under 3× with every option, as the decision on the card accepts.
- **A miss at 1 KB** costs 51.3 µs on 3.13 and 51.4 µs on 3.14t with the reader, against 59.8 and 61.9 µs: 2.67× and 2.75× ZEO. After a prefetch, a read costs 8.9 and 8.4 µs, against 10.6 and 11.6 µs.
- **The oracle.** On the `small` fixture of B5, the reader decodes all 31,138 current records, and each state pickles to the bytes of the state of `ObjectReader.getState`, with each reference to the object that the pickle cache holds for its oid, of the same class. No record of the fixture takes the Python path, and none of the 293,644 loads of the Plone requests. In the conformance suites and the `client` tests, 21 of 384,958 records took the Python path on 3.14t: references into another database, weak references and `Broken` classes, which the tests of ZODB make on purpose. These suites pass with the reader in every connection, on 3.13 and 3.14t, but for one doctest of ZEO that fails on `main` too.
- **A Plone request** on the `small` fixture, in one Zope thread with an empty object cache and a warm client cache, takes 90.3 ms for a folder listing and 85.4 ms for a document view with the reader, against 93.8 and 99.0 ms without it: 4 % and 14 % less, about 7 µs per load. ZOE-275 estimated about 7 and 15 points for the whole path in Rust. The Python calls under `Connection.setstate` go from 41 to 26 per load; most of those left are the `__setstate__` in Python of the values of a state, such as `DateTime`.
- **A `persistent` in Rust is not needed.** The reader resolves a reference that the pickle cache holds in 0.15 µs on 3.13 and 0.21 µs on 3.14t, and makes a new ghost in 0.42 and 0.60 µs, with the C calls of `cPickleCache`. A cache of objects in Rust would save at most the lookup, 0.05 µs a reference. The C of `persistent` is about 15 % of a hit with the reader (`unghostify`, `__setstate__`, the cache calls), work that a port would do too.
- **The hook.** ZODB gives no attribute for the reader alone: `Connection.__init__` makes an `ObjectReader` by name. The one class hook is `DB.klass`, so (b) needs a Connection class of ZOE whose `__init__` gives the reader. The storage cannot set `DB.klass`: `registerDB` receives the MVCC adapter, not the database. Zope sets it with `connection-class` in `<zodb_db>`, but after `DB()` has pooled one stock connection, which a Zope of one thread then uses for each request. A subscriber of `zoe.plone` at `IDatabaseOpenedWithRoot` that calls `zoe.reader.install(db)` reaches every connection, with no line in `zope.conf`.

## Question

The card asks whether one path in Rust can replace the Python frames between a ghost and the client cache with the same result for every object, how much of the time of ZOE-275 it removes at 1 KB and 100 KB, on a hit and on a miss, on 3.13 and 3.14t, and whether it needs a cache of objects in Rust. The decision on the card builds option (b) only: a reader of ZOE in place of `Connection._reader`. It replaces the unpickler setup, the class lookup and the frames of `ObjectReader`, and keeps `Connection.setstate`, `MVCCAdapterInstance.load`, `zoe.zeo.ClientStorage.loadBefore` and the client cache. This finding says what option (a) and the full path would add, as an estimate from the numbers of (b).

## Method

**The prototype.** `zoe._zoe.Reader` in `crates/zoe-py/src/reader.rs` (about 850 lines) and `zoe.reader` in `python/zoedb/src/zoe/reader.py`:

- `zoe.reader.Reader` derives from `zoe._zoe.Reader` and from ZODB's `ObjectReader`, in that order. The Rust base gives `setGhostState(obj, pickle)` and `getState(pickle)`; `ObjectReader` gives the rest (`getGhost`, `getClassName`, `_persistent_load` and the loaders) and its `__init__`, which sets `_conn`, `_cache` and `_factory`. `Connection._resetCache` sets `_reader._cache`, and the reader reads `_cache` at each decode, so a new cache reaches it.
- `getState` runs one decoder over the record: the class pickle, then the state pickle, with one memo for both, as one unpickler loads them. It takes the opcodes that ZODB writes at protocol 3 and builds the objects as the C unpickler of zodbpickle does (`_pickle_33.c` of the fork 4.5+zoe.1): the ints, `BINFLOAT`, `BINUNICODE` with `surrogatepass`, `BINBYTES` and `SHORT_BINBYTES`, the Python 2 str as ASCII strict, the tuples, lists and dicts with their marks, `APPEND`, `APPENDS`, `SETITEM`, `SETITEMS`, `GLOBAL`, `REDUCE`, `NEWOBJ` (the `tp_new` of the class), `BUILD` (`__setstate__`, else the instance dict with interned keys and the slot state), `BINPERSID`, the puts and gets, `DUP`, `POP` and `POP_MARK`. The constants of the opcodes come from `zoe_pickle::opcode`.
- A `GLOBAL` looks its module line and name line up in a table of the reader. A miss calls `_factory(_conn, module, name)`, as the `find_global` closure of `ObjectReader` does, and keeps the result, unless it is a subclass of `ZODB.broken.Broken`.
- `BINPERSID` does what `_persistent_load` does: for `(oid, klass)`, `cache.get(oid, None)`, else the class (through the table when `klass` is a pair of names), `klass.__new__(klass)` (on `TypeError`, `conn.get(oid)`) and `cache.new_ghost(oid, obj)`; for a bare oid, `cache.get` or `conn.get`.
- `setGhostState` calls `obj.__setstate__(state)` with the state of the Rust path, or with the state of `ObjectReader.getState` when the record takes the Python path.
- A record takes the Python path whole for any other construct: another opcode, a protocol above 3, a class that the factory gives as `Broken`, a weak reference, a reference into another database, a persistent id of another shape, a Python 2 str that is not ASCII, an int of more than 16 bytes, and malformed bytes (a short read, a missing mark or memo entry). The Python path then raises what it raises. An exception of a call into Python (the factory, `REDUCE`, `__setstate__`, the cache) leaves the reader as it leaves the C unpickler. The reader counts the decoded records and the fallbacks by reason, in the process (`zoe.reader.counts()`).
- `zoe.reader.Connection` is ZODB's `Connection` with a `Reader` made in `__init__`. `zoe.reader.install(db)` sets `db.klass` to it and gives a `Reader` to each connection of the pool of `db`.

The reader adds no lock to the pickle cache: a connection and its reader serve one thread at a time, as ZODB requires. The table of globals has a mutex, held for a lookup or an insert only, and no Python code runs while a call holds it. On 3.14t the GIL stayed off in every run.

**The oracle.**

1. `zoedb/tests/conformance` (the ZODB test modules and storage suites against `zoe.Storage`, the native suites, the ZEO suites with the native client) and `zoedb/tests/client`, on 3.13 and 3.14t, with `SPIKE_ZOE_READER=1`. The switch is in the `conftest.py` of the tests: it sets `ZODB.Connection.ObjectReader` to `zoe.reader.Reader`, so each Connection of the run, also those that the tests of ZODB make on their own storages, has the reader. That is a switch of the tests, not a hook for a product.
2. `zoedb/tests/test_zoe_reader_spike.py`: the states of ints of each width, floats, str with a lone surrogate, bytes of 70,000 bytes, tuples of each length, shared lists, sets, frozensets, an `OrderedDict`, subclasses of list and dict, objects with slots, with `__setstate__` and with both slots and a dict, dicts and lists of 2,500 items, and persistent references, against `ObjectReader`; a weak reference and a reference into another database take the Python path.
3. `bench/plone/src/plonebench/inzope/reader_oracle.py`, run in Zope by `python -m plonebench.loadpath --oracle`: every current record of the FileStorage of the `small` fixture, opened read-only, with the class factory of Zope, in two connections of one database, one with the reader and one with `ObjectReader`. The two states must pickle to the same bytes when each persistent object pickles as its oid and its class, and each persistent object of the state of the reader must be the object that the pickle cache of its connection holds for its oid.

**The measure.** `just bench profile terms` of ZOE-275 with `--reader`, which calls `zoe.reader.install(db)` before the first `db.open()`, on the native client, and without it on the native client and on ZEO, in the same session: the same corpus, rounds and layers as ZOE-275. With the reader, the layers of the unpickler setup and of the class lookup still time ZODB's Python path, which the load no longer takes, so this finding reads `getState` (now the whole decode in Rust) and the load. py-spy sampled the hit loops with the reader on 3.13 (`just bench profile loop --reader --py-spy`). `just bench profile plone` of ZOE-275 ran with and without `--reader`, which calls `zoe.reader.install` on the database of Zope.

**Machine and builds.** The workstation of ZOE-275: Intel Core Ultra 7 155H, 30 GB, Linux 7.0, every run pinned with `taskset` to CPUs 0 to 11, one heavy job at a time on the machine, the 1-minute load at 0.5 to 1.5 at the start of each timing run. `main` at `f41d0991` with the prototype, release builds, rustc 1.99, CPython 3.13.15 and 3.14.7t, ZODB 6.3, ZEO 6.2 and the forks of persistent, zodbpickle and BTrees in the benchmark environments. The Plone instance runs Plone 6.1.5 on CPython 3.13.15 with the versions of `plone/versions.cfg`, as in ZOE-275. The raw results are not in the repository: the harness on the prototype branch gives them again.

## Results

### The hit, the miss and the prefetch

µs per load, the median of the rounds. "off" is ZODB's `ObjectReader`, "on" the reader.

| | native 3.13 off | native 3.13 on | ZEO 3.13 | native 3.14t off | native 3.14t on | ZEO 3.14t |
| --- | --- | --- | --- | --- | --- | --- |
| **hit, 1 KB** | 6.32 | **3.38** | 8.55 | 6.69 | **3.23** | 8.59 |
| × ZEO | 1.35 | **2.53** | | 1.28 | **2.66** | |
| **hit, 100 KB** | 30.90 | **25.10** | 29.37 | 33.45 | **27.61** | 34.14 |
| × ZEO | 0.95 | **1.17** | | 1.02 | **1.24** | |
| miss, 1 KB | 59.76 | 51.31 | 136.94 | 61.87 | 51.40 | 141.53 |
| miss, 100 KB | 132.83 | 119.24 | 221.35 | 131.24 | 123.98 | 254.79 |
| prefetch, then the reads, 1 KB | 10.58 | 8.93 | – | 11.65 | 8.41 | – |
| prefetch, then the reads, 100 KB | 53.25 | 49.07 | – | 63.22 | 54.96 | – |

The reader off agrees with ZOE-275 within 0.3 µs at 1 KB (6.62 and 6.90 µs there). ZEO is faster here than in ZOE-275 at 100 KB on 3.14t (34.1 against 41.8 µs); the table compares runs of one session.

**The terms of a hit at 1 KB.** µs per load, as differences of layers:

| Term | 3.13 off | 3.13 on | 3.14t off | 3.14t on |
| --- | --- | --- | --- | --- |
| client cache | 0.45 | 0.48 | 0.45 | 0.45 |
| `ClientStorage.loadBefore` | 0.25 | 0.26 | 0.30 | 0.30 |
| `MVCCAdapterInstance.load` | 0.19 | 0.19 | 0.15 | 0.13 |
| `getState`: unpickler setup, class lookup, decode | 2.93 | 0.39 | 3.38 | 0.39 |
| `__setstate__` | 0.27 | 0.24 | 0.25 | 0.17 |
| bookkeeping | 0.21 | 0.20 | 0.30 | 0.27 |
| rest: `unghostify`, the frame of `Connection.setstate`, the call of the reader | 1.97 | 1.59 | 1.75 | 1.51 |
| **load** | **6.32** | **3.38** | **6.69** | **3.23** |

At 100 KB, `getState` goes from 11.04 to 6.24 µs on 3.13 and from 11.56 to 6.42 µs on 3.14t: the new `bytes` of the data of 100 KB is most of what remains. The client cache stays at 11.0 and 10.6 µs.

**Python calls per load.** 13 without the reader, 5 with it: `Connection.setstate`, `MVCCAdapterInstance.load`, and the three frames of `zoe.zeo` (`loadBefore`, `_with_server`, the lambda). `ZEO.ClientStorage` makes 17 on a hit.

**The references.** The decode of the `PersistentMapping` of the corpus costs, per reference, 0.15 µs on 3.13 and 0.21 µs on 3.14t with the reader when each child is in the pickle cache, against 0.63 and 0.88 µs; and 0.42 and 0.60 µs when each child becomes a new ghost, against 0.95 and 1.42 µs. A Python callback of `ObjectReader` costs 0.19 to 0.20 µs alone, and `cPickleCache.get` 0.05 µs.

**The frames, py-spy on 3.13, hit with the reader.** The share of the samples under the timed loop, each sample in one part:

| Part | hit 1 KB | hit 100 KB |
| --- | --- | --- |
| the call into `Client.load_before`: the client cache, its copy and checksum, `getpid`, pyo3 | 24.4 | 63.8 |
| `Connection.setstate` and the bookkeeping | 21.7 | 3.7 |
| decode in Rust (the objects of the state, the table, the references) | 12.0 | 26.1 |
| frames of `zoe.zeo` | 10.7 | 1.7 |
| `MVCCAdapterInstance.load` | 8.3 | 1.3 |
| `unghostify` (C) | 8.3 | 1.2 |
| outside a load: the loop, the access | 6.3 | 1.0 |
| the call of `setGhostState` into Rust | 4.7 | 0.8 |
| `__setstate__` (C) | 3.6 | 0.4 |

At 1 KB, `getpid` alone has 12.0 % of the samples, against 3.1 % in ZOE-275; py-spy stops the process at each sample, so it may count a system call above its cost, and the layer times give it about 0.2 µs. At 100 KB, the copy of the entry (`Cache::copy` and its `memmove`) and the xxh3 of the entry (18.8 %) are most of the call into the client, and the copy of the data into the new `bytes` of the state is most of the decode.

### The oracle

**The tests.** With the reader in every connection:

| | 3.13 | 3.14t |
| --- | --- | --- |
| `zoedb/tests/conformance`, `zoedb/tests/client`, the tests of the reader | 226 passed, 2 skipped, 3 failed | 229 passed, 2 failed |
| records decoded by the reader | – | 384,937 |
| records of the Python path | – | 21: 14 references into another database, 5 weak references, 2 `Broken` classes |

Two failures are the same doctest of ZEO, `drop_cache_rather_than_verify.txt` (`len(db.storage._cache) <= 1` gives `False`), in the ZEO suites with the native client against `zoe serve` and against ZEO. It fails without the reader too, on `main` at `f41d0991`: it is not the reader's. The counts of 3.13 were lost: the tests of the reader set them to zero in that run. In that run, a test of the reader that asked for a stock `ObjectReader` also failed on the switch itself; it makes its `ObjectReader` itself since, and passes on both builds.

**The `small` fixture of B5.** `just bench plone prepare` built it on this machine (`small-ba01ff255313`); its FileStorage has 31,138 current records.

| | records |
| --- | --- |
| current records | 31,138 |
| decoded by the Rust path | 31,138 (100 %) |
| state that pickles to the bytes of `ObjectReader.getState` | 31,138 |
| a reference that is not the object of the pickle cache for its oid | 0 |
| the Python path | 0 |

A first run found one bucket of `five.intid` whose two states pickled to different bytes. The states were equal; only the sharing of one oid differed. A name of the oracle (its loop variable) kept a ghost of the reader's connection alive from an earlier record, so that ghost kept the oid bytes of that record, while the other connection made the ghost again with the oid bytes of the bucket. With no name left holding a ghost, the two connections hold the same ghosts, and every record compares equal. A run of the same comparison outside Zope, with ZODB's class factory, also gave no difference.

### A Plone request

One Zope thread, `cacheMinimize()` before each request, the median of the 90 requests of each kind after the first visit of each path, the requests of ZOE-275:

| Request | without the reader | with the reader | less | loads | misses of the client cache |
| --- | --- | --- | --- | --- | --- |
| folder listing | 93.8 ms | 90.3 ms | 3.7 % | 451 to 457 | 0 |
| document view | 99.0 ms | 85.4 ms | 13.7 % | 2,223 | 0 |

The reader decoded 293,644 records in the run (the timed requests and the counted ones), and none took the Python path. Per load, the Python calls under `Connection.setstate` go from 41.2 to 26.3, and the Python callbacks of `persistent_load` from 2.6 to none. ZOE-275 measured 105.5 and 107.9 ms without the reader in its run; the folder listing gains less than its estimate, because most of its load time is the `__setstate__` of `DateTime` in the metadata of the catalog, which every path keeps.

## What remains, and what (a) and the full path would add

With the reader, a hit at 1 KB on 3.13 is 3.38 µs. From the shares of py-spy times that time:

| Term | µs | (b), the reader | (a), `setstate` of the ZOE Connection in Rust | full path |
| --- | --- | --- | --- | --- |
| client cache: lookup, copy, checksum | about 0.3 | kept | kept | kept, without the copy into a `bytes` |
| `getpid` and pyo3 of the call into the client | about 0.3 | kept (ZOE-275 fix 2 removes `getpid`) | kept | removed: one check for the whole path |
| frames of `zoe.zeo` | 0.36 | kept (ZOE-275 fix 1 removes two of three) | kept | removed |
| `MVCCAdapterInstance.load` | 0.28 | kept | kept | removed: the reader reads `_start` of the instance |
| `Connection.setstate` and the bookkeeping | 0.73 | kept | removed, but for the bookkeeping as C calls (about 0.05) | removed, the same |
| the call of `setGhostState` | 0.16 | kept | removed | removed |
| decode in Rust | 0.41 | new | kept | kept |
| `unghostify`, `__setstate__` (C) | 0.40 | kept | kept | kept |
| outside a load | 0.21 | – | – | – |
| **hit, 1 KB, 3.13** | **3.38** | **3.38**, 2.5× ZEO | **about 2.5**, 3.4× | **about 1.6**, 5.3× |

With the fixes 1 and 2 of ZOE-275, (b) is about 2.95 µs on 3.13 (2.9×), and (b) with the fixes and (a) about 2.1 µs (4.1×). On 3.14t the terms are the same within 0.15 µs: (b) is 3.23 µs (2.7×), (b) with the fixes about 2.8 µs (3.1×), (a) without the fixes about 2.4 µs (3.6×), (a) with them about 2.0 µs (4.3×), the full path about 1.5 µs (5.7×).

At 100 KB, (a) removes the same frames, about 1 µs. The full path also decodes from the entry of the client cache, without the copy into the `bytes` of the pickle, about 6 µs of the 11 µs of the client cache: about 17.5 µs on 3.13 and 20 µs on 3.14t, 1.7× ZEO. The checksum of the entry (about 4.7 µs) stays under the rule of ADR 0034, and the copy of the data into the `bytes` of the state (about 6 µs) stays in every path, because the state needs its own `bytes`.

The full path also needs what (b) does not: the `_start` of `MVCCAdapterInstance`, a private attribute, and the rule of `MVCCAdapterInstance.load` for a record that `loadBefore` does not find (`ReadConflictError`, not `POSKeyError`). It works on `zoe.zeo.ClientStorage` only.

## Each line of `Connection.setstate` and of `ObjectReader`

ZODB 6.3. "Python" means the prototype leaves the line as ZODB has it.

| Line | Where the prototype does it |
| --- | --- |
| `Connection.setstate`: `oid = obj._p_oid`; `if self.opened is None: ... raise ConnectionStateError` | Python |
| `p, serial = self._storage.load(oid)`: `MVCCAdapterInstance.load`, `ClientStorage.loadBefore`, `Client.load_before` | Python, then the client cache in Rust, as without the reader |
| `self._load_count += 1` | Python |
| `self._reader.setGhostState(obj, p)` | Rust: `zoe._zoe.Reader.setGhostState` |
| `obj._p_serial = serial`; `self._cache.update_object_size_estimation(oid, len(p))`; `obj._p_estimated_size = len(p)` | Python |
| `if isinstance(obj, Blob): ... loadBlob(oid, serial)` | Python. A blob needs no fallback: its record is an ordinary record, and these lines stay in `Connection.setstate` |
| `except ConflictError: raise`; `except: self._log.exception(...); raise` | Python: an exception of the reader reaches it as one of `ObjectReader` |
| `ObjectReader.setGhostState`: `state = self.getState(pickle)`; `obj.__setstate__(state)` | Rust: the decode, then the call of `__setstate__` |
| `getState`: `_get_unpickler(pickle)`: a `BytesIO`, the `find_global` closure, `PersistentUnpickler`, `Unpickler.__init__` | none: one decoder reads the `bytes` of the record |
| `getState`: `unpickler.load()` of the class metadata | Rust: decoded, its class from the table; its memo stays for the state |
| `getState`: `unpickler.load()` of the state; `except EOFError: log; raise` | Rust; a short record takes the Python path, which logs and raises |
| `find_global(modulename, name)`: `factory(conn, modulename, name)`, `DB.classFactory`, `ZODB.broken.find_global` | Rust: `_factory(_conn, module, name)` once per module and name and reader, then the table; a `Broken` class is not kept and the record takes the Python path |
| `_persistent_load(reference)`: a tuple, a bytes or str, a list | Rust for a tuple and for a bytes or str; a list takes the Python path |
| `load_persistent(oid, klass)`: the str oid as ASCII bytes; `self._cache.get(oid, None)`; `_get_class(*klass)` for a pair of names; `persistentBroken` for a `Broken` class; `klass.__new__(klass)`, `self._conn.get(oid)` on `TypeError`; `self._cache.new_ghost(oid, obj)` | Rust, as calls into `cPickleCache` and the class; a `Broken` class takes the Python path |
| `load_oid(oid)`: `self._cache.get(oid, None)`, else `self._conn.get(oid)` | Rust |
| `load_persistent_weakref`, `load_multi_persistent`, `load_multi_oid` | the Python path, for the whole record |
| `getGhost`, `getClassName` | Python: `Connection.get`, the export and the import call them, not a load |

## The hook

ZODB gives two places for the reader, and neither is the reader alone:

- `Connection.__init__` ends with `self._reader = ObjectReader(self, self._cache, self._db.classFactory)`, which reads the name `ObjectReader` of the module `ZODB.Connection`. Setting that name is a monkeypatch: the tests of the spike use it as their switch, a product cannot.
- `DB.klass` is the class that `DB.open` makes. A subclass of `Connection` whose `__init__` gives the reader is the hook of (b), and of (a) too. It costs nothing per load.

What sets `DB.klass`:

1. **The storage cannot.** `DB()` wraps a storage that is not an `IMVCCStorage` in an `MVCCAdapter`, which calls `storage.registerDB(self)` with itself. So `zoe.zeo.ClientStorage` never sees the database, and the storage stanza alone cannot reach `DB.klass`.
2. **Zope's `connection-class`.** `<zodb_db main>` takes `connection-class zoe.reader.Connection`, and `ZopeDatabase.open` sets `DB.klass` with it. But `DB.__init__` already opened and pooled one stock `Connection` to check the root, and `DB.open` gives the pooled connection first. A Zope of one thread uses that connection for each request, without the reader.
3. **A subscriber.** Zope notifies `IDatabaseOpenedWithRoot` with the main database once at its start. A subscriber of `zoe.plone` that calls `zoe.reader.install(db)` sets `DB.klass` and gives a reader to the connections that the pool already holds. That writes `_reader` of a Connection, a private attribute that ZODB itself writes in `__init__` and whose `_cache` it writes in `_resetCache`. It needs no line in `zope.conf`, only the `plone` extra.
4. **An application outside Zope** calls `zoe.reader.install(db)` after `DB()`, or uses a subclass of `DB` with `klass = zoe.reader.Connection`.

The harness and the oracle call `zoe.reader.install(db)` (option 3 without the subscriber). A build takes option 3 for Plone and option 4 elsewhere, and `connection-class` works with it.

## Verdict on a `persistent` in Rust

Not needed. The reader resolves every reference with the C calls of `cPickleCache` (`get`, `new_ghost`) and the class (`__new__`): 0.15 to 0.21 µs per reference that the cache holds, 0.42 to 0.60 µs per new ghost, in the decode of a mapping, against 0.63 to 1.42 µs with `ObjectReader`. A cache of objects in Rust would save at most the lookup, 0.05 µs a reference; a Plone request decodes 2.6 references per load (ZOE-275). With the reader, the C of `persistent` is about 15 % of a hit at 1 KB: `unghostify` (8.3 %), `__setstate__` (3.6 %), the calls of the cache (about 3 %). A port would do the same work. No term of this finding needs a `persistent` in Rust.

## Proposal for the specification

For the human to decide. The non-goal:

> - Rewriting `persistent` and `BTrees` in Rust. That is a separate, client-side project ("What stays on persistent and BTrees" says what is left there). The decode of a record into the state of a ghost may move into Rust, as a reader of ZOE in place of ZODB's `ObjectReader` that calls the C of `persistent` and keeps no cache of objects of its own (`docs/spikes/ghost-to-cache.md`).

And at the end of "What stays on persistent and BTrees":

> A load from the client cache spends most of its time in the Python frames of ZODB and of the client, and little in the C of `persistent` (`docs/spikes/load-path.md`). A reader of ZOE in place of `Connection._reader` decodes the record in Rust, finds each class once through the class factory of the database, and resolves each persistent reference with `cPickleCache.get` or a new ghost; a record that it does not decode goes to ZODB's `ObjectReader` whole (`docs/spikes/ghost-to-cache.md`).

## Build cards

If the human takes the verdict, in this order:

1. **The reader of ZOE.** `zoe.reader.Reader`, `zoe.reader.Connection` and `zoe.reader.install(db)` (a new public Python API), the subscriber of `zoe.plone` at `IDatabaseOpenedWithRoot`, the counts of the fallbacks as metrics, and the oracle of this spike as its acceptance test: the conformance suites, the `client` tests and the `small` fixture with the reader. It needs an ADR: the reader, its set of opcodes, the rule that a record decodes as `ObjectReader` decodes it or goes to `ObjectReader` whole, the table of classes (it assumes that the class factory gives the same class for a module and a name during the life of a connection, and keeps no `Broken` class), and the hook.
2. **The two fixes of ZOE-275** for a hit: `zoe.zeo.ClientStorage.loadBefore` calls the native client without `_with_server` on the path of a connected client, and the native client checks the process without `getpid`. No public API changes, so no ADR.
3. **Option (a).** `zoe.reader.Connection.setstate`, with the lines of ZODB's `setstate` and the reader in Rust, `Blob` and a closed connection to ZODB's method. It overrides a method of ZODB, so it needs an amendment of the ADR of card 1: which versions of ZODB it follows and how its tests compare it with ZODB's `setstate`.
4. **The full path**, after the numbers of card 3: the load through the client cache of `zoe.zeo.ClientStorage` from the `_start` of the MVCC instance, decoded from the entry. It reads private state of `MVCCAdapterInstance`, so it needs an ADR of its own, and the rule of ADR 0034 stays.

## Limits

- One workstation, not the reference machine; each number comes from one run. A term of less than 0.3 µs is within the noise of the layer times. The estimates of (a) and of the full path are shares of py-spy on 3.13 times the measured load; they are not measured.
- py-spy does not read a process of 3.14t; the layer times stand for it there.
- The oracle compares the pickles of the states, not the objects after `__setstate__`: both paths call the same `__setstate__` with the states, so equal states give equal objects.
- The table of classes keeps a class for the life of a reader, so a module that binds a name to another class while a connection lives is not seen by the reader of that connection. The table keeps no `Broken` class, so a class that imports later (the "repaired global" of `ZODB.broken`) is found then. The conformance suites have no other such case.
- The Plone instance runs the stock persistent, zodbpickle and BTrees, not the forks.
