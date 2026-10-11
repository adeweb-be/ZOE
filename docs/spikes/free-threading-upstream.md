# Spike: free-threading in persistent, BTrees and zodbpickle

Card ZOE-003, requirement N8.

**Verdict.** The declaration alone keeps the GIL off and passes every upstream suite on CPython 3.14t, but it is not safe to ship as-is. The pickle cache of `persistent` keeps uncounted references, and that protocol relies on the GIL. `zodbpickle` reads shared dicts through borrowed references. All three packages have lazily initialised statics. Each package needs a small set of fixes before the declaration (listed below). Declaring these three packages is also not enough for a free-threaded Zope: eight more packages that every Zope process imports turn the GIL back on.

## Question

Can `persistent`, `BTrees` and `zodbpickle` declare `Py_MOD_GIL_NOT_USED`, so that importing them on CPython 3.14t keeps the GIL off, and what must change first?

## Method

1. Read the upstream sources at the heads of `zopefoundation/persistent`, `BTrees` and `zodbpickle`. Find every piece of process-wide mutable state, every borrowed reference on a shared object, and every read-modify-write that relies on the GIL.
2. Add the declaration to every module: `PyUnstable_Module_SetGIL(module, Py_MOD_GIL_NOT_USED)` under `#ifdef Py_GIL_DISABLED`, right after `PyModule_Create`. All five init functions use single-phase init. One edit in `BTreeModuleTemplate.c` covers the 22 BTrees modules.
3. Build the patched packages for CPython 3.14.7t. Run the acceptance test and the upstream test suites with `PYTHON_GIL=0`, so that a test dependency cannot turn the GIL back on.
4. Run a stress test that moves Connections, their caches and their objects between threads: 32 threads, a ZODB connection pool of 8, a cache of 50 objects, 200 rounds per thread, 2,000 objects.
5. Import the other C extensions that a Zope process loads, one at a time, and check `sys._is_gil_enabled()`.

## Results

| Check | Released packages | Patched packages |
| --- | --- | --- |
| GIL after `import persistent, BTrees, zodbpickle` on 3.14t | on (first: `persistent._timestamp`) | off |
| Acceptance test `test_free_threading_upstream.py` | 2 failed | 2 passed |
| `persistent` suite, GIL off | not run | 618 tests, 0 failures, 1 error (`test_docs` needs the source tree) |
| `zodbpickle` suite, GIL off | not run | 432 tests, 0 failures, 0 errors |
| `BTrees` suite, GIL off | not run | 13,721 tests, 0 failures, 0 errors |
| Pool stress test, GIL off, 5 runs | not run | 0 crashes, 0 errors; increments consistent |

cffi 2.1.1, which `persistent` loads through its `_ring` module even in the C implementation, keeps the GIL off.

The stress test passing does not prove the pickle cache safe: it did not hit the race described below.

## Changes required before the declaration

### persistent

- **The uncounted references of the pickle cache (blocking).** The cache dict holds "stolen" references. The protocol needs "the reference count reaches 0, then the entry leaves the dict" to be atomic against every lookup. On 3.14t that is not guaranteed. Biased reference counting hands a final cross-thread decrement to the thread that allocated the object, and the free-threaded GC clears objects while other threads run. With the Connection pool, a thread routinely holds ghosts that another thread allocated, so `Per_dealloc` can run while another thread looks the same oid up. Fix without touching the attribute path: `PyUnstable_EnableTryIncRef` on insert; lookups and `cc_oid_unreferenced` in a `Py_BEGIN_CRITICAL_SECTION` on the cache dict with `PyUnstable_TryIncRef`; rework the `Py_SET_REFCNT(self, 2)` workaround in `Per_dealloc`. These APIs need CPython 3.14, so the declaration needs a `PY_VERSION_HEX >= 0x030E0000` guard (3.13t keeps the GIL; 3.13t is not a ZOE target).
- `PyDict_GetItem(cls->tp_dict, "__slotnames__")` reads a class dict that every Connection shares: use `PyDict_GetItemRef` (3.13 and later; guard for 3.12).
- Lazy statics: `s_register`, `s_readCurrent`, the `TimeStamp` import in `cPersistence.c`, and `_p_invalidate` in `cPickleCache.c`. Move them to module init.
- `TimeStamp_type` is never passed to `PyType_Ready`. Ready it in module init.
- An existing bug, independent of free-threading: `cPickleCache.c` (`cc_new_ghost`) calls `Py_DECREF` on the borrowed result of `PyDict_GetItem` when the oid is already in the cache.
- `cPickleCache.c` and `cPersistence.c` do not check `PyModule_Create` for NULL.

### BTrees

- The lazy `format` static in `set_repr` (`SetTemplate.c`). Create it in module init.
- `BTreeItemsType` and `BTreeIter_Type` are never passed to `PyType_Ready`.
- `BTreeModuleTemplate.c` does not check `PyModule_Create` for NULL.
- `persistent` must be declared first: every BTrees module imports it.

### zodbpickle

- Lazy statics that set two pointers one after the other (`whichmodule`, and two more pairs), where another thread can see the first set and the second still NULL. Also `codecs_encode`, `latin1`, `name_str`, `str_class`. Move them all to module init.
- Borrowed references on the dicts of `copyreg` (`dispatch_table`, `extension_registry`, `extension_cache`, `inverted_registry`), which `copyreg.pickle` and `add_extension` change at run time: use `PyDict_GetItemRef`.
- `whichmodule` iterates over `sys.modules` and keeps borrowed items across attribute lookups: iterate over a copy. `find_class` takes a borrowed module from `sys.modules`: use `PyImport_GetModule`.

## The contract that stays a contract

These are safe only under ZODB's rule that one thread at a time uses a Connection with its objects, cache and ring. The audit documents the rule and adds no lock to them:

- the per-object state flags of `persistent` and the instance dicts of its objects;
- buckets, sets, nodes and iterators of `BTrees`, persistent or not, which a thread uses one at a time;
- each Pickler and Unpickler of `zodbpickle`, which one thread uses at a time.

The LRU ring and the cache counters of `persistent` were on this list. The audit proposed to leave the ring update on attribute access, the hottest path in Plone, without a lock. The build showed that a sweep from another thread corrupts the ring then, so the fork locks it (see the status below).

## The other packages that turn the GIL back on

Imported one at a time on 3.14t, each of these leaves the GIL on, directly or through what it imports:

| Package | Version | GIL after import |
| --- | --- | --- |
| zope.proxy | 7.3 | on |
| zope.security | 8.4 | on |
| zope.i18nmessageid | 8.3 | on |
| zope.hookable | 8.3 | on |
| ExtensionClass | 6.4 | on |
| Acquisition | 6.3 | on |
| AccessControl | 7.5 | on |
| Persistence | 5.5 | on |
| zope.interface | 8.6 | off |
| zope.container | 7.3 | off |
| DateTime | 6.0 | off |
| cffi | 2.1.1 | off |

The specification says that free-threaded parallelism in Zope needs `persistent`, `BTrees` and `zodbpickle` declared first. The list above shows that a Zope or Plone process needs these eight declared too. The specification's "Context" section and its risk table should say so, and a follow-up spike should audit them the same way.

## Upstream

The patches go upstream to zopefoundation as draft pull requests from the forks in the `adeweb-be` organisation (`adeweb-be/persistent`, `adeweb-be/BTrees`, `adeweb-be/zodbpickle`): one per package, each with the declaration and the blocking fixes above, `persistent` first. They are not opened yet: opening them is a public action that waits for the human's go.

## Status of the patches

Every fix above is written on the `free-threading` branch of each fork, with the declaration:

- `adeweb-be/persistent`: the interned strings and the `TimeStamp` import at module init, `PyType_Ready(TimeStamp_type)`, `PyDict_GetItemRef` in `pickle_slotnames`, no `Py_DECREF` of the borrowed reference in `cc_new_ghost`, and the pickle-cache rework below. The declaration applies to CPython 3.14 and later only; 3.13t keeps the GIL.
- `adeweb-be/BTrees`: `set_repr` without a static, and `PyType_Ready` for the items and iterator types.
- `adeweb-be/zodbpickle`: the lazy statics created at module init, `PyDict_GetItemRef` on the copyreg dicts, `whichmodule` over a copy of `sys.modules`, and `find_class` through `PyImport_GetModule`.

### The pickle-cache rework

- On the free-threaded build, a critical section per cache protects the dict, the ring and the counters in every cache method, in the sweeps, in `ghostify`, `unghostify`, `accessed` (the attribute path) and `Per_dealloc`. On the GIL builds the macros are empty and the generated code of the hot paths does not change.
- The cache dict holds a small entry object per persistent object, not the object itself, because `PyDict_GetItem` on 3.14t takes and drops a reference on the value and so frees a dying object twice. Insertion calls `PyUnstable_EnableTryIncRef`; a lookup calls `PyUnstable_TryIncRef` and treats a failure as a miss. `Per_dealloc` removes the entry only if it points to the object being freed, so a newer object with the same oid stays.
- Three variants were measured. Locking only the cache methods and the sweeps crashes persistent's own `test_threading` (7 runs in 10). Adding `unghostify` passes the upstream tests, but a sweep from another thread while a thread reads attributes of other objects of the same cache corrupts the ring (10 crashes in 10). The human chose the third variant, which also locks `accessed`.

| Operation, ns | 3.14, unpatched | 3.14, patched | 3.14t, unpatched (unsafe) | 3.14t, patched |
| --- | --- | --- | --- | --- |
| attribute access, loaded object | 25.3 | 26.2 | 36.7 | 42.8 |
| `get`, hit | 58.7 | 58.2 | 69.6 | 77.3 |
| `new_ghost` | 170.7 | 173.9 | 203 | 222 |
| ghost created then freed | 249 | 251 | 312 | 349 |
| deactivate and activate | 348 | 351 | 435 | 499 |

The 3.14 differences are noise: the code of the attribute path and of `get` is the same.

With the GIL off on 3.14t, and on 3.12 and 3.14, the suites pass: persistent 783 tests, BTrees 13,721, zodbpickle 432, and ZODB 1,600 with one error that is not about persistent (`testdocumentation` needs a ZODB source checkout). `test_threading.py` passes 50 runs in a row with the GIL off and 50 with `PYTHON_GIL` unset; upstream master crashes in it on 3.14t. The acceptance test and the pool stress test pass with the GIL off.

### Limits that stay

- A sweep from another thread of a Connection in use (for example `DB.cacheMinimize()` while requests run) keeps the ring consistent but is still not supported: the thread that uses the Connection can get spurious `AttributeError`s or a `SystemError` from `unghostify`, and it can read an instance dict that the sweep frees. CPython delays the free of a managed dict for lock-free readers through private API only.
- While the `__del__` of a cached object runs, lookups in other threads miss it. If `__del__` keeps the object alive, another object may have taken its oid meanwhile.
- Two suspicions from the review are not reproduced: two threads that deactivate the same object can each build a dict over the same inline values, and the free-threaded GC can let a lookup succeed on a dead ghost in a short window before it clears it.
- `persistent` cannot be installed on 3.13t, because cffi refuses that build.

The human decided to keep the branches on the forks and to open no pull request upstream for now.
