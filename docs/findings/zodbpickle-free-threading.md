# Finding: zodbpickle turns the GIL on at `import ZODB` on CPython 3.14t

Card ZOE-045. Found by ZOE-043.

**Verdict.** `zodbpickle._pickle` 4.5 does not declare free-threading support, so `import ZODB` turns the GIL on. Its C code also shares state in ways that are not safe without the GIL. One of them is not safe with the GIL either: stock 4.5 crashes on 3.13 and 3.14 when another thread changes a dict while it is pickled. Our fork `adeweb-be/zodbpickle`, branch `zoe-045-free-threading`, commit `84029ff`, version `4.5+zoe.1`, makes the shared state safe and declares support. The pickle bytes do not change. With the fork, `import ZODB` keeps the GIL off on 3.14t. BTrees still turns it on, when a module imports it.

## Shared state of `_pickle.c`

The branch `free-threading` of the fork holds commit `0c71a10`. It declares support, creates the lazy statics at init, reads the `copyreg` dicts with strong references, iterates over a copy of `sys.modules` and gets the module in `find_class()` with a strong reference. The audit found it correct, and the card builds three commits on it.

| State | Who writes it | 4.5 | Fork |
| --- | --- | --- | --- |
| Exception types, `empty_tuple` | nothing after init | read-only | unchanged |
| `copyreg.dispatch_table` | `copyreg.pickle()`, user code | borrowed read: a replace in another thread can free the reducer | `PyDict_GetItemRef()` on 3.13 and later (`0c71a10`) |
| `copyreg._extension_registry`, `_inverted_registry` | `add_extension()`, `remove_extension()` | borrowed read | strong reference (`0c71a10`) |
| `copyreg._extension_cache` | `load_extension()`, `clear_extension_cache()` | borrowed read | strong reference (`0c71a10`) |
| The four mapping dicts of `_compat_pickle` | nothing: the standard library, ZODB and zodbpickle only read them | read-only after import | unchanged, as in CPython 3.14 |
| 8 function statics: interned strings and `codecs.encode` | first use | two statics set one after the other | created in `initmodule()` (`0c71a10`) |
| 14 `_Py_IDENTIFIER` strings | first use, through `_PyUnicode_FromId()` | the lock of each identifier does not cover the identifier array of the interpreter, which grows on first use | interned strings in one table, created in `initmodule()` (`dd1b992`) |
| `sys.modules` in `whichmodule()` | imports in any thread | borrowed entries while `getattr()` runs code | iterates over a copy (`0c71a10`) |

A dict or a list that is pickled is not module state, but the caller can share it with other threads:

| Container | 4.5 | Fork |
| --- | --- | --- |
| Exact dict, `batch_dict_exact()` | borrowed key and value while `save()` runs code, and a `PyDict_Next()` position that a change makes invalid | critical section on the dict, and strong references to the key and the value (`e32b204`) |
| Exact list, `batch_list_exact()` | borrowed item while `save()` runs code | `PyList_GetItemRef()` (`e32b204`) |

## Per-object use in ZODB

ZODB never uses one `Pickler` or one `Unpickler` from two threads at once. `Connection._commit()` creates one `ObjectWriter`, and with it one pickler, for each object that it stores. `ObjectReader._get_unpickler()` creates a new unpickler for each load, and `referencesf()`, `get_refs()` and `ExportImport` create theirs for each call. A connection belongs to one thread at a time. The fork thus protects only the shared state and adds no lock to the objects. A `Pickler` or an `Unpickler` is still for one thread at a time.

## What applies from CPython 3.14

| Change in CPython 3.14 `Modules/_pickle.c` | Applies to the fork |
| --- | --- |
| `Py_mod_gil` set to `Py_MOD_GIL_NOT_USED` | yes. The module uses single-phase init, so `0c71a10` calls `PyUnstable_Module_SetGIL()` |
| Module state and `_Py_ID()` instead of lazy statics and `_Py_IDENTIFIER` | the removal of lazy statics applies (`0c71a10`, `dd1b992`). Statics that do not change after init replace the module state, which needs multi-phase init |
| Critical section around the `PyDict_Next()` loop of `batch_dict_exact()`, with strong references | yes (`e32b204`) |
| `PyList_GetItemRef()` in `batch_list_exact()` | yes (`e32b204`). CPython also raises when the list changes size. The fork keeps the 4.5 behaviour |
| Critical section around the `sys.modules` loop of `whichmodule()` | the copy of `0c71a10` has the same effect |
| Critical section in `save_set()` | no: protocol 3 has no set opcodes, and sets go through `__reduce__` |
| `PyDict_GetItemRef()` for the extension registry | yes. The fork also uses it for the dispatch table and the extension cache, where CPython 3.14 still reads a borrowed reference |

## Stress test

`src/zodbpickle/tests/test_free_threading.py` (`a92301b`) starts eight threads. Each thread pickles and unpickles a payload with its own pickler and unpickler, for protocols 0 to 3. The payload uses the extension registry, `copyreg.dispatch_table`, persistent ids, the name mapping tables, and a function that the pickler must find in `sys.modules`. The threads also pickle one shared dict and one shared list, and call `find_class()` on one shared unpickler. One more thread changes the `copyreg` tables, the extension cache, `sys.modules` and the shared containers all the time. A thread fails when its bytes differ from the bytes of a single thread.

Each run is a new process under `faulthandler`.

| Build | Interpreter | GIL | Runs | Passed | Failed | Crashed |
| --- | --- | --- | --- | --- | --- | --- |
| fork `84029ff` | 3.14t | off | 20 | 20 | 0 | 0 |
| fork `84029ff` | 3.14t | `PYTHON_GIL=0` | 20 | 20 | 0 | 0 |
| fork `84029ff` | 3.14t | `PYTHON_GIL=1` | 5 | 5 | 0 | 0 |
| fork `84029ff` | 3.13, 3.14 | on | 5 each | 5 each | 0 | 0 |
| branch `free-threading` (`0c71a10`) | 3.14t | off | 10 | 1 | 0 | 9 |
| stock 4.5 | 3.14t | on, from the import | 5 | 3 | 1 | 1 |
| stock 4.5 | 3.14t | `PYTHON_GIL=0` | 10 | 0 | 0 | 10 |
| stock 4.5 | 3.13, 3.14 | on | 5 each | 0 | 0 | 5 each |

The crashes of `0c71a10` and of stock 4.5 on 3.14 are in `batch_dict_exact()`: a value that the changer removes from the shared dict is freed while the pickler saves it. With the GIL, `save()` calls `persistent_id()`, which is Python code, and the GIL can go to the changer at that point.

## Pickle bytes

A check script builds a corpus of 110 objects: integers at each opcode limit, floats, strings, bytes up to 70,000 bytes, the singletons and their types, tuples, lists and dicts larger than the batch size, sets, shared and recursive references, classes, `__slots__`, `__getstate__`, `__reduce__`, `__reduce_ex__`, `__getnewargs__`, a `copyreg` dispatch entry, extension codes of 1, 2 and 4 bytes, functions, builtins, names that `_compat_pickle` maps, `datetime`, `Decimal` and persistent ids. It pickles each object with protocols 0 to 3, with and without `fix_imports`: 880 pickles, with `PYTHONHASHSEED=0`. On 3.12, 3.13, 3.14 and 3.14t, stock 4.5 and the fork give the same bytes, with the same digest on all four interpreters. The same 4 pickles raise the same `TypeError` in both: an object with `__slots__` at protocols 0 and 1.

## Suites

The suite of the fork passes on 3.12, 3.13, 3.14, 3.14t, and 3.14t with `PYTHON_GIL=0`: 433 tests, 0 failures, 25 skipped.

`just conformance` passes on 3.13, on 3.14t, and on 3.14t with `PYTHON_GIL=0`: 41 passed, 3 skipped. On 3.14t without `PYTHON_GIL=0`, the GIL is on during the run, because the suites import BTrees.

## The GIL in the ZODB stack

On 3.14t, a new process imports one module and prints `sys._is_gil_enabled()`:

| Module | GIL after import |
| --- | --- |
| `zodbpickle._pickle`, `persistent`, `ZODB`, `ZODB.DB` | off |
| `transaction`, `zc.lockfile`, `zope.interface` and its C optimizations | off |
| `zodburi`, `zoedb` | off |
| `BTrees` 6.5, any module (the first C module is `BTrees._OOBTree`) | on |
| `ZODB.MappingStorage`, `ZODB.FileStorage`, which import BTrees | on |

BTrees is the module that still turns the GIL on. This card does not fork it.

## Limits

- A `Pickler` or an `Unpickler` is for one thread at a time. ZODB uses them so.
- The `memo` setters of `Pickler` and `Unpickler` iterate over a dict of the caller without a critical section, as in CPython 3.14.
- In `save_reduce()`, `get_class()` can return NULL, and 4.5 then releases NULL. This is not about threads, and the fork does not change it.
