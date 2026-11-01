# Finding: where a load spends its time between the ghost and the client cache

Card ZOE-275, requirements N4 and F28. The harness is `bench/shootout/src/shootout/loadpath.py` (`just bench profile`), with `bench/plone/src/plonebench/loadpath.py` for the Plone requests. ZOE-273 (the native client prefetches the references of a record) and ZOE-274 (a path in Rust from the ghost to the client cache) read this finding.

**Verdict.** On the native client, ZODB and its Python frames take most of a hit, and the server takes most of a miss.

- **A hit at 1 KB** costs 6.6 µs on CPython 3.13 and 6.9 µs on 3.14t, against 9.2 µs for `ZEO.ClientStorage` on both: 1.4× and 1.3×, where N4 asks for 3× on 3.14t. The client cache is 0.55 µs of it (9 %). About 60 % of the hit is Python frames and the objects that they make: a new unpickler for each record (24 %), `Connection.setstate`, `ObjectReader`, `MVCCAdapterInstance.load` and the three frames of `zoe.zeo` (26 %), the class lookup through four Python functions (6 %), and the check of the process and the glue of pyo3 (4 %). A path in Rust from the ghost to the client cache (ZOE-274) removes these terms. It keeps the decode of the state, `__setstate__`, `unghostify`, the bookkeeping and the cache: about 2.5 µs on 3.13 and 2.6 µs on 3.14t, so about 3.6× and 3.5× ZEO, if the decode in Rust costs what the C unpickler costs. That is above 3×, with a small margin.
- **A hit at 100 KB** costs 30 µs on the native client on 3.13 and 32 µs on 3.14t, against 30 µs and 42 µs for ZEO: 1.0× and 1.3×. Two copies of the record and one checksum take two thirds of it: the cache copies the entry into a `bytes` (28 %) and checks its xxh3 (15 %), and the unpickler copies the data again (23 %). The Python frames and the glue of the call are 21 %. A path in Rust that decodes from the entry and copies the data once gives about 15 µs on 3.13 and 16 µs on 3.14t: 1.9× and 2.6×. 3× at 100 KB on 3.14t also needs a hit that does not hash the whole entry: then about 11 µs, 3.7×.
- **A miss at 1 KB** costs 57 µs on 3.13 and 62 µs on 3.14t, against 145 µs and 169 µs for ZEO: 2.6× and 2.7×. 86 % of the miss is the wait for the server. A prefetch of the children before the reads, which ZOE-273 does after the miss of their container, brings a read to 12 to 14 µs per child: about 12× ZEO on both builds. At 100 KB, a miss costs 138 to 141 µs, and 57 to 73 µs per child with the prefetch, against 235 µs and 403 µs for ZEO.
- **The callbacks.** A `persistent_load` callback costs 0.20 µs on 3.13 and 0.27 µs on 3.14t, of which the lookup in `cPickleCache` is 0.05 to 0.07 µs. A record of the `small` fixture of B5 holds 1.3 references on average, and a load of a Plone request decodes 2.6. So a path in Rust that resolves each reference with a call into `cPickleCache` keeps at most about 0.2 µs per load of the lookups, and a cache of objects in Rust would save at most that. **A `persistent` in Rust is not needed for the callbacks.**
- **A Plone request** on the `small` fixture, in one Zope thread, with an empty object cache and a warm client cache, loads 456 objects for a folder listing and 2,222 for a document view, with no miss. The loads take 24 % of the time of a folder listing and 40 % of a document view: 56 µs and 19 µs per load. Most of that stays with any path: the decode of the state, and the `__setstate__` of `DateTime`, which is Python code, for the dates in the metadata of the catalog (8 and 9 points of the request alone). A path in Rust removes about 7 points of a folder listing and 15 points of a document view.
- **3.14t** moves no term. cProfile gives the same shares within 3 points on 3.13 and 3.14t, and the layer times agree within 0.3 µs at 1 KB.

So ZOE-273 is the lever of the cold reads, and ZOE-274 is the lever of the hot reads at 1 KB. Neither reaches 3× in the hot reads at 100 KB: the copies and the checksum of the client cache decide there.

## Question

The card asks: of the time of one load through ZODB on the native client, how much is `Connection.setstate` and `MVCCAdapterInstance.load`, how much `zoe.zeo.ClientStorage.loadBefore` and the call into `zoe._zoe.Client`, how much the C unpickler, how much the `persistent_load` callbacks (and how many per record), how much `__setstate__`, and how much the bookkeeping (`update_object_size_estimation`, `_p_serial`, `_p_estimated_size`, `_load_count`)? How does the split change at 100 KB, on a miss, on 3.14t, and in a Plone request? Which terms does a path in Rust from the ghost to the client cache remove, and does such a path need a cache of objects in Rust?

`docs/spikes/client-cache.md` measured a hit through ZODB at 5.6 µs on a prototype of the cache, of which 0.2 µs was the lookup in Rust. `docs/spikes/phase3-budget.md` measured 16 µs of ZODB work per object of `read_hot` at 1 KB on 3.13, on a client without a cache.

## Method

**The load.** A read of an attribute of a ghost calls `unghostify` in `cPersistence`, which calls `Connection.setstate(obj)`. That method calls `MVCCAdapterInstance.load(oid)`, which calls `ClientStorage.loadBefore(oid, start)`. On the native client, `loadBefore` calls `_with_server`, which calls a lambda, which calls `Client.load_before` of `zoe._zoe`: the lookup in the client cache, in Rust, or a round trip to the server on a miss. Then `ObjectReader.setGhostState` calls `getState`, which builds an unpickler (`_get_unpickler`, `PersistentUnpickler`, `Unpickler.__init__`, on a `BytesIO`), decodes the class metadata (one `find_global` through `DB.classFactory` and `ZODB.broken.find_global`) and the state (a `persistent_load` callback for each reference), then calls `obj.__setstate__(state)`. `Connection.setstate` then counts the load and sets `_p_serial`, the size estimate of the cache and `_p_estimated_size`.

**The harness.** `just bench profile terms` starts a server on a new store, opens a `DB` on a client of it, and writes the corpus of zodbshootout: a `PersistentMapping` of its `PObject`s, 1,000 of 1 KB (pickles of 2.8 KB) or 200 of 100 KB (pickles of 103 KB). The clients are the native client (`zoe.zeo.ClientStorage` against `zoe serve`) and `ZEO.ClientStorage` against a ZEO 6 server on a FileStorage, each with a client cache of 256 MB, which holds the corpus, and no other setting changed. A round does what `read_hot` does: `cacheMinimize()`, then a read of one attribute of each object. Then the round runs each layer of the load alone, over the same objects, in a loop of its own:

| Layer | What the loop calls |
| --- | --- |
| load | an attribute of each ghost, after `cacheMinimize()`, less the same loop on loaded objects |
| `MVCCAdapterInstance.load` | `conn._storage.load(oid)` |
| `ClientStorage.loadBefore` | `storage.loadBefore(oid, start)` |
| client cache | `storage._native.load_before(oid, start)`; on ZEO, `storage._cache.loadBefore(oid, start)` |
| `getState` | `conn._reader.getState(pickle)` |
| unpickler setup | `conn._reader._get_unpickler(pickle)` |
| class metadata | `_get_unpickler(pickle).load()` |
| `__setstate__` | `obj.__setstate__(state)` on a loaded object |
| bookkeeping | the four lines of `Connection.setstate` after the decode, and its `isinstance(obj, Blob)` |

A term is the difference of two layers of the same round, less an empty loop, per load: `ClientStorage.loadBefore` is that layer less the client cache, `MVCCAdapterInstance.load` is that layer less `loadBefore`, the class lookup is the class metadata less the unpickler setup, the state decode is `getState` less the class metadata. The rest is the load less every measured layer: `unghostify` and the frames of `Connection.setstate`, `setGhostState` and `getState`. A miss round empties the client cache before the read and before each layer that reaches it, and reads the loads that `zoe serve` counted on its admin port: one per object in every round. On the native client, the miss round also prefetches every oid with `ClientStorage.prefetch` and reads the objects at once, as after a prefetch of ZOE-273 that follows the miss of the container. A row runs 30 rounds at 1 KB and 15 at 100 KB, after one round that is not counted. The tables give the median of the rounds.

**The profiles.** On 3.13, py-spy 0.4.2 (`record --native`, 997 samples per second, raw format) sampled `just bench profile loop`, which repeats one hit round or one miss round for 20 seconds; `frames` counts the samples under the timed loop only. Each sample goes to one part: the part of the deepest frame of its stack that a rule of `PARTS` matches. py-spy does not read a process of 3.14t (`Failed to find python version from target process`), and `perf` did not run: the host has `kernel.perf_event_paranoid` at 4 and the session had no root. So on 3.14t the layer times and cProfile measure the split, and cProfile ran on 3.13 too, so the two builds compare under the same distortion. cProfile doubles the time of a load, so its shares are not times.

**The counts.** `sys.setprofile` counted the Python calls of one round of 1,000 loads. `scan` read each current record of the FileStorage of the `small` fixture, with `noload`, and counted its references by kind, in the interpreter of the zoeclient Plone instance, which then imported each class to see whether its `__setstate__` is Python code.

**The Plone request.** `just bench profile plone` copies the store of the `small` fixture (built by `just bench plone prepare`), starts `zoe serve` on it, writes the `zope.conf` of an instance of the stack `zoe-native` with the settings of B5 (client cache of 128 MB in memory, object cache of 30,000), and runs one Zope thread in the interpreter of the zoeclient scenario. It sends 200 anonymous requests through the WSGI publisher of Zope in its own process: the listings of 10 published Folders and the views of 10 published Documents of the first section, in turn, with `cacheMinimize()` before each. It records the time, the loads of the connections and the loads that `zoe serve` counted (the misses of the client cache) of each request. Then it sends each path once more with `sys.setprofile` on, to count the loads, the callbacks, the Python calls under `Connection.setstate` and the classes loaded. A second run sampled the requests with py-spy at 199 samples per second.

**Machine and load.** A developer workstation, not the reference machine: Intel Core Ultra 7 155H (6 performance cores with 2 threads each, 8 efficiency cores, 2 low-power cores: 22 logical CPUs), 30 GB of memory, Linux 7.0, the stores on ext4. Every run was pinned with `taskset` to the performance cores (CPUs 0 to 11); the client and the server talk over the loopback. Other jobs used the machine at the same time, up to a load of 70. Each timing run started only when the 1-minute load was below 4: the terms runs started at 3.5 (3.13) and 2.8 (3.14t), and the profiles at 1.8 to 3.8. A profile of the Plone requests that started at 0.8 fell behind its sampling when other jobs took the machine (load 29), and was dropped.

**Builds.** `main` at f2137a0 with the harness of this card, release builds; rustc 1.99. CPython 3.13.15 and 3.14.7t; on 3.14t the GIL stayed off in every process. ZODB 6.3, ZEO 6.2, zodbshootout 0.8.0, and the forks of persistent (6.8+zoe.1), zodbpickle (4.5+zoe.1) and BTrees (6.5+zoe.3). The Plone instance has Plone 6.1.5 and Zope 5.14.2, with ZODB 6.2, persistent 6.5, zodbpickle 4.3 and BTrees 6.3, the versions of `plone/versions.cfg`, on CPython 3.13.15. The raw results are not in the repository: `just bench profile` gives them again.

## Results

### A hit

µs per load, the median of the rounds. The rest holds `unghostify` and the frames of `Connection.setstate`, `setGhostState` and `getState`; as a difference of all the layers, it has the widest range (−0.6 to 3.8 µs at 1 KB on 3.13).

| Term | native 3.13 | native 3.14t | ZEO 3.13 | ZEO 3.14t |
| --- | --- | --- | --- | --- |
| **load, 1 KB** | **6.62** | **6.90** | **9.24** | **9.22** |
| client cache | 0.58 | 0.54 | 3.51 | 2.57 |
| `ClientStorage.loadBefore` | 0.28 | 0.31 | 0.05 | 0.08 |
| `MVCCAdapterInstance.load` | 0.21 | 0.15 | 0.27 | 0.17 |
| unpickler setup | 1.28 | 1.45 | 1.56 | 1.73 |
| class lookup | 0.79 | 1.00 | 0.93 | 1.05 |
| state decode | 0.98 | 0.91 | 1.24 | 1.32 |
| `__setstate__` | 0.30 | 0.26 | 0.37 | 0.30 |
| bookkeeping | 0.23 | 0.28 | 0.29 | 0.36 |
| rest | 1.89 | 1.83 | 1.41 | 1.75 |
| **load, 100 KB** | **30.39** | **32.12** | **29.86** | **41.84** |
| client cache | 10.80 | 10.59 | 12.48 | 14.87 |
| `ClientStorage.loadBefore` | 0.25 | 0.51 | 0.26 | 0.14 |
| `MVCCAdapterInstance.load` | 0.57 | 0.44 | 0.82 | 0.37 |
| unpickler setup | 1.22 | 1.56 | 1.32 | 1.79 |
| class lookup | 0.68 | 0.91 | 0.79 | 0.96 |
| state decode | 8.91 | 9.35 | 9.30 | 9.81 |
| `__setstate__` | 0.42 | 0.49 | 0.51 | 0.56 |
| bookkeeping | 0.23 | 0.36 | 0.28 | 0.42 |
| rest | 7.45 | 7.02 | 3.90 | 13.05 |

The loop and the access of a loaded object cost 0.1 to 0.2 µs per object, and `cacheMinimize()` 0.3 to 0.6 µs per object, outside the load.

**The frames, py-spy on 3.13.** The share of the samples of a hit or a miss in each part, each sample once. One load is 100 %.

| Part | hit 1 KB | hit 100 KB | miss 1 KB | miss 100 KB | ZEO hit 1 KB |
| --- | --- | --- | --- | --- | --- |
| client cache (Rust; ZEO: `ZEO.cache`) | 8.9 | 45.2 | 0.1 | 0.2 | 46.4 |
| the rest of the call into Rust: pyo3, `getpid`, the `bytes` | 4.2 | 1.6 | 5.4 | 11.6 | – |
| wait for the server | – | – | 86.3 | 45.2 | – |
| frames of `zoe.zeo` (`loadBefore`, `_with_server`, the lambda); ZEO: `loadBefore` | 4.7 | 1.7 | 0.5 | 1.3 | 0.7 |
| `MVCCAdapterInstance.load` | 3.6 | 1.2 | 0.4 | 0.9 | 2.1 |
| `Connection.setstate` and the bookkeeping | 10.9 | 3.8 | 1.3 | 3.3 | 6.7 |
| `ObjectReader` frames (`setGhostState`, `getState`) | 8.7 | 3.2 | 1.0 | 3.2 | 6.0 |
| unpickler setup | 24.3 | 8.1 | 1.3 | 5.8 | 16.0 |
| class lookup (`find_global`, `classFactory`, `broken.find_global`) | 6.0 | 1.8 | 0.6 | 1.6 | 3.6 |
| state decode (C unpickler) | 19.8 | 30.3 | 1.4 | 24.1 | 12.4 |
| `__setstate__` (C) | 2.4 | 0.6 | 0.3 | 0.8 | 1.3 |
| `unghostify` (C) | 4.2 | 1.4 | 0.9 | 1.4 | 3.0 |
| outside a load: the loop, the access | 2.4 | 1.1 | 0.4 | 0.7 | 1.7 |

Within the parts:

- **The unpickler setup** is a quarter of a hit at 1 KB, more than the decode of the state. `ZODB._compat.Unpickler` is a Python subclass with a Python `__init__`, and the C `__init__` of zodbpickle looks up `peek`, `read`, `readinto` and `readline` on the `BytesIO` (`_Unpickler_SetInputStream`, 6.4 % alone).
- **The client cache at 100 KB**: the copy of the entry into a `bytes` is 24.7 % plus 3.5 % of `memmove`, the xxh3 of the entry is 14.8 %. **The state decode at 100 KB**: the copy of the data into its own `bytes` is 23.3 %. At 1 KB, the cache is a copy (3.1 %), the checksum (1.6 %) and the lookup.
- **`getpid`** is 3.1 % of a hit at 1 KB (0.2 µs): `Client.load_before` checks the process on each call.
- **On a miss at 100 KB**, the decode costs more than three times what it costs on a hit (24 % of 138 µs), because the reply arrives in memory that another thread wrote. The rest of the call into Rust (12 %) holds the copy of the reply into a `bytes`, `getpid` and the wake-up after the wait.
- **On 3.14t**, cProfile gives the same shares as on 3.13 within 3 points, at 1 KB and at 100 KB. At 1 KB, on 3.14t then on 3.13: unpickler setup 21 and 19 %, C unpickler 15 and 16 %, `ObjectReader` 12 and 12 %, `Connection.setstate` 12 and 12 %, the frames of `zoe.zeo` 8 and 7 %, the class lookup 8 and 7 %, the call into Rust 7 and 10 %.

### A miss, and the prefetch

µs per load, the median of the rounds. The round trip is the layer of `ClientStorage.loadBefore` on a miss less the same layer on a hit. The rows of the prefetch are per object of the corpus, from a second run of 10 rounds; the first run gave 12.9, 12.6, 58.3 and 65.2 µs, and the other rows of the two runs agree within 6 µs.

| | native 3.13 | native 3.14t | ZEO 3.13 | ZEO 3.14t |
| --- | --- | --- | --- | --- |
| miss, 1 KB | 56.5 | 61.9 | 144.6 | 169.0 |
| of which the round trip and the store in the cache | 45.0 | 46.5 | 136.0 | 179.9 |
| prefetch of every oid, then the reads at once | 11.6 | 14.0 | – | – |
| miss, 100 KB | 137.6 | 139.7 | 235.5 | 403.2 |
| of which the round trip and the store in the cache | 79.4 | 77.5 | 200.8 | 376.0 |
| prefetch of every oid, then the reads at once | 57.2 | 72.7 | – | – |

- The server counted one load per object in each miss round, and 1.002 loads per object at 1 KB and 1.01 at 100 KB in the rounds of the prefetch with the reads at once: a read that comes before the reply of its prefetch sends a second load, because the pool does not join a load to one in flight. That is 2 and 2 loads of 1,000 and 200.
- A read after the prefetch costs 12 to 14 µs per object at 1 KB, twice a hit: the reads wait for the replies of the prefetch, so the rate at which the pool brings them in bounds the read, not the hit.
- The ZEO server runs on the interpreter of its client, so on 3.14t it is slower, and its misses with it. Its misses on 3.14t also vary most (133 to 317 µs at 1 KB), so the round trip there comes out above the miss.

### The counts

**Python calls per load.** A load on the native client makes 13 Python calls, on a hit and on a miss: `Connection.setstate`, `MVCCAdapterInstance.load`, `loadBefore`, `_with_server` and the lambda of `zoe.zeo`, `setGhostState`, `getState`, `_get_unpickler`, `PersistentUnpickler`, `Unpickler.__init__`, the `find_global` closure, `DB.classFactory` and `ZODB.broken.find_global`. Ten are ZODB's, three are `zoe.zeo`'s. `ZEO.ClientStorage` makes 17 on a hit and 41 on a miss. In a Plone request, a load makes 41 Python calls under `Connection.setstate`: the callbacks, the class lookups of Zope's `ClassFactory` and the Python `__setstate__` of the values add them.

**The callbacks.** A `PObject` of zodbshootout has no reference, and its `PersistentMapping` has one per child. One callback costs 0.20 µs on 3.13 and 0.27 µs on 3.14t (two Python frames: `_persistent_load` and `load_persistent`), and `cPickleCache.get` alone 0.05 and 0.07 µs. The decode of the mapping costs 0.69 and 0.83 µs per reference when each child is a ghost in the object cache, and 1.04 and 1.32 µs when each child becomes a new ghost (`klass.__new__` and `new_ghost`).

The `small` fixture has 31,124 current records of 207 classes, with 1.3 references per record on average; 28 % of the records belong to classes without references. Every one of the 40,556 references is an oid with its class: no weak reference, no reference into another database, no bare oid. The records with the most references are `OOBucket` (12.1 per record), `IOBucket` (6.5), `OIBucket` (6.4), `Folder` (5.9) and `ZopeVersionHistory` (4.0).

**`__setstate__` in Python.** 7 of the 207 classes of the fixture, with 5.7 % of its records, have a `__setstate__` in Python: `Persistence.mapping.PersistentMapping` (1,516 records), `BTrees.Length.Length` (165), `ZODB.blob.Blob` (52), `Products.PortalTransforms.Transform.Transform` (26), `ZopePageTemplate`, `PersistentAdapterRegistry` (2 each) and `ZCatalog.Catalog.Catalog` (1). In the Plone requests, these are 9.4 % of the loads. The values in a state count too: a `DateTime` in the metadata of the catalog runs its Python `__setstate__` at each decode.

### A Plone request

One Zope thread, `cacheMinimize()` before each request, the median of the 90 requests of each kind after the first visit of each path, without py-spy:

| Request | time | loads | misses of the client cache |
| --- | --- | --- | --- |
| folder listing | 105.5 ms | 456 | 0 |
| document view | 107.9 ms | 2,222 | 0 |

The first request of the run, a folder listing, took 1,960 ms with 447 misses, and the first document view 258 ms with 1,797 misses. The first visit of each other path missed 12 to 29 records for a folder and 1 for a document: the objects of the site, which every request reads, are in the client cache after the first requests. A document view loads 2,222 objects with an empty object cache, mostly records of the tools: 59 % of the loads of the counted requests are `PersistentList`s, nearly all of them in the map of `portal_transforms`, which the view of the rich text of a Document reads.

**The split, py-spy on the 200 requests**, as a share of the request time of each kind, each sample once. Under py-spy a request takes about 1.4 times as long, so the shares count, not the times.

| Part | folder listing | document view |
| --- | --- | --- |
| outside a load | 75.8 | 60.0 |
| **the loads** | **24.2** | **40.0** |
| `__setstate__` in Python (of which `DateTime`) | 8.6 (8.2) | 10.7 (9.1) |
| state decode (C unpickler) | 6.7 | 11.3 |
| `persistent_load` callbacks (of which the garbage collector) | 2.5 (1.0) | 5.3 (1.5) |
| unpickler setup | 1.8 | 3.9 |
| class lookup (through `App.ClassFactory`) | 1.2 | 1.8 |
| `Connection.setstate`, `ObjectReader`, `MVCCAdapterInstance.load`, `zoe.zeo` frames | 2.2 | 4.9 |
| client cache, the rest of the call into Rust, the wait for the server | 0.6 | 1.0 |
| `unghostify`, `__setstate__` in C | 0.5 | 1.2 |
| **removed by a path in Rust** | **about 7** | **about 15** |

So a load costs about 56 µs in a folder listing and 19 µs in a document view (the share times the request time, over the loads), against 6.6 µs for a `PObject`. A folder listing loads the buckets of the catalog, whose states hold the metadata of many items, each with its dates; each date runs `DateTime.__setstate__`, which computes its fields again. A document view loads many small records of the tools, with 2.6 references per load on average. A path in Rust removes the unpickler setup, the class lookup, the other frames of ZODB and of the client, and the Python frames of the callbacks; the garbage collector runs on the new ghosts in any path.

## What a path in Rust removes (ZOE-274)

At 1 KB, on a hit, from the parts of py-spy on 3.13 and the layer times:

| Term | share of a hit | µs | with a path in Rust |
| --- | --- | --- | --- |
| unpickler setup | 24.3 % | 1.3 to 1.6 | removed: the decode reads the entry |
| `Connection.setstate`, `ObjectReader`, `MVCCAdapterInstance.load`, `zoe.zeo` frames | 25.9 % | 1.7 | removed: one call from `unghostify` into Rust |
| class lookup | 6.0 % | 0.4 to 0.8 | removed: a table of classes, `DB.classFactory` once per class |
| `getpid`, pyo3 | 4.2 % | 0.3 | removed: one check of the process for the whole path |
| state decode | 19.8 % | 1.0 to 1.3 | kept: the same Python objects to build |
| client cache | 8.9 % | 0.55 | kept; the `bytes` of the pickle goes |
| `__setstate__`, `unghostify`, the bookkeeping in C | about 8.6 % | 0.6 | kept: the bookkeeping as C calls |

That is about 60 % removed and 37 % kept, plus 2.4 % for the loop outside the load: from 6.6 to about 2.5 µs on 3.13, and from 6.9 to about 2.6 µs on 3.14t, against 9.2 µs for ZEO, so 3.6× and 3.5×. The estimate holds if the decode in Rust costs no more than the C unpickler. The decode in Rust builds the same objects, so it will not cost much less.

At 100 KB, the Python frames and the glue are 21 % of a hit. A decode from the entry, with one copy of the data into its `bytes` in place of two, removes the 28 % of the first copy. That gives about 15 µs on 3.13 and 16 µs on 3.14t, against 30 and 42 µs for ZEO: 1.9× and 2.6×. The checksum of the entry is then 30 % of what remains; without it, about 11 µs: 2.8× and 3.7×.

What stays with any path: a `__setstate__` in Python (5.7 % of the records of the fixture, and the `DateTime` values of the catalog), the objects of the state, `unghostify`, and the copy of the data of a large record.

**The callbacks do not need a cache of objects in Rust.** A path in Rust resolves a reference with the C calls of `cPickleCache`: a lookup costs 0.05 µs from Python and less from Rust, and a new ghost is a `tp_new` and `new_ghost`. The Python frames of a callback (0.15 to 0.2 µs) go. With 2.6 references per load in a Plone request, a cache of objects in Rust would save at most 2.6 lookups, about 0.13 to 0.18 µs per load, against 19 to 56 µs per load of the requests. The C of `persistent` costs little in a load: `unghostify` 4 %, `__setstate__` 2 %, the bookkeeping about 2 %.

## What prefetch removes (ZOE-273)

ZOE-273 does nothing for a hit. On a cold read of a container, it turns one round trip per child into a fill of the cache that the reads overlap: 56.5 to 11.6 µs per child at 1 KB on 3.13 (61.9 to 14.0 on 3.14t), 137.6 to 57.2 µs at 100 KB (139.7 to 72.7). The cold reads of 1 KB go from 2.6× and 2.7× ZEO to about 12×; at 100 KB, from 1.7× and 2.9× to 4.1× and 5.5×. Two facts bound it:

- The replies of the prefetch bound the reads, not the hit: 11.6 to 14.0 µs per object at 1 KB, against a hit of 6.6 to 6.9 µs. One load per oid stays a round trip of its own on the pool; a batch on the wire could bring the replies faster, but the card allows no new call.
- 0.2 % to 1 % of the reads come before their reply and send a second load, because the pool does not join a load to one in flight.

So with ZOE-274 too, a cold read of 1 KB gains less than a hot one: the reads still wait for the replies.

## Fixes found along the way

Each becomes a card if the human wants it:

1. **The frames of `zoe.zeo.ClientStorage.loadBefore`** (`loadBefore`, `_with_server` and a lambda) cost 0.3 µs, 4.7 % of a hit at 1 KB. `loadBefore` can call `self._native.load_before` and keep `_with_server` for the case of a lost server only.
2. **`getpid` on each `Client.load_before`** costs 0.2 µs, 3.1 % of a hit at 1 KB. The check of the process can read a value that a handler of `pthread_atfork` sets in the child.
3. **The checksum of a hit** is 15 % of a hit at 100 KB (ADR 0034: a doubt is a miss). A hit at 100 KB reaches 3× only without it, so it is a question for the cache, not a fix: see question 2.

## Proposal for the specification

The non-goal "Rewriting `persistent` and `BTrees` in Rust" stays: the C of `persistent` costs 6 to 8 % of a load, and a path in Rust needs calls into `cPickleCache`, not a cache of its own. The proposed wording of the non-goal, for the human to decide:

> - Rewriting `persistent` and `BTrees` in Rust. That is a separate, client-side project ("What stays on persistent and BTrees" says what is left there). The load path between a ghost and the client cache (`Connection.setstate`, `ObjectReader` and the client) may move into Rust, as calls into the C of `persistent` (`docs/spikes/load-path.md`).

and one sentence at the end of "What stays on persistent and BTrees":

> A load from the client cache spends about 60 % of its time in the Python frames of ZODB and of the client, and 6 to 8 % in the C of `persistent` (`docs/spikes/load-path.md`); a path in Rust for the load calls the C of `persistent` and keeps no cache of objects of its own.

## Questions for the human

1. **ZOE-274 at 1 KB.** The estimate is 3.5× to 3.6× ZEO on a hit, with the decode in Rust at the cost of the C unpickler. Build the prototype of ZOE-274 as the card says, or first a smaller step: a reader of ZOE in place of `Connection._reader` (the option (b) of ZOE-274), which removes the unpickler setup, the class lookup and the frames of `ObjectReader` (39 % of a hit) and keeps `Connection.setstate` and the load through the client?
2. **The hot reads at 100 KB.** With the copies and the checksum of the client cache, a hit at 100 KB reaches about 2.6× ZEO on 3.14t with a path in Rust. 3× needs a hit that does not hash the whole entry, which changes the rule of ADR 0034 that a doubt is a miss. Change the rule (for example, verify an entry once per process), or accept that the hot reads at 100 KB stay under 3×?

## Limits

- One workstation with other jobs on it, not the reference machine; each number comes from one run. The layer times are differences of loops, so a term of less than 0.3 µs is within the noise; py-spy gives the same split within about 0.4 µs at 1 KB.
- The layers run alone, with warm data caches of the CPU, so a term in a loop of its own costs less than in the load. At 100 KB the client cache costs 13.7 µs of a load by py-spy and 10.8 µs in its own loop, so the rest of the hit at 100 KB (7 µs) is mostly that.
- The estimate of ZOE-274 assumes a decode in Rust at the cost of the C unpickler, and a call from `unghostify` into Rust at the cost of a C call.
- 3.14t has no sampling profile; cProfile and the layer times stand for it.
- `cacheMinimize()` before each Plone request makes it load the tools of the site, which a real instance keeps in its object cache: a request of a warm instance loads far fewer objects. The Plone instance runs the stock versions of persistent, zodbpickle and BTrees, not the forks.
