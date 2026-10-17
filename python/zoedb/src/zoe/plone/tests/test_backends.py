"""Integration tests of the backends of zoe.plone on a Plone site.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing` with `ZOE_PLONE_BACKENDS=locking,registry`, as
`just plone zoe backends` runs them. A backend that is not installed skips
its tests.
"""

import pickle
import unittest

import transaction
from Acquisition import aq_base
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, TEST_USER_ID, setRoles
from plone.locking.interfaces import ILockable
from plone.registry.interfaces import IRegistry
from zope.annotation.interfaces import IAnnotations
from zope.component import getUtility

import zoe.kv
from zoe.plone import locking, registry


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

    def test_a_read_of_the_lock_writes_nothing(self):
        ILockable(self.doc).lock()
        transaction.commit()
        stored = self.row()
        ILockable(self.doc).lock_info()
        ILockable(self.doc).locked()
        transaction.commit()
        self.assertEqual(self.row(), stored)


class TestRegistry(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        if not registry.installed():
            self.skipTest("the registry backend is not installed")
        self.registry = getUtility(IRegistry)

    def stored(self, name):
        namespace = zoe.kv.Namespace(self.registry._p_jar, "durable", registry.NAMESPACE)
        encoded = namespace.get(registry.VALUE + name.encode())
        return None if encoded is None else pickle.loads(encoded)

    def test_the_records_live_in_the_namespace(self):
        records = self.registry.records
        self.assertIsInstance(records, registry.KVRecords)
        name = "plone.site_title"
        self.assertIn(name, self.registry)
        self.registry[name] = "Another title"
        transaction.commit()
        self.assertEqual(self.stored(name), "Another title")
        self.assertEqual(self.registry[name], "Another title")
        self.assertEqual(self.registry.records[name].value, "Another title")
        # The names come in the order of the stock trees.
        names = list(self.registry.records.keys())
        self.assertEqual(names, sorted(names, key=str.encode))

    def test_the_records_move_back_into_the_registry(self):
        from plone.registry.registry import _Records

        name = "plone.site_title"
        self.registry[name] = "Moved back"
        transaction.commit()
        count = len(self.registry.records)
        registry.use_object_graph(self.registry)
        transaction.commit()
        records = self.registry._records
        self.assertIs(type(records), _Records)
        self.assertEqual(records._values[name], "Moved back")
        self.assertEqual(len(records._values), count)
        self.assertIsNone(self.stored(name))
