"""The recovery suite of ZODB: a copy into a second `zoe.Storage`.

`copyTransactionsFrom` restores each transaction of the store into the
destination `self._dst`.
"""

import os

from ZODB.tests.RecoveryStorage import RecoveryStorage

import zoe
import zoe.testing


class TestRecoveryStorage(zoe.testing.StorageTestCase, RecoveryStorage):
    def setUp(self):
        zoe.testing.StorageTestCase.setUp(self)
        self._dst = zoe.Storage(os.path.abspath("destination"))

    def tearDown(self):
        self._dst.close()
        zoe.testing.StorageTestCase.tearDown(self)
