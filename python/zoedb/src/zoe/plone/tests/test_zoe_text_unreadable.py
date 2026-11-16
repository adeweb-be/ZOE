"""Acceptance test of the profile steps of the text index on a site that
holds an object whose blob file cannot be read.

The tests run on `ZOE_TEXT_SITE`. `setUp` adds three Files with a text
file each, and removes the blob file of the third one from the blob
directory of the store, so the read of its data raises `POSKeyError`.
They check that:

- the import of `zoe.plone:text` finishes, the three indexes are
  `ZoeTextIndex`, a text query of each finds the two readable Files, and
  the log names the path of the third one;
- the import of `zoe.plone:text-uninstall` does the same, with
  ZCTextIndex indexes.

A cleanup puts the blob file back, removes the Files, and leaves the site
with the `ZoeTextIndex` indexes of the layer.

`just plone zoe backends` runs these tests.
"""

import logging
import os
import unittest

import transaction
from plone.namedfile.file import NamedBlobFile
from Products.ZCTextIndex.ZCTextIndex import ZCTextIndex

from zoe.plone import testing as zoe_testing
from zoe.plone import textindex
from zoe.plone.tests.test_catalog_mirror import key_of, opened, wait_ready
from zoe.plone.tests.test_zoe_text_index import wait_text

# The ids, titles, descriptions and file texts of the Files. The blob file
# of `UNREADABLE` is removed.
FILES = {
    "f1": ("Comet report one", "Comet notes one", b"The comet crosses the sky."),
    "f2": ("Comet report two", "Comet notes two", b"The comet has a long tail."),
    "f3": ("Comet report three", "Comet notes three", b"The comet comes back."),
}

# The File whose blob file is removed.
UNREADABLE = "f3"

# The word that each File holds in each text index.
WORD = "comet"


class TestAnUnreadableObject(unittest.TestCase):
    layer = zoe_testing.ZOE_TEXT_SITE

    def setUp(self):
        self.db = self.layer["zoeDB"]
        self.storage = self.layer["zoeStorage"]
        self.blob_path = None
        self.addCleanup(self.clean)
        with opened(self.db) as site:
            for oid, (title, description, data) in FILES.items():
                site.invokeFactory(
                    "File",
                    oid,
                    title=title,
                    description=description,
                    file=NamedBlobFile(
                        data, contentType="text/plain", filename=f"{oid}.txt"
                    ),
                )
            transaction.commit()
            self.key = key_of(site)
        # A new blob has no serial after its commit: a load gives it.
        self.db.cacheMinimize()
        with opened(self.db) as site:
            blob = site[UNREADABLE].file._blob
            blob._p_activate()
            self.blob_path = blob.committed()
        wait_ready(self.storage, self.key)
        wait_text(self.storage, self.key)
        self.blob_mode = os.stat(self.blob_path).st_mode
        with open(self.blob_path, "rb") as f:
            self.blob_data = f.read()

    def clean(self):
        if self.blob_path is not None and not os.path.exists(self.blob_path):
            with open(self.blob_path, "wb") as f:
                f.write(self.blob_data)
            os.chmod(self.blob_path, self.blob_mode)
        self.db.cacheMinimize()
        with opened(self.db) as site:
            pc = site.portal_catalog
            if not isinstance(
                pc._catalog.getIndex("SearchableText").aq_base,
                textindex.ZoeTextIndex,
            ) or any(k.startswith(textindex.PENDING) for k in pc._catalog.indexes):
                site.portal_setup.runAllImportStepsFromProfile("profile-zoe.plone:text")
            site.manage_delObjects([oid for oid in FILES if oid in site.objectIds()])
            transaction.commit()
            key = key_of(site)
        wait_text(self.storage, key)

    def remove_blob(self):
        """Remove the blob file of `UNREADABLE`, and make each object of
        the connections a ghost, so the next read loads the blob again."""
        os.remove(self.blob_path)
        self.db.cacheMinimize()

    def import_logged(self, profile):
        """Import `profile` and commit. Return the messages that the logger
        of the text index gives at WARNING."""
        with (
            self.assertLogs("zoe.plone.textindex", logging.WARNING) as logs,
            opened(self.db) as site,
        ):
            site.portal_setup.runAllImportStepsFromProfile(profile)
            transaction.commit()
        return [record.getMessage() for record in logs.records]

    def check(self, kind, messages):
        """Check that the text indexes are of `kind`, that a query of each
        finds the two readable Files, and that `messages` name the path of
        the unreadable one."""
        with opened(self.db) as site:
            pc = site.portal_catalog
            self.assertEqual(
                [k for k in pc._catalog.indexes if k.startswith(textindex.PENDING)],
                [],
            )
            for name in textindex.TEXT_INDEXES:
                self.assertIsInstance(pc._catalog.getIndex(name).aq_base, kind, name)
                found = {b.getId for b in pc.unrestrictedSearchResults(**{name: WORD})}
                self.assertEqual(found, set(FILES) - {UNREADABLE}, name)
            path = "/".join(site[UNREADABLE].getPhysicalPath())
        self.assertTrue(any(path in m for m in messages), messages)

    def test_the_install_step_skips_the_unreadable_file(self):
        with opened(self.db) as site:
            site.portal_setup.runAllImportStepsFromProfile(
                "profile-zoe.plone:text-uninstall"
            )
            transaction.commit()
        self.remove_blob()
        messages = self.import_logged("profile-zoe.plone:text")
        wait_text(self.storage, self.key)
        self.check(textindex.ZoeTextIndex, messages)

    def test_the_uninstall_step_skips_the_unreadable_file(self):
        self.remove_blob()
        messages = self.import_logged("profile-zoe.plone:text-uninstall")
        self.check(ZCTextIndex, messages)
