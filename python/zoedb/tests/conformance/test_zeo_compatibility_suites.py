r"""Conformance: tests of ZEO's suites that fail on zoe serve itself.

`just conformance` runs this module on each interpreter.

`zoe.testing.ZEO_LAYER` makes the test forker of ZEO start `zoe serve`
(`test_zeo_harness.py`). The tests of ZEO's suites that this module runs
need no change to the forker. They fail today because `zoe serve` answers
differently from ZEO's server:

- `ThreadTests` and `blob_transaction.txt` store with the serial
  `'\0' * 8`, a str. ZEO's server takes it; `zoe serve` fails the vote
  with `TypeError: an oid or a tid is not bytes`.
- `history(None, None)` raises `TypeError` for the size, where ZEO's
  server raises `POSKeyError` for the oid.
- The client sends `iterator_gc` without waiting, then `iterator_next` for
  the same iterator. The calls of one connection run concurrently on
  `zoe serve`, so `iterator_next` can run first and not raise `KeyError`.
- After a frame that is not a call, the server closes the connection, and
  the client does not connect again in time for its next transaction.
  On CPython 3.12 the client cannot connect again after such a close (its
  asyncio registers the closed socket), so these tests run from 3.13.

The tests of ZEO run unchanged, under `ZEO_LAYER` and their own layers. A
test passes; a skip counts as a failure. The tests that fail only now and
then run several times.
"""

import sys

import pytest
from suite_support import run_under_zeo_layer, select

# Each id is `test.id()`.
ZEO_TESTS = [
    "ZEO.tests.testZEO.MappingStorageTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.MappingStorageTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkDisconnectedOnThread2Close",
    "ZEO.tests.testZEO.BlobWritableCacheTests.checkSecondBeginFails",
    "ZEO.tests.testZEO.ZRPCConnectionTests.checkExceptionLogsAtError",
]

# The blob suite of ZODB on a client that does not share the blob directory
# of the server, as `ZEO.tests.testZEO` builds it.
NON_SHARED_BLOB_TESTS = [
    "blob_transaction_txt",
]

# These fail only now and then: each runs RUNS times.
ITERATOR_TESTS = [
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkIteratorGCStorageCommitting",
    "ZEO.tests.testZEO.BlobAdaptedFileStorageTests.checkIteratorGCStorageTPCAborting",
]

BAD_MESSAGE_TESTS = [
    "ZEO.tests.testConnection.FileStorageConnectionTests.checkBadMessage1",
    "ZEO.tests.testConnection.FileStorageConnectionTests.checkBadMessage2",
    "ZEO.tests.testConnection.MappingStorageConnectionTests.checkBadMessage1",
    "ZEO.tests.testConnection.MappingStorageConnectionTests.checkBadMessage2",
]

RUNS = 5


def test_the_thread_and_history_tests_of_testzeo_pass(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    run_under_zeo_layer(select(testZEO.test_suite(), ZEO_TESTS))


def test_the_blob_transactions_of_a_client_without_a_shared_blob_directory_pass(
    tmp_path, monkeypatch
):
    from ZEO.tests import testZEO
    from ZODB.tests import testblob

    monkeypatch.chdir(tmp_path)
    suite = testblob.storage_reusable_suite(
        "ClientStorageNonSharedBlobs", testZEO.ServerManagingClientStorage
    )
    run_under_zeo_layer(select(suite, NON_SHARED_BLOB_TESTS))


def test_the_iterator_gc_tests_pass_each_time(tmp_path, monkeypatch):
    from ZEO.tests import testZEO

    monkeypatch.chdir(tmp_path)
    run_under_zeo_layer(select(testZEO.test_suite(), ITERATOR_TESTS) * RUNS)


@pytest.mark.skipif(
    sys.version_info < (3, 13),
    reason=(
        "the asyncio of CPython 3.12 registers a closed socket after a broken "
        "pipe, so the client does not connect again after the server closes "
        "on a bad frame; CPython 3.13 fixes it"
    ),
)
def test_the_bad_message_tests_of_testconnection_pass_each_time(tmp_path, monkeypatch):
    from ZEO.tests import testConnection

    monkeypatch.chdir(tmp_path)
    run_under_zeo_layer(select(testConnection.test_suite(), BAD_MESSAGE_TESTS) * RUNS)
