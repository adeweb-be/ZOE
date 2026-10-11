"""The recovery suite of ZODB: a copy into a second `zoe.Storage`.

`copyTransactionsFrom` restores each transaction of the store into the
destination `self._dst`. The tests that undo are skipped.
"""

import os

from ZODB.tests.RecoveryStorage import RecoveryStorage

import zoe
import zoe.testing

UNDO = "the storage does not undo: supportsUndo() is false"
UNDO_TESTS = {
    "testRestoreAcrossPack",
    "testRestoreWithMultipleObjectsInUndoRedo",
    "testRestoreWithMultipleUndoRedo",
}


class TestRecoveryStorage(zoe.testing.StorageTestCase, RecoveryStorage):
    def setUp(self):
        if self._testMethodName in UNDO_TESTS:
            self.skipTest(UNDO)
        zoe.testing.StorageTestCase.setUp(self)
        self._dst = zoe.Storage(os.path.abspath("destination"))

    def tearDown(self):
        self._dst.close()
        zoe.testing.StorageTestCase.tearDown(self)
