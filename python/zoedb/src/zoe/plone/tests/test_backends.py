"""Integration tests of the locking backend of zoe.plone on a Plone site.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing` with `ZOE_PLONE_BACKENDS=locking`, as
`just plone zoe backends` runs them. Without the backend, they skip.
"""

import pickle
import unittest

import transaction
from Acquisition import aq_base
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, TEST_USER_ID, setRoles
from plone.locking.interfaces import ILockable
from zope.annotation.interfaces import IAnnotations

import zoe.kv
from zoe.plone import locking


class TestLocking(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        if not locking.installed():
            self.skipTest("the locking backend is not installed")
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        portal.invokeFactory("Document", "doc")
        transaction.commit()
        self.doc = portal["doc"]

    def row(self):
        namespace = zoe.kv.Namespace(self.doc._p_jar, "ephemeral", locking.NAMESPACE)
        return namespace.get(self.doc._p_oid)

    def test_a_lock_writes_the_namespace_and_no_object(self):
        serial = self.doc._p_serial
        lockable = ILockable(self.doc)
        lockable.lock()
        self.assertTrue(lockable.locked())
        transaction.commit()
        # The lock is in the namespace, under the oid of the item.
        stored = pickle.loads(self.row())
        self.assertEqual(len(stored["locks"]), 1)
        self.assertEqual(set(stored["types"]), {"plone.locking.stealable"})
        # The item and its annotations stay as they were.
        self.assertEqual(self.doc._p_serial, serial)
        self.assertNotIn("_dav_writelocks", aq_base(self.doc).__dict__)
        self.assertNotIn("plone.locking", IAnnotations(self.doc))
        info = lockable.lock_info()
        self.assertEqual(info[0]["creator"], TEST_USER_ID)
        lockable.unlock()
        transaction.commit()
        self.assertIsNone(self.row())
        self.assertFalse(ILockable(self.doc).locked())

    def test_a_rollback_to_a_savepoint_drops_the_lock(self):
        lockable = ILockable(self.doc)
        # A read first, so the rows of the transaction exist before the
        # savepoint; then the rows join after one.
        for read_first in (True, False):
            if read_first:
                self.assertFalse(lockable.locked())
            savepoint = transaction.savepoint()
            lockable.lock()
            self.assertTrue(lockable.locked())
            savepoint.rollback()
            self.assertFalse(lockable.locked(), read_first)
            transaction.commit()
            self.assertIsNone(self.row(), read_first)

    def test_a_refresh_before_a_savepoint_stays_after_its_rollback(self):
        ILockable(self.doc).lock()
        transaction.commit()

        def stored():
            (lock,) = pickle.loads(self.row())["locks"].values()
            return lock.getModifiedTime()

        before = stored()
        # The writes of the namespaces join first, then the rows; then the
        # rows join first.
        for writes_first in (True, False):
            if writes_first:
                other = zoe.kv.Namespace(self.doc._p_jar, "durable", "other")
                other.set("a", b"1")
            (lock,) = self.doc.wl_lockValues()
            # A change in place, as `refresh` makes.
            lock._modifiedtime = lock.getModifiedTime() + 100
            transaction.savepoint().rollback()
            transaction.commit()
            self.assertGreater(stored(), before, writes_first)
            before = stored()

    def test_an_invalid_lock_goes_as_ofs_deletes_it(self):
        lockable = ILockable(self.doc)
        lockable.lock()
        transaction.commit()
        (lock,) = self.doc.wl_lockValues()
        # The lock ended long ago.
        lock._modifiedtime -= 10**6
        self.assertFalse(self.doc.wl_isLocked())
        transaction.commit()
        stored = self.row()
        self.assertTrue(stored is None or not pickle.loads(stored)["locks"])

    def test_a_read_of_the_lock_writes_nothing(self):
        ILockable(self.doc).lock()
        transaction.commit()
        stored = self.row()
        ILockable(self.doc).lock_info()
        ILockable(self.doc).locked()
        transaction.commit()
        self.assertEqual(self.row(), stored)
