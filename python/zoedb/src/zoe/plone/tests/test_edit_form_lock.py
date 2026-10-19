"""Acceptance test of the lock of an edit form on the locking backend.

The edit form of a Document locks the Document when it opens: the
behaviour `plone.locking` of plone.app.lockingbehavior calls
`lockOnEditBegins` of plone.locking on the `IEditBegunEvent` of the form.
With the stock storage of plone.locking, that GET writes the Document: a
WebDAV `LockItem` in its `_dav_writelocks`, and the type of the lock in
its annotation `plone.locking` (ADR 0043). With the locking backend of
`zoe.plone`, the lock goes to the ephemeral namespace of the store.

The test checks, on the functional layer of plone.app.testing on a ZOE
store, with the locking backend on:

- the GET of the edit form of a Document, through the publisher of Zope,
  commits no record;
- after that GET, the Document is locked.

A GET of the view of the Document runs first, so that a write of a first
request of the site does not count.

The backend as built passes this test, so the test is a guard. With the
stock storage of plone.locking, the same GET commits five records: the
Document, its annotations, the mapping of the types of the locks, the
mapping `_dav_writelocks` and the `LockItem`.

`just plone zoe backends` runs the tests of `zoe.plone` with
`ZOE_PLONE_BACKENDS=locking`, and `just plone zoe backends -t
test_edit_form_lock` runs only this module. Without the backend, the test
skips, as the integration tests of the backend do.
"""

import unittest

import transaction
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    SITE_OWNER_NAME,
    SITE_OWNER_PASSWORD,
    TEST_USER_ID,
    setRoles,
)
from plone.locking.interfaces import ILockable
from plone.testing.zope import Browser
from ZODB.utils import get_pickle_metadata, p64, u64

from zoe.plone import locking

# The id of the Document of the test.
DOCUMENT = "document"


def records_after(storage, tid):
    """Return the records that `storage` committed after `tid`, as a list
    of `(oid, class)` pairs, in the order of the transactions."""
    found = []
    for record in storage.iterator(p64(u64(tid) + 1)):
        for data in record:
            module, name = get_pickle_metadata(data.data)
            found.append((u64(data.oid), f"{module}.{name}"))
    return found


class TestTheLockOfAnEditForm(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    # A failure shows each record.
    maxDiff = None

    def setUp(self):
        if not locking.installed():
            self.skipTest("the locking backend is not installed")
        portal = self.layer["portal"]
        setRoles(portal, TEST_USER_ID, ["Manager"])
        portal.invokeFactory("Document", DOCUMENT, title="A Document")
        transaction.commit()
        self.document = portal[DOCUMENT]
        self.browser = Browser(self.layer["app"])
        self.browser.handleErrors = False
        self.browser.addHeader(
            "Authorization", f"Basic {SITE_OWNER_NAME}:{SITE_OWNER_PASSWORD}"
        )

    def test_the_get_of_the_edit_form_writes_no_object(self):
        storage = self.layer["zodbDB"].storage
        url = self.document.absolute_url()
        self.browser.open(url)
        before = storage.lastTransaction()
        self.browser.open(f"{url}/@@edit")
        self.assertIn('name="form.buttons.save"', self.browser.contents)
        self.assertEqual(records_after(storage, before), [])
        transaction.begin()
        self.assertTrue(ILockable(self.document).locked())
