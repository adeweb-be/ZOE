# Finding: persistent loses the state of an object on CPython 3.14t

Card ZOE-043. Found by ZOE-027.

**Verdict.** The bug is in the C code of `persistent` and not in ZOE. On CPython 3.13 and later, the instance dict of a `Persistent` subclass can share the inline values of the object. `ghostify()` and `_p_deactivate()` release that dict but leave the values in it. On the free-threaded build, the garbage collector can then clear the released dict, and this clears the state of the object that a later load put in the same values. Borrowed references and a race between two threads on one object are not the cause. The fix moves the values into the dict before the release, with `PyObject_ClearManagedDict()`. It is in our fork `adeweb-be/persistent`, branch `zoe-043-getstate-race`, commit `57a8c54`, version `6.8+zoe.1`.

## Symptom

ZODB's `RaceTests.test_race_loadopen_vs_local_invalidate` fails on 3.14t, with MappingStorage, FileStorage and zoe:

- `obj.__getstate__()` in `ZODB.serialize` raises `SystemError: null argument to internal routine`;
- `root['obj1'].value` raises `AttributeError` after a load;
- the report of that `AttributeError` crashes the interpreter in `dir()` of the object.

## Reproducer

A script that uses only ZODB, `persistent` and MappingStorage follows the race test: one thread invalidates `obj1` and reads `obj1` and `obj2`, the other increments both and commits, 500 times each, with `sys.setswitchinterval(5e-6)`. Each run is a new process under `faulthandler`. The GIL is on in each run, because `zodbpickle._pickle` turns it on when ZODB loads.

| `persistent` | Interpreter | Runs | Failed | Of which crashed |
| --- | --- | --- | --- | --- |
| stock 6.8 | 3.14t | 50 | 35 | 11 |
| branch `free-threading` | 3.14t | 50 | 36 | 16 |
| `6.8+zoe.1` | 3.14t | 50 | 0 | 0 |
| `6.8+zoe.1`, `PYTHON_GIL=0` | 3.14t | 20 | 0 | 0 |
| stock 6.8 | 3.13, 3.14 | 20 each | 0 | 0 |
| stock 6.8, `PURE_PYTHON=1` | 3.14t | 20 | 0 | 0 |
| branch `free-threading`, `gc.disable()` | 3.14t | 20 | 0 | 0 |
| branch `free-threading`, one lock around each iteration | 3.14t | 10 | 0 | 0 |

The pure-Python implementation does not fail, so the bug is in the C code. With the garbage collector off, it does not fail either.

## Root cause

A Python subclass of `Persistent` gets `Py_TPFLAGS_INLINE_VALUES` on CPython 3.13 and later. Its attributes live in the inline values of the object. `_PyObject_GetDictPtr()` creates a dict that uses these values: the `ma_values` of the dict point into the object. In stock 6.8, `ghostify()` (`cPersistence.c`, line 192) and `Per__p_deactivate()` (line 319) set the dict pointer to NULL and release the dict. The inline values stay valid, and the released dict still points at them. The next load creates a new dict on the same values and puts the new state in them. While the old dict is alive, a clear of the old dict clears the state of the object. `PyObject_ClearManagedDict()` exists for this case. It copies the values into the dict before it releases it.

On a build with the GIL, the release frees the old dict at once, so the bug shows only when user code keeps a reference to the old `__dict__`. On the free-threaded build, biased reference counting keeps the old dict alive. ZODB moves connections between threads through its pool, so the thread that releases the dict is often not the thread that created it. The release then puts the dict in the queue of its owner thread. A garbage collection in any thread merges that queue, finds the dict with no reference, treats it as garbage and calls `tp_clear()` on it. `tp_clear()` clears the shared values.

The interleaving, with connection C used by thread A and then by thread B:

1. Thread A loads `obj1` of C. `pickle___setstate__()` creates dict D1, owned by thread A, on the inline values of `obj1`.
2. A closes C. B opens C and invalidates `obj1`. `_p_deactivate()` and `ghostify()` set the dict pointer of `obj1` to NULL and release D1. B does not own D1, so D1 waits in the queue of A.
3. B reads `obj1.value`. The load creates dict D2 on the same inline values and stores the new value in them. D2 counts one item.
4. A collection runs. It merges the queue of A and clears D1 with `tp_clear()`. This sets the inline values of `obj1` to NULL.
5. D2 still counts one item, but its value is NULL. `pickle_copy_dict()` (line 423) gets that NULL from `PyDict_Next()`, and `PyObject_SetItem()` raises the `SystemError`. An attribute read raises `AttributeError`, and `dir()` reads the NULL and crashes.

Steps 2 to 4 need no access to one object from two threads at the same time. They need a collection while the old dict waits for its owner thread. The second regression test of the fork runs these steps in this order, with no concurrent access, and fails in most runs without the fix.

## Fix

`detach_dict()` in `cPersistence.c` now serves `ghostify()` and `_p_deactivate()` on all builds. For a type with inline values, it takes a reference to the dict and calls `PyObject_ClearManagedDict()`, which moves the values into the dict and releases the dict of the object. The released dict then owns its values, so a clear of it does not touch the object. On the free-threaded build, a critical section on the object serializes most calls from two threads that deactivate it. The section pauses when the dict lock in `PyObject_ClearManagedDict()` blocks, and then both threads can get the dict. Concurrent use of one object is not supported. `PyObject_ClearManagedDict()` is documented only for clear and dealloc, so the fix depends on the internals of CPython 3.13 and 3.14 and needs a check again on 3.15.

The fork adds two tests to `persistent/tests/test_threading.py`. One keeps the old `__dict__`, invalidates and loads the object, then clears the old dict. The other loads the object in a second thread, then invalidates and loads it in the main thread and runs `gc.collect()`. Without the fix on 3.14t, both fail with the same `SystemError`: the first in each run, the second in most runs. The first also fails on 3.14. The suite of the fork passes on 3.12, 3.13, 3.14 and 3.14t: 785 tests, 0 failures, 6 skipped.

## The GIL at import

The branch `free-threading` declares free-threading support in `cPersistence`, `cPickleCache` and `_timestamp` on 3.14t and later. With the fork, importing `persistent` keeps the GIL off. Importing ZODB turns it on, because `zodbpickle._pickle` 4.5 does not declare support. The race needs no GIL-free interpreter: it fails with the GIL on.

## Limits

A sweep of the cache of a connection that another thread uses, for example `DB.cacheMinimize()` from another thread, is still not supported. The thread that uses the objects can get `AttributeError` or `SystemError`.
