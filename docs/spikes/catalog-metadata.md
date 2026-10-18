# Spike: the values of the metadata buckets of a catalog, and what BTrees compares on them

Card ZOE-213, step 1; requirement F6 (tier 1). The build of the card follows from it: `Heap::with_values` of `zoe_pickle::python`, the heap of tier 1.

**Verdict.** The metadata of a Plone 6.1 catalog holds three objects of other classes: `DateTime`, `Missing.Value` and the i18n `Message` of `Type`. Tier 1 can merge their buckets byte for byte as ZODB's resolver does, with no guess, when it models four facts. `DateTime.__eq__` compares the instant and the zone, so the same instant in another zone is a change for BTrees, not an equal value. `DateTime.__setstate__` replaces the zone of the state with a str of the table of DateTime, one object for each name in the whole process, so the merged pickle writes each name once. `Message.__reduce__` gives new arguments and a new copy of the text. `Missing.Value` is a singleton that is equal only to itself and above every other value. With them, the 75 conflicts of the acceptance test that went to the client now merge in the store.

## Question

Which values does `Catalog.data` of a Plone 6.1 site hold, how do they pickle, and which comparisons does `bucket_merge` make on them? What must tier 1 model to give the record of ZODB's resolver, and where must it give no answer?

## Method

- CPython 3.13, ZODB 6, BTrees 6.5, DateTime 6.0, pytz 2026.5, Missing 5.3, zope.i18nmessageid 8.3, zodbpickle with protocol 3: the environment of `python/`.
- Small scripts outside the repository pickled the values with the pickler of ZODB, loaded them again with `PersistentUnpickler` and `find_global` of `ZODB.ConflictResolution`, as the resolver does, and pickled them again. They compared each pair of 20 values with `<` and `==`. They ran `tryToResolveConflict` on records of `IOBucket` for the merged records. The bytes and the tables are the data of the new tests.
- The source of `MergeTemplate.c` and `_compat.h` of BTrees, `DateTime/DateTime.py`, `DateTime/pytz_support.py`, `Missing/__init__.py`, `zope/i18nmessageid/_zope_i18nmessageid_message.c`, and `PersistentReference` of `ZODB.ConflictResolution`.

## Findings

### The values

`Catalog.data` is an `IOBTree`: the rid of each object maps to a tuple of one value for each metadata column, 44 on a Plone 6.1 site. `recordify` gives `Missing.Value` for each column that the object does not give. A Document gives strs, bools, ints, tuples of strs and `None`, a `DateTime` for `created`, `modified`, `effective` and `expires`, an Event also for `start` and `end`, and a `Message` of the domain `plone` for `Type`. One `DateTime` can be in two columns (`created` and `modified` of a new item), and the pickle then writes it once and gets it from the memo.

### What BTrees compares

`bucket_merge` compares the values of a key that the three buckets hold with `TEST_VALUE`, which is `COMPARE` of `_compat.h`: `None` first, then `PyObject_RichCompareBool(a, b, Py_LT)`, then `Py_EQ`. A comparison that raises counts as "less" with an exception set. `_p_resolveConflict` then returns a state with the exception set, Python raises `SystemError`, and ZODB turns it into a `ConflictError`: a refusal. A script checked it on a `DateTime` against a str. On two tuples, `<` looks for the first pair of items that are not equal with `==` (an object is equal to itself without a call), then compares that pair with `<`. So the model needs `==` and `<` of each pair of kinds that a column can hold on two sides of a merge: the same kind, `Missing.Value` against a `DateTime` when `start` comes or goes, and a `Message` against a `Message`.

### DateTime

- It pickles as NEWOBJ of `DateTime.DateTime.DateTime` with no arguments, then BUILD of `(_micros, _timezone_naive, _tz)`.
- `__setstate__` calls `_parse_args(micros / 1e6, tz)`, which sets `_tz` to `_TZINFO._zmap[tz.lower()]`, then sets `_micros` and `_timezone_naive` from the state. The values of `_zmap` are 640 names (the names of pytz and the names of DateTime itself, such as `GMT+2`), and each maps to itself. So a state that holds one of these names keeps it. Another name, such as `utc` or `EST`, becomes another name, or fails.
- `_zmap` holds one str object for each name. Every `DateTime` of the resolver with that zone holds that object. So the merged pickle writes the name once and gets it from the memo after, also for two `DateTime`s from two of the three records.
- `__eq__` gives false for an object that is not a `DateTime`, else compares `(_micros, _tz)`. Two `DateTime`s of one instant in two zones are not equal. A save that only moves `modified` to another zone is a change of the value for BTrees.
- `lessThan` (`__lt__`) compares `_micros` with the `_micros` of the other object. `None` gives false, an int or a float gives a comparison with microseconds. Another object without `_micros` compares with the int `_micros`: a str, a tuple, a list, a dict or a bytes raises `TypeError`, and `Missing.Value` gives true through `Missing.__gt__`.
- In 1900 to 2999, `__setstate__` gives a state for each of the 640 names, and `__getstate__` gives the same state back.

### Missing.Value

- It pickles as the GLOBAL `Missing.V`, from `__reduce__`, which gives `'V'`. `find_global` gives the singleton, the same object in the three records.
- `__eq__` is `other is self`; `__lt__` gives false; `__gt__` gives true for each other object. Python calls the method of the left object first, so `x < Missing.Value` is true for each built-in value, a `DateTime` and a `Message`, and `Missing.Value < x` is false. It never raises, but a persistent reference on the left raises in its `__cmp__`.

### Message

- It pickles as REDUCE of `zope.i18nmessageid.message.Message` with 7 arguments: the text, the domain, the default, the mapping, the plural, the default plural and the number. A record of an older version can hold fewer.
- `__reduce__` gives a new tuple, a new str copy of the text (`str(self)`, which is not the shared str of one character; the empty str stays shared), the domain, the default and the other values that the object keeps, and a new dict of the mapping.
- A `Message` compares as a str: its text, without its domain. A message equal to the old one as a str, in another domain, is no change for BTrees, and the merge keeps the object of the side that it takes.

### Where the model gives no answer

A zone that is not one of the 640 names, a `_micros` out of 1900 to 2999, a state or arguments of another form, a `DateTime` without its BUILD, each other object of another class, a set, and a GLOBAL of `Missing` other than `V` (an alias of the singleton). A `<` of a number or a class against a `DateTime`, of a `Message` against a value that is not a str, and of a class against `Missing.Value` gives no answer too: Python can give an answer there, but the metadata of a catalog does not make these comparisons.

The table of names is the one of DateTime 6.0 with pytz 2026.5. A client with an older pytz cannot load a `DateTime` of a newer zone. A client of the same site wrote that zone, so the clients of one site load it. A version of DateTime that changes `__eq__` or `_zmap` changes the record of ZODB's resolver: the table and the rules belong to DateTime 6.0, the version of Plone 6.1.

## Results

The acceptance test `test_catalog_metadata.py` runs 120 seeded cases with the 44 columns of Plone 6.1. Before the build, outcomes and records matched, and 75 of the 120 cases ran `tryToResolveConflict` in the client on a metadata bucket, in each of the three modes. After it, the 4 tests pass: each case has the outcome and the records of FileStorage with ZODB's resolver, and no case where ZODB merges calls the client.

The new tests of `zoe_pickle::python` compare the bytes of a record with 20 values and the 800 comparisons of `<` and `==` of their pairs with CPython: the model gives the answer of Python or no answer, and it gives an answer for each `==` with a `DateTime`, a `Message` or `Missing.Value`. The tests of `structural_merges` merge three records of an `IOBucket` of metadata, written by CPython, to the record of `tryToResolveConflict`.

## B5 `contention-fields`

Step 3 of the card. One run of each binary of `zoe serve`, the branch and `main` at `a5c8ebd`, with the same instances and the same client: `zoe-native-qi`, the small fixture, steps `4:60,16:60`, `--port-offset 420`, the merge policy of the fixture (split merge off), on a Linux machine with a 1-minute load of 5 to 9 that other runs shared, so the times do not compare. Per step: ok operations per second, first-attempt successes, p95 in ms, `ConflictError`s at the vote, and their classes; then the outcomes of tier 1 over the run.

| binary | users | ok/s | 1st try | p95 | conflicts | classes |
| --- | ---: | ---: | ---: | ---: | ---: | --- |
| branch | 4 | 13.2 | 89.3 % | 905 | 119 | `IIBTree` 102, `IOBTree` 9, `OOBTree` 8 |
| branch | 16 | 26.9 | 89.7 % | 1,365 | 172 | `IIBTree` 118, `IOBTree` 30, `OOBTree` 24 |
| `main` | 4 | 6.4 | 86.7 % | 1,513 | 69 | `IIBTree` 55, `IOBTree` 7, `OOBTree` 6, `IOBucket` 1 |
| `main` | 16 | 28.5 | 92.1 % | 1,557 | 139 | `IIBTree` 109, `OOBTree` 15, `IOBTree` 14, `IOBucket` 1 |

Tier 1 over the run (warm-up included): the branch merged 5, refused 317 and gave no answer for 5; `main` merged 10, refused 231 and gave no answer for 6. The clients resolved nothing; 5 and 6 resolutions in the clients failed on `OOBucket`.

With queued indexing, a save of `contention-fields` does not write the catalog: the indexer of the site applies the catalog operations one batch after the other, so two saves rarely meet on a metadata bucket. The conflicts that remain are on the nodes of trees (`IIBTree`, `IOBTree`, `OOBTree`), which tier 1 refuses as BTrees does, and which the split merge of ADR 0048 (off in these runs) takes. This card does not move them, and the two runs differ by their load, not by the card. The gate of phase 4 counts `ConflictError`s, and a conflict on a metadata bucket that ZODB merges was never one: the client merged it before this card, and the store merges it now, one round trip earlier. So the gate does not need this card; the replay counts these conflicts under `native` instead of `python-client`.
