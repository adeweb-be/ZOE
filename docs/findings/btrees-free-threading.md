# Finding: BTrees turns the GIL on in a ZODB application on CPython 3.14t

Card ZOE-046. Found by ZOE-045.

**Verdict.** BTrees 6.5 does not declare free-threading support, so the first import of a family module, such as `BTrees._OOBTree`, turns the GIL on. Its C extensions share no state that changes after module init, except two lazy initializations, which the fork's branch `free-threading` (`06324b5`) already removes. With one tree for each thread, stock 6.5 forced to run without the GIL passes the stress test without its re-entrant step. The real hazard is not specific to threads: a search holds borrowed pointers into a node while a comparison of object keys runs Python code, and that code can free the node's arrays with `_p_invalidate()`. Stock 6.5 then crashes, with or without the GIL. Our fork `adeweb-be/BTrees`, branch `zoe-046-free-threading`, commit `5f863e0`, version `6.5+zoe.1`, keeps the declaration and makes such a search raise `RuntimeError`. The pickled states and the conflict-resolution results do not change. With the fork, `import ZODB, BTrees.OOBTree, BTrees.IOBTree, BTrees.LOBTree, BTrees.OIBTree` keeps the GIL off on 3.14t. In a Plone process, `ExtensionClass` is the next module that turns it on.

## Shared state of the C extensions

Each of the 22 modules (`_OOBTree` to `_fsBTree`) compiles `BTreeModuleTemplate.c` with its own key and value macros, and uses single-phase init. References are to stock 6.5.

| State | Built | After init | Fork |
| --- | --- | --- | --- |
| Interned strings `sort_str`, `reverse_str`, `__setstate___str`, `_bucket_type_str`, `max_internal_size_str`, `max_leaf_size_str`, `__slotnames__str` (`BTreeModuleTemplate.c:54-59`) | `module_init()` | read-only, immortal | unchanged |
| `ConflictError` (`BTreeModuleTemplate.c:60`), from `BTrees.Interfaces.BTreesConflictError` | `module_init()`, strong reference | read-only | unchanged |
| `cPersistenceCAPI` (`include/persistent/persistent/cPersistence.h:112`) | `module_init()`, `PyCapsule_Import()` | read-only; its functions act on one object or on the cache of one connection | unchanged |
| `object_` (`objectkeymacros.h:17`), the `object` type | `module_init()` | read-only, immortal | unchanged |
| `BTreeType_setattro_allowed_names` (`BTreeTemplate.c:2504`) | `module_init()` | read-only tuple | unchanged |
| The static types `Bucket`, `Set`, `BTree`, `TreeSet` and their metatype | `module_init()` | read-only, except below | unchanged |
| `BTreeItemsType`, `BTreeIter_Type` (`BTreeItemsTemplate.c:485`, `743`) | never readied: `PyType_Ready()` ran on the first attribute lookup, in any thread | lazy | readied at init (`06324b5`) |
| `format` in `set_repr()` (`SetTemplate.c:372`) | first `repr()` | lazy | `PyUnicode_FromFormat()` (`06324b5`) |
| `tp_dict` of the tree types | `BTreeType_setattro()` (`BTreeTemplate.c:2507-2532`) writes `max_leaf_size`, `max_internal_size` and three zope.interface names. `_module_builder.py:226-229` sets the sizes at import | `PyDict_SetItem()`, then `PyType_Modified()`. On 3.14t the dict has its own lock and the type cache takes the type lock, so a lookup in another thread gets the old or the new value | unchanged |
| `_p_resolveConflict` (`BucketTemplate.c:1633`, `1682`; `MergeTemplate.c:41`, `89`) | no state: it builds three new buckets from the states and raises `ConflictError` | — | unchanged |

The upstream branch `tseaver-207-move_static_globals_to_module_state` moves these statics into module state, for multi-phase init (19 commits, about 1,300 lines). Multi-phase init serves subinterpreters. Statics that do not change after init are safe without the GIL, so the fork does not take that branch.

## Borrowed references across Python code

| Site (stock 6.5) | Borrowed pointer | Python code that runs | Fork |
| --- | --- | --- | --- |
| `COMPARE()`, `objectkeymacros.h:12-15`, `objectvaluemacros.h:8-11` | the slot of a node array, read again for the second comparison | `__lt__` and `__eq__` of an object key or value | `object_compare()` holds a reference to both operands (`d601889`) |
| `BUCKET_SEARCH`, `BucketTemplate.c:38-52`; `BTREE_SEARCH`, `BTreeModuleTemplate.c:319-331` | `keys` or `data` of the node, and the length read before the loop | the key comparison | raises `RuntimeError` if the array or the length changes (`4f7cb25`) |
| `BTree_rangeSearch()`, `BTreeTemplate.c:1823-1831` | the first and the last key, after `PER_UNUSE` of their buckets | the comparison of the two keys | covered by `object_compare()` |
| `_BTree_get()` `:282`, `BTree_findRangeEnd()` `:1494`, `_BTree_set()` `:749-830` | the child pointer, after `PER_UNUSE` of the parent; in `_BTree_set()`, the slot `d` while the child runs | `PER_USE(child)` calls `jar.setstate()`; the child runs comparisons | unchanged: see Limits |
| `__getstate__`, `__setstate__` | items of the state tuple | none between the read and the reference count | safe: a tuple does not change |

`PER_USE` makes an object sticky. Sticky stops `_p_deactivate()`: the bucket's override (`BucketTemplate.c:1137`) frees the arrays only in the up-to-date state. It does not stop `_p_invalidate()`: `Per_set_changed(self, NULL)` in the persistent fork (`cPersistence.c:1161-1170` at `57a8c54`) sets the state to up-to-date and calls `_p_deactivate()`, which calls `_bucket_clear()` (`BucketTemplate.c:1092`) and frees `keys` and `values`. The ZOE-043 fix changes how `ghostify()` drops the instance dict. The arrays of a bucket are not in that dict, so the fix does not change this.

Can an invalidation from another connection do it? ZODB applies invalidations in `Connection.newTransaction()`, through `PickleCache.invalidate()`, at a transaction boundary and in the thread that uses the connection. No C loop of that thread runs then, unless Python code inside the loop, such as a key comparison or `jar.setstate()`, reaches a transaction boundary or invalidates objects itself. So not from another thread under ZODB's rule, but yes from re-entrant code, with or without the GIL.

## Key comparisons in the bucket loops

The loops assume that the node does not change while a comparison runs. For object keys, the comparison runs Python code. With the GIL, that code can already switch to another thread, so the C code never relied on the GIL here. It relies on two rules: one thread uses a tree at a time, and the comparison code does not touch the node. Without the GIL, nothing changes for a tree that one thread uses.

For integer, float and `fs` keys, the comparison is C code. With the GIL, a whole C call was atomic for other threads, so a tree shared by two threads stayed memory-safe. Without the GIL, it is a data race. ZODB gives each connection its own copy of a tree, so this concerns only a tree outside the database that an application shares between threads.

## The fork

The branch `zoe-046-free-threading` starts from `06324b5` and adds four commits:

| Commit | Change |
| --- | --- |
| `d601889` | `COMPARE()` calls `object_compare()`, which holds a reference to both operands during both comparisons. The results do not change |
| `4f7cb25` | For object keys, `BUCKET_SEARCH` and `BTREE_SEARCH` record the array and the length of the node, and raise `RuntimeError` if a comparison changes either. Other key types compile without the check |
| `3fc3324` | `src/BTrees/tests/test_free_threading.py`, the stress test |
| `5f863e0` | Version `6.5+zoe.1` and the change log |

The fork builds against its own copy of `cPersistence.h`, which equals the header of the persistent fork except for one comment.

## Stress test

Sixteen threads each take one family (`OO`, `OI`, `OL`, `IO`, `II`, `IF`, `LO`, `LL`, `QQ`, `UO`) and change their own BTree and TreeSet: inserts, deletes, lookups and range scans, checked against a dict. The threads start at a barrier and first use each type of their module at the same time. A small data manager with a `PickleCache` gives the nodes oids, stores their states, aborts, and ghostifies them with `minimize()`, `_p_invalidate()` and `_p_deactivate()`. Each round resolves conflicts on `Bucket`, `BTree`, `Set` and `TreeSet` states and compares the result with the expected state, and checks that a conflicting change raises `BTreesConflictError`. For object keys, after a commit, a key comparison invalidates the buckets of the tree while a lookup or a range scan runs. The search must give the correct result or raise `RuntimeError`.

Each run is a new process under `faulthandler`, with `sys.setswitchinterval(1e-6)`.

| Build | Interpreter | GIL | Re-entrant step | Rounds | Runs | Passed | Crashed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fork `5f863e0` | 3.14t | off | yes | 100 | 20 | 20 | 0 |
| fork `5f863e0` | 3.14t | `PYTHON_GIL=0` | yes | 100 | 20 | 20 | 0 |
| fork `5f863e0` | 3.14t | `PYTHON_GIL=1` | yes | 100 | 5 | 5 | 0 |
| fork `5f863e0` | 3.12, 3.13, 3.14 | on | yes | 100 | 5 each | 5 each | 0 |
| branch `free-threading` (`06324b5`) | 3.14t | off | yes | 12 | 10 | 0 | 10 |
| stock 6.5 | 3.14t | `PYTHON_GIL=0` | yes | 12 | 10 | 0 | 10 |
| stock 6.5 | 3.14t | on, from the import | yes | 12 | 5 | 0 | 5 |
| stock 6.5 | 3.13, 3.14 | on | yes | 12 | 5 each | 0 | 5 each |
| branch `free-threading` (`06324b5`) | 3.14t | off | no | 100 | 10 | 10 | 0 |
| stock 6.5 | 3.14t | `PYTHON_GIL=0` | no | 100 | 10 | 10 | 0 |
| stock 6.5 | 3.13 | on | no | 100 | 5 | 5 | 0 |

With the fork, each run of 100 rounds gets a few hundred `RuntimeError` from the guard and no wrong result. Without the guard, the search reads the freed or NULL array of the invalidated bucket and the process crashes, with or without the GIL. Without the re-entrant step, every build passes: one tree for each thread needs no change beyond the declaration.

## Pickled states and conflict resolution

A check script builds, for each of the 22 families, four key kinds (integers, strings, tuples and floats for object keys; small and full-range integers for integer keys; two-byte strings for `fs`), eight sizes from 0 to 5,000 keys, and the four types. It pickles the state of each object and the object itself, at protocol 3, before and after deletes and inserts: 7,920 pickles. It also runs 1,200 three-way `_p_resolveConflict()` calls for each family, with deletes, changes, inserts, identical changes in both versions and values of mixed types: 26,400 results, of which 6,002 are `BTreesConflictError`, 212 `SystemError` and 32 `TypeError`. The fork and stock 6.5 give the same digest on 3.12, 3.13, 3.14 and 3.14t, and on 3.14t without the GIL.

## Suites

The suite of the fork, with ZODB installed, passes on 3.12, 3.13, 3.14, 3.14t, and 3.14t with `PYTHON_GIL=0`: 13,722 tests, 0 failures, 254 skipped.

`just conformance` passes on 3.13, on 3.14t, and on 3.14t with `PYTHON_GIL=0`: 41 passed, 3 skipped. On 3.14t the GIL now stays off for the whole run. The one warning is a `ValueError` that ZODB's `test_racetest` raises in a thread on purpose.

## The GIL in the ZODB and Plone stack

On 3.14t, in the workspace of `python/` with the extra `plone` of zoedb, a new process imports one module and prints `sys._is_gil_enabled()`. The warning of CPython names the module that turns the GIL on.

| Import | GIL after import | Turned on by |
| --- | --- | --- |
| `ZODB`, `ZODB.FileStorage`, `ZODB.MappingStorage`, `BTrees.OOBTree`, `transaction`, `zodburi`, `zoedb` | off | — |
| `OFS.Application`, `Products.CMFPlone` | on | `ExtensionClass._ExtensionClass` 6.4 |
| `zope.component` | on | `zope.hookable._zope_hookable` 8.3 |
| `Acquisition`, `AccessControl`, `Persistence`, `ComputedAttribute`, `MethodObject` C modules | on | `ExtensionClass._ExtensionClass` |
| `zope.security` C modules | on | `zope.proxy._zope_proxy_proxy` 7.3 |
| `zope.container._zope_container_contained` 7.3, `zope.i18nmessageid` 8.3, `lxml.etree` 6.1.3 | on | the module itself |

`ExtensionClass` is the next module that turns the GIL on in a Plone process. This card does not fork it.

## Limits

- A BTree, a bucket, a set and an iterator are for one thread at a time. ZODB uses them so.
- A tree outside the database that two threads share is a data race without the GIL, also for integer keys.
- The guard covers the arrays of the node that a search reads. Python code that invalidates the parent of that node during the search, or a whole tree in `_BTree_set()`, still uses borrowed child pointers. A `PickleCache` keeps a child alive only while the child is not a ghost.
- A sweep of the cache of a connection that another thread uses, such as `DB.cacheMinimize()`, is still not supported: `_p_deactivate()` of a bucket checks the state and then frees the arrays, and the owner thread can make the bucket sticky in between.
- In `MergeTemplate.c`, an error in the comparison of two values is not checked. For values of mixed types, `_p_resolveConflict()` returns a result with an exception set, and Python raises `SystemError`. Stock 6.5 does the same, and the fork keeps it: 212 of the corpus results.
