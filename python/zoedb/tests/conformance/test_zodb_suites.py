"""Conformance test: the test suites of ZODB pass against `zoe.Storage`.

`just conformance` runs this module on each interpreter of `just test`:
CPython 3.12, 3.13, 3.14 and 3.14t.

`zoe.testing` gives two names:

- `STORAGE_LAYER`, a layer of zope.testrunner. Between its `testSetUp` and
  its `testTearDown`, the default storage of ZODB is a `zoe.Storage` in a new
  directory: `ZODB.DB(None)`, `ZODB.MappingStorage.DB()` and
  `ZODB.tests.util.DB()` open one. A test that names a storage class gets
  that class.
- `StorageTestCase`, the base class of the storage suites of ZODB. Its
  `setUp` opens a `zoe.Storage` in a new directory as `self._storage`. Its
  `open(read_only=False)` opens the same store again.

The run collects the suites of ZODB unchanged and runs each test under
`STORAGE_LAYER` and under the layers of the test:

- the storage suites: BasicStorage, MTStorage, Synchronization,
  ConflictResolution, PackableStorage, RevisionStorage, HistoryStorage,
  IteratorStorage, PersistentStorage, ReadOnlyStorage and
  TransactionalUndoStorage;
- the blob suite of ZODB for a blob storage, with recovery and undo;
- the test modules of ZODB in `FULL`, on the default storage.

The run skips only the tests in `SKIPS`, each with its reason. ZODB itself
skips the tests in `OWN_SKIPS`.

These parts of ZODB do not apply to the storage, so the run does not
collect them:

- `PackableStorageWithOptionalGC`: it packs without garbage collection, and
  the pack of the store always collects garbage. `PackableStorage` runs.
- the modules that test one named storage, its file format or its tools:
  `testFileStorage`, `testfsIndex`, `testfsoids`, `test_fsdump`,
  `testRecover`, `testMappingStorage`, `testMVCCMappingStorage`,
  `testDemoStorage`, `test_storage` (`MinimalMemoryStorage`) and `testConfig`
  (the configuration sections of these storages).
- `testblob`: its suite tests the blob wrapper of ZODB over a FileStorage.
  The blob suite for a blob storage runs.
- `blob_packing.txt` of the blob suite: it finds the blob files through
  `fshelper`, an attribute of the blob storages of ZODB that `IBlobStorage`
  does not declare. The acceptance test of pack checks the blob files.
- `testdocumentation`: it needs the source tree of ZODB.
- `testhistoricalconnections`: it replaces `time.time`, and the store takes
  its tids from its own clock.
- the tests in `OUT`, each for the reason that `OUT` gives.
"""

import doctest
import importlib
import unittest

import pytest
from suite_support import cases_and_layers
from ZODB.tests.TransactionalUndoStorage import TransactionalUndoStorage

import zoe
import zoe.testing

# The storage suites of ZODB: each module and its test classes.
STORAGE_SUITES = {
    "BasicStorage": ["BasicStorage"],
    "MTStorage": ["MTStorage"],
    "Synchronization": ["SynchronizedStorage"],
    "ConflictResolution": [
        "ConflictResolvingStorage",
        "ConflictResolvingTransUndoStorage",
    ],
    "PackableStorage": ["PackableStorage", "PackableUndoStorage"],
    "RevisionStorage": ["RevisionStorage"],
    "HistoryStorage": ["HistoryStorage"],
    "IteratorStorage": ["IteratorStorage", "ExtendedIteratorStorage"],
    "PersistentStorage": ["PersistentStorage"],
    "ReadOnlyStorage": ["ReadOnlyStorage"],
    "TransactionalUndoStorage": ["TransactionalUndoStorage"],
}

# The test modules of ZODB that run on the default storage.
FULL = [
    "testActivityMonitor",
    "testBroken",
    "testCache",
    "testConnection",
    "testConnectionSavepoint",
    "testDB",
    "testPersistentList",
    "testPersistentMapping",
    "testPersistentWeakref",
    "testSerialize",
    "testThreadedShutdown",
    "testUtils",
    "testZODB",
    "test_TransactionMetaData",
    "test_cache",
    "test_doctest_files",
    "test_mvccadapter",
    "test_prefetch",
    "test_racetest",
    "testconflictresolution",
    "testcrossdatabasereferences",
    "testmvcc",
    "testpersistentclass",
]

# The tests that the run skips, and the reason of each skip. A name is the
# class that defines the test and the name of the test method.
SKIPS = {
    "TransactionalUndoStorage.testTransactionalUndoIterator": (
        "it checks that the record of an undo names the tid of the record that "
        "it copies (data_txn); the store keeps every pickle whole and no back "
        "pointer, so data_txn is None, and a back pointer is a format change"
    ),
    "PackableUndoStorage.testPackUndoLog": (
        "it replaces time.time and packs at a time of its fake clock, and the "
        "store takes its tids from its own clock: the pack time comes after "
        "the last transaction"
    ),
}

# ZODB skips these tests itself when the test case cannot open a second
# client of the storage. They can also pass.
OWN_SKIPS = {
    "RaceTests.test_race_external_invalidate_vs_disconnect",
    "RaceTests.test_race_load_vs_external_invalidate",
}

# The tests that the run does not collect, and the reason of each.
OUT = {
    "ZODB.tests.testDB.passing_None_to_DB": (
        "it checks that DB(None) opens a MappingStorage, and the layer opens "
        "a zoe.Storage"
    ),
    "ZODB.tests.testDB.connectionDebugInfo": (
        "it replaces time.time, and the store takes its tids from its own clock"
    ),
    "ZODB.tests.testblob.loadblob_tmpstore": (
        "it calls loadBlob after close, and a closed storage refuses each call"
    ),
    "ZODB.tests.testblob.commit_from_wrong_partition": (
        "it replaces os.rename to test the copy helper of the blob storages "
        "of ZODB, and the store moves its blob files itself"
    ),
}


class UndoHelpers:
    """The helper of `TransactionalUndoStorage` that
    `ConflictResolvingTransUndoStorage` calls. The test classes of
    FileStorage mix both in. It holds no test."""

    _begin_undos_vote = TransactionalUndoStorage._begin_undos_vote


def name_of(test):
    """Return the name of a test in SKIPS, OWN_SKIPS and OUT.

    A test method has the name of the class that defines it and its own
    name. A doctest has its id, without a directory.
    """
    method = getattr(test, "_testMethodName", "runTest")
    if isinstance(test, doctest.DocTestCase) or method == "runTest":
        return test.id().rsplit("/", 1)[-1]
    for cls in type(test).__mro__:
        if method in vars(cls):
            return f"{cls.__name__}.{method}"
    raise AssertionError(f"no class defines {test.id()}")


def run(suite):
    """Run suite under STORAGE_LAYER and the layers of its tests.

    Skip the tests in SKIPS with their reason. Do not run the tests in OUT.
    Return the result and the number of tests that the run collected.
    """
    result = unittest.TestResult()
    started = []
    collected = 0
    try:
        for test, layers in cases_and_layers(suite, (zoe.testing.STORAGE_LAYER,)):
            name = name_of(test)
            if name in OUT:
                continue
            collected += 1
            if name in SKIPS:
                result.startTest(test)
                result.addSkip(test, SKIPS[name])
                result.stopTest(test)
                continue
            for layer in layers:
                if layer not in started:
                    layer.setUp()
                    started.append(layer)
            for layer in layers:
                if hasattr(layer, "testSetUp"):
                    layer.testSetUp()
            try:
                test.run(result)
            finally:
                for layer in reversed(layers):
                    if hasattr(layer, "testTearDown"):
                        layer.testTearDown()
    finally:
        for layer in reversed(started):
            layer.tearDown()
    return result, collected


def check(result, collected):
    """Assert that each collected test ran and passed, or has a skip."""
    problems = [
        f"{test.id()}\n{trace}" for test, trace in result.failures + result.errors
    ]
    problems += [
        f"{test.id()}: unexpected success" for test in result.unexpectedSuccesses
    ]
    assert not problems, "\n\n".join(problems)
    assert collected > 0
    assert result.testsRun == collected

    skipped = {name_of(test): reason for test, reason in result.skipped}
    unexpected = {
        name: reason
        for name, reason in skipped.items()
        if name not in SKIPS and name not in OWN_SKIPS
    }
    assert not unexpected, unexpected
    for name in skipped.keys() & SKIPS.keys():
        assert skipped[name] == SKIPS[name], name


def test_the_layer_makes_a_zoe_storage_the_default_storage_of_zodb():
    import ZODB
    import ZODB.MappingStorage
    import ZODB.tests.util
    from ZODB.MappingStorage import MappingStorage

    layer = zoe.testing.STORAGE_LAYER
    # zope.testrunner names a layer by these attributes.
    assert isinstance(layer.__name__, str)
    assert isinstance(layer.__bases__, tuple)

    db = ZODB.DB(None)
    assert type(db.storage) is MappingStorage
    db.close()

    layer.setUp()
    try:
        layer.testSetUp()
        try:
            dbs = [ZODB.DB(None), ZODB.MappingStorage.DB(), ZODB.tests.util.DB()]
            for db in dbs:
                assert isinstance(db.storage, zoe.Storage)
            # Each database has a store of its own.
            assert len({db.storage.getName() for db in dbs}) == len(dbs)
            with dbs[0].transaction() as conn:
                conn.root()["key"] = "value"
            with dbs[1].transaction() as conn:
                assert "key" not in conn.root()
            # A test that names a storage class gets that class.
            named = MappingStorage()
            assert type(named) is MappingStorage
            named.close()
        finally:
            layer.testTearDown()

        db = ZODB.DB(None)
        assert type(db.storage) is MappingStorage
        db.close()
    finally:
        layer.tearDown()


@pytest.mark.parametrize("module_name", sorted(STORAGE_SUITES))
def test_the_storage_suite_of_zodb_passes(module_name):
    module = importlib.import_module(f"ZODB.tests.{module_name}")
    loader = unittest.defaultTestLoader
    suite = unittest.TestSuite()
    for class_name in STORAGE_SUITES[module_name]:
        tests = getattr(module, class_name)
        bases = (zoe.testing.StorageTestCase, tests, UndoHelpers)
        case = type(f"Zoe{class_name}", bases, {})
        names = loader.getTestCaseNames(case)
        # The run collects the tests of ZODB unchanged.
        assert set(names) == set(loader.getTestCaseNames(tests))
        for name in names:
            assert getattr(case, name) is getattr(tests, name), name
        suite.addTests(loader.loadTestsFromTestCase(case))

    check(*run(suite))


def test_the_blob_suite_of_zodb_passes():
    from ZODB.tests import testblob

    suite = testblob.storage_reusable_suite(
        "Zoe",
        lambda name, blob_dir: zoe.Storage(name, blob_dir=blob_dir),
        test_blob_storage_recovery=True,
        test_packing=False,
        test_undo=True,
    )
    names = {name_of(test) for test, _ in cases_and_layers(suite)}
    assert "RecoveryBlobStorage.testSimpleBlobRecovery" in names
    assert "blob_transaction_txt" in names

    check(*run(suite))


@pytest.mark.parametrize("module_name", FULL)
def test_the_test_module_of_zodb_passes_on_the_default_storage(module_name):
    module = importlib.import_module(f"ZODB.tests.{module_name}")
    if hasattr(module, "test_suite"):
        suite = module.test_suite()
    else:
        suite = unittest.defaultTestLoader.loadTestsFromModule(module)

    check(*run(suite))
