"""Integration tests of the backends of zoe.plone on a Plone site.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing` with `ZOE_PLONE_BACKENDS=locking,registry`, as
`just plone zoe backends` runs them. A backend that is not installed skips
its tests. The CSRF tests of a control panel run with and without the
registry backend, and pass on both.
"""

import pickle
import shutil
import tempfile
import unittest
from urllib.parse import urlencode

import transaction
import ZODB
from Acquisition import aq_base
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    SITE_OWNER_NAME,
    SITE_OWNER_PASSWORD,
    TEST_USER_ID,
    setRoles,
)
from plone.locking.interfaces import ILockable
from plone.registry.interfaces import IRegistry
from plone.testing.zope import Browser
from zope.annotation.interfaces import IAnnotations
from zope.component import getUtility

import zoe
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


class TestRegistry(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        if not registry.installed():
            self.skipTest("the registry backend is not installed")
        self.registry = getUtility(IRegistry)

    def stored(self, name):
        namespace = zoe.kv.Namespace(
            self.registry._p_jar, "durable", registry.NAMESPACE
        )
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

    def test_a_read_moves_no_record_and_the_move_is_explicit(self):
        from plone.registry.registry import _Records

        portal = self.layer["portal"]
        name = "plone.site_title"
        self.registry[name] = "Explicit"
        transaction.commit()
        registry.use_object_graph(portal)
        transaction.commit()
        # A read of a stock registry leaves it as it is.
        self.assertEqual(self.registry[name], "Explicit")
        self.assertIs(type(self.registry.records), _Records)
        self.assertFalse(self.registry._p_changed)
        registry.use_namespace(portal)
        transaction.commit()
        self.assertIsInstance(self.registry._records, registry.KVRecords)
        self.assertEqual(self.stored(name), "Explicit")
        self.assertEqual(self.registry[name], "Explicit")

    def test_a_new_registry_moves_its_records_at_its_first_write(self):
        from plone.registry import field
        from plone.registry.record import Record
        from plone.registry.registry import Registry
        from ZODB.FileStorage import FileStorage

        directory = tempfile.mkdtemp(prefix="zoe-registry-")
        try:
            source = ZODB.DB(zoe.Storage(f"{directory}/store"))
            conn = source.open()
            new = Registry()
            new.records["plone.a"] = Record(field.TextLine(title="a"), "one")
            conn.root()["registry"] = new
            transaction.commit()
            # Before a write with a connection, the registry keeps its
            # records itself, and a read changes nothing.
            self.assertTrue(new.records.local())
            self.assertEqual(new["plone.a"], "one")
            self.assertFalse(new._p_changed)
            # The first write moves every record into the namespace.
            new.records["plone.b"] = Record(field.TextLine(title="b"), "two")
            self.assertFalse(new.records.local())
            transaction.commit()
            namespace = zoe.kv.Namespace(conn, "durable", registry.NAMESPACE)
            for name, value in (("plone.a", "one"), ("plone.b", "two")):
                encoded = namespace.get(registry.VALUE + name.encode())
                self.assertEqual(pickle.loads(encoded), value)
                self.assertEqual(new[name], value)
            conn.close()
            # A copy through the ZODB iterator leaves the rows out, and the
            # registry of the copy never reads as empty.
            copy = FileStorage(f"{directory}/Data.fs")
            copy.copyTransactionsFrom(source.storage)
            source.close()
            db = ZODB.DB(copy)
            conn = db.open()
            try:
                copied = conn.root()["registry"]
                with self.assertRaises(zoe.kv.Unsupported):
                    copied["plone.a"]
                with self.assertRaises(zoe.kv.Unsupported):
                    len(copied.records)
            finally:
                transaction.abort()
                conn.close()
                db.close()
        finally:
            shutil.rmtree(directory, ignore_errors=True)

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


class TestRegistryCSRF(unittest.TestCase):
    """plone.protect refuses a control panel save without its CSRF token, with
    the registry backend as with the stock registry: the backend marks the
    registry as changed, so plone.protect sees the write."""

    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        portal = self.layer["portal"]
        self.url = portal.absolute_url() + "/@@social-controlpanel"
        self.browser = Browser(self.layer["app"])
        self.browser.handleErrors = False
        self.browser.addHeader(
            "Authorization", f"Basic {SITE_OWNER_NAME}:{SITE_OWNER_PASSWORD}"
        )

    def value(self):
        transaction.begin()
        return getUtility(IRegistry)["plone.twitter_username"]

    def test_a_save_without_its_token_is_refused(self):
        query = urlencode(
            {
                "form.widgets.twitter_username": "an-account",
                "form.widgets.share_social_data-empty-marker": "1",
                "form.buttons.save": "Save",
            }
        )
        self.browser.open(f"{self.url}?{query}")
        self.assertIn("@@confirm-action", self.browser.url)
        self.assertNotEqual(self.value(), "an-account")

    def test_a_save_with_its_token_writes_the_registry(self):
        self.browser.open(self.url)
        control = self.browser.getControl(name="form.widgets.twitter_username")
        control.value = "an-account"
        self.browser.getControl(name="form.buttons.save").click()
        self.assertEqual(self.value(), "an-account")
