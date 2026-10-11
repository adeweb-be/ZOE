# Finding: BTrees turns the GIL on in a ZODB application on CPython 3.14t

Card ZOE-046. Found by ZOE-045.

**Verdict.** BTrees 6.5 does not declare free-threading support, so the first import of a family module, such as `BTrees._OOBTree`, turns the GIL on. Its C extensions share no state that changes after module init, except two lazy initializations, which the fork's branch `free-threading` (`06324b5`) already removes. With one tree for each thread, stock 6.5 forced to run without the GIL passes the stress test without its re-entrant step. The real hazard is not specific to threads: a search holds borrowed pointers into a node while a comparison of object keys runs Python code, and that code can free the node's arrays with `_p_invalidate()`. Stock 6.5 then crashes, with or without the GIL. A delete has the same hazard: it compares the key with the separator of its node and then writes the new separator into the node's array. Our fork `adeweb-be/BTrees`, branch `zoe-046-free-threading`, commit `fb36949`, version `6.5+zoe.2`, keeps the declaration and makes such a search or delete raise `RuntimeError`. The pickled states and the conflict-resolution results do not change. With the fork, `import ZODB, BTrees.OOBTree, BTrees.IOBTree, BTrees.LOBTree, BTrees.OIBTree` keeps the GIL off on 3.14t. In a Plone process, `ExtensionClass` is the next module that turns it on.

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
| `BTree_rangeSearch()`, `BTreeTemplate.c:1823-1831` | the first and the last key, after `PER_UNUSE` of their buckets. `first` stays borrowed across `PER_USE(highbucket)`, which can call `jar.setstate()` before the comparison takes a reference | `jar.setstate()`, then the comparison of the two keys | the comparison is covered by `object_compare()`; the load before it is not: see Limits |
| `_BTree_set()`, `BTreeTemplate.c:749-853`, on a delete | the slot `d = self->data + min` of its own node, while the child runs and while the deleted key is compared with the separator `d->key` (`:830`); then `DECREF_KEY(d->key)` and a store into `d->key` | the comparisons in the child, and `__lt__` and `__eq__` of the deleted key | keeps a reference to the child, records the array and the length of the node, raises `RuntimeError` if either changes after the child or after the separator comparison, and releases the old separator after the store (`beee033`) |
| `_BTree_get()` `:282`, `BTree_findRangeEnd()` `:1494` | the child pointer, after `PER_UNUSE` of the parent | `PER_USE(child)` calls `jar.setstate()`; the child runs comparisons | unchanged: see Limits |
| `__getstate__`, `__setstate__` | items of the state tuple | none between the read and the reference count | safe: a tuple does not change |

`PER_USE` makes an object sticky. Sticky stops `_p_deactivate()`: the bucket's override (`BucketTemplate.c:1137`) frees the arrays only in the up-to-date state. It does not stop `_p_invalidate()`: `Per_set_changed(self, NULL)` in the persistent fork (`cPersistence.c:1161-1170` at `57a8c54`) sets the state to up-to-date and calls `_p_deactivate()`, which calls `_bucket_clear()` (`BucketTemplate.c:1092`) and frees `keys` and `values`. The ZOE-043 fix changes how `ghostify()` drops the instance dict. The arrays of a bucket are not in that dict, so the fix does not change this.

Can an invalidation from another connection do it? ZODB applies invalidations in `Connection.newTransaction()`, through `PickleCache.invalidate()`, at a transaction boundary and in the thread that uses the connection. No C loop of that thread runs then, unless Python code inside the loop, such as a key comparison or `jar.setstate()`, reaches a transaction boundary or invalidates objects itself. So not from another thread under ZODB's rule, but yes from re-entrant code, with or without the GIL.

## Key comparisons in the bucket loops

The loops assume that the node does not change while a comparison runs. For object keys, the comparison runs Python code. With the GIL, that code can already switch to another thread, so the C code never relied on the GIL here. It relies on two rules: one thread uses a tree at a time, and the comparison code does not touch the node. Without the GIL, nothing changes for a tree that one thread uses.

For integer, float and `fs` keys, the comparison is C code. With the GIL, a whole C call was atomic for other threads, so a tree shared by two threads stayed memory-safe. Without the GIL, it is a data race. ZODB gives each connection its own copy of a tree, so this concerns only a tree outside the database that an application shares between threads.

## The fork

The branch `zoe-046-free-threading` starts from `06324b5` and adds seven commits:

| Commit | Change |
| --- | --- |
| `d601889` | `COMPARE()` calls `object_compare()`, which holds a reference to both operands during both comparisons. The results do not change |
| `4f7cb25` | For object keys, `BUCKET_SEARCH` and `BTREE_SEARCH` record the array and the length of the node, and raise `RuntimeError` if a comparison changes either. Other key types compile without the check |
| `3fc3324` | `src/BTrees/tests/test_free_threading.py`, the stress test |
| `5f863e0` | Version `6.5+zoe.1` and the change log |
| `beee033` | `_BTree_set()` keeps a reference to the child, and raises `RuntimeError` if its node changes after the child returns or after the comparison with the separator. It releases the old separator after the store of the new one |
| `ec3c5e3` | The delete of a separator key in the stress test, and a test that runs it with freed memory poisoned |
| `fb36949` | Version `6.5+zoe.2` and the change log |

The fork builds against its own copy of `cPersistence.h`, which equals the header of the persistent fork except for one comment.

## Stress test

Sixteen threads each take one family (`OO`, `OI`, `OL`, `IO`, `II`, `IF`, `LO`, `LL`, `QQ`, `UO`) and change their own BTree and TreeSet: inserts, deletes, lookups and range scans, checked against a dict. The threads start at a barrier and first use each type of their module at the same time. A small data manager with a `PickleCache` gives the nodes oids, stores their states, aborts, and ghostifies them with `minimize()`, `_p_invalidate()` and `_p_deactivate()`. Each round resolves conflicts on `Bucket`, `BTree`, `Set` and `TreeSet` states and compares the result with the expected state, and checks that a conflicting change raises `BTreesConflictError`. For object keys, every round, after the commit or the abort, two re-entrant steps run. In the first, a key comparison invalidates the buckets of the tree while a lookup or a range scan runs. In the second, the thread deletes a separator key of the root, and the comparison of the key with that separator invalidates the root. The operation must give the correct result or raise `RuntimeError`.

Each run is a new process under `faulthandler`, with `sys.setswitchinterval(1e-6)`. The rows of `fb36949` and its parent `5f863e0` with the delete step run with freed memory poisoned (`MALLOC_PERTURB_=165`, `GLIBC_TUNABLES=glibc.malloc.tcache_count=0`). Without the poison, a write into the freed array of the root gives no error.

| Build | Interpreter | GIL | Re-entrant step | Rounds | Runs | Passed | Crashed |
| --- | --- | --- | --- | --- | --- | --- | --- |
| fork `fb36949` | 3.14t | off | search and delete | 100 | 10 | 10 | 0 |
| fork `fb36949` | 3.14t | `PYTHON_GIL=0` | search and delete | 100 | 10 | 10 | 0 |
| fork `fb36949` | 3.13 | on | search and delete | 100 | 3 | 3 | 0 |
| fork `5f863e0` | 3.14t | off | search and delete | 12 | 10 | 0 | 0; all 10 failed: a thread read a key object after it was freed |
| fork `5f863e0` | 3.13 | on | search and delete | 12 | 5 | 0 | 5 |
| fork `5f863e0` | 3.14t | off | search | 100 | 20 | 20 | 0 |
| fork `5f863e0` | 3.14t | `PYTHON_GIL=0` | search | 100 | 20 | 20 | 0 |
| fork `5f863e0` | 3.14t | `PYTHON_GIL=1` | search | 100 | 5 | 5 | 0 |
| fork `5f863e0` | 3.12, 3.13, 3.14 | on | search | 100 | 5 each | 5 each | 0 |
| branch `free-threading` (`06324b5`) | 3.14t | off | search | 12 | 10 | 0 | 10 |
| stock 6.5 | 3.14t | `PYTHON_GIL=0` | search | 12 | 10 | 0 | 10 |
| stock 6.5 | 3.14t | on, from the import | search | 12 | 5 | 0 | 5 |
| stock 6.5 | 3.13, 3.14 | on | search | 12 | 5 each | 0 | 5 each |
| branch `free-threading` (`06324b5`) | 3.14t | off | no | 100 | 10 | 10 | 0 |
| stock 6.5 | 3.14t | `PYTHON_GIL=0` | no | 100 | 10 | 10 | 0 |
| stock 6.5 | 3.13 | on | no | 100 | 5 | 5 | 0 |

With the fork, each run of 100 rounds gets a few hundred `RuntimeError` from the guards and no wrong result. Without the search guard, the search reads the freed or NULL array of the invalidated bucket and the process crashes, with or without the GIL. Without the delete guard, the delete writes into the freed array of the root: on 3.13 the process crashes, and on 3.14t a thread reads a key object after it was freed.

`test_delete_separator_with_poisoned_memory` runs one such delete in a new process with freed memory poisoned. It ends with a segmentation fault on `5f863e0`, on 3.13 and on 3.14t, and passes with `fb36949`. Without the poison, the same delete on `5f863e0` exits with no error. Without the re-entrant step, every build passes: one tree for each thread needs no change beyond the declaration.

## Pickled states and conflict resolution

A check script builds, for each of the 22 families, four key kinds (integers, strings, tuples and floats for object keys; small and full-range integers for integer keys; two-byte strings for `fs`), eight sizes from 0 to 5,000 keys, and the four types. It pickles the state of each object and the object itself, at protocol 3, before and after deletes and inserts: 7,920 pickles. It also runs 1,200 three-way `_p_resolveConflict()` calls for each family, with deletes, changes, inserts, identical changes in both versions and values of mixed types: 26,400 results, of which 6,002 are `BTreesConflictError`, 212 `SystemError` and 32 `TypeError`. The fork at `5f863e0` and stock 6.5 give the same digest on 3.12, 3.13, 3.14 and 3.14t, and on 3.14t without the GIL. The fork at `fb36949` gives the same digest on 3.13, 3.14t and 3.14t without the GIL.

## Suites

The suite of the fork, with ZODB installed, passes at `5f863e0` on 3.12, 3.13, 3.14, 3.14t, and 3.14t with `PYTHON_GIL=0`: 13,722 tests, 0 failures, 254 skipped. At `fb36949` it passes on 3.13, 3.14t, and 3.14t with `PYTHON_GIL=0`: 13,723 tests, 0 failures, 254 skipped.

`just conformance` passes at `5f863e0` on 3.13, on 3.14t, and on 3.14t with `PYTHON_GIL=0`, and at `fb36949` on 3.13 and 3.14t: 41 passed, 3 skipped. On 3.14t the GIL now stays off for the whole run. The one warning is a `ValueError` that ZODB's `test_racetest` raises in a thread on purpose.

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
- The guards cover the node that a search reads, and the node of `_BTree_set()` across the child and the separator comparison. Python code that invalidates the parent of a node during a search still meets borrowed child pointers in `_BTree_get()` and `BTree_findRangeEnd()`. A `PickleCache` keeps a child alive only while the child is not a ghost.
- A load (`jar.setstate()` from `PER_USE`) or the release of a key or a value can also run Python code. The fork does not guard these: in `BTree_rangeSearch()`, `first` stays borrowed across `PER_USE(highbucket)`; `BTree_grow()` keeps a slot across the creation of the new bucket and a load; `_bucket_set()` and the removal of an empty child release objects before they move the arrays.
- A sweep of the cache of a connection that another thread uses, such as `DB.cacheMinimize()`, is still not supported: `_p_deactivate()` of a bucket checks the state and then frees the arrays, and the owner thread can make the bucket sticky in between.
- In `MergeTemplate.c`, an error in the comparison of two values is not checked. For values of mixed types, `_p_resolveConflict()` returns a result with an exception set, and Python raises `SystemError`. Stock 6.5 does the same, and the fork keeps it: 212 of the corpus results.
