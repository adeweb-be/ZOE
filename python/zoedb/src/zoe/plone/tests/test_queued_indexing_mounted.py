"""Acceptance test of queued indexing on a site in a mounted ZOE store,
under a root database without KV namespaces.

A Zope instance can keep its root database on a storage without KV
namespaces (a FileStorage, or a ZEO server) and mount a ZOE store at a
mount point of its configuration. A Plone site just below that mount point
keeps its catalog in the ZOE store, so its saves queue their catalog
operations there (ADR 0047). The queue of that site must not depend on a
save in the process that serves it.

The test checks, with the option on:

- process one: a root database on a FileStorage, and a `zoe.Storage`
  mounted at `/mounted` with a Plone site `/mounted/site`. An add of a
  Document queues its operations in the ZOE store and writes no entry of
  the catalog. No indexer thread runs, so the keys stay in the queue. Then
  the process ends: both databases close.
- process two, a restart: both databases open again on the same files, the
  configuration of Zope names the mount point, and Zope sends
  `IDatabaseOpenedWithRoot`. The process makes no save. The thread of the
  process applies the queue of the site: the queue becomes empty, and the
  catalog finds the Document.

The layers of plone.app.testing need the interpreter of a Plone instance.
`bin/test -s zoe.plone` of the instance of the zoe scenario runs it, on the
layer of `zoe.plone.testing`. `just plone zoe backends` runs it. The two
processes are two pairs of databases in the process of the test, as the
tests of the module `test_indexing` put a site in another database. The
layers do not load the ZCML of `zoe.plone`, so the test registers the
subscribers of `IDatabaseOpenedWithRoot` that its ZCML files name.
"""

import shutil
import tempfile
import time
import unittest
from unittest import mock

import App.config
import transaction
import ZODB
from Acquisition import aq_base
from OFS.Application import Application
from OFS.Folder import Folder
from plone.app.testing import PLONE_FUNCTIONAL_TESTING, SITE_OWNER_NAME
from plone.testing import zope as zope_testing
from Products.CMFUid.UniqueIdGeneratorTool import UniqueIdGeneratorTool
from Testing.makerequest import makerequest
from ZODB.FileStorage import FileStorage
from zope.component import getGlobalSiteManager
from zope.component.hooks import getSite, setSite
from zope.event import notify
from zope.globalrequest import clearRequest, setRequest
from zope.processlifetime import DatabaseOpenedWithRoot, IDatabaseOpenedWithRoot

import zoe
import zoe.kv

# The longest time that the test waits for the thread, in seconds.
TIMEOUT = 30
# The name of the mounted database, and its mount point.
MOUNTED = "mounted"
MOUNT_POINT = "/mounted"


def switch_on(test):
    """Switch queued indexing on until the end of the test.

    The helper of the acceptance test of queued indexing: it calls
    `install()` of the module `zoe.plone.indexing`, and `uninstall()` at
    the end of the test.
    """
    from zoe.plone import indexing

    indexing.install()
    test.addCleanup(indexing.uninstall)


def subscribe_to_the_opening(test):
    """Register the subscribers of `IDatabaseOpenedWithRoot` that the ZCML
    files of `zoe.plone` name, until the end of the test, as Zope does
    when it loads them. The layers of plone.app.testing do not load the
    ZCML of `zoe.plone`."""
    import importlib
    import pathlib
    import xml.etree.ElementTree as ElementTree

    import zoe.plone

    event = "zope.processlifetime.IDatabaseOpenedWithRoot"
    registry = getGlobalSiteManager()
    found = 0
    for path in sorted(pathlib.Path(zoe.plone.__file__).parent.glob("*.zcml")):
        for element in ElementTree.parse(path).iter():
            if not element.tag.endswith("subscriber"):
                continue
            if element.get("for") != event:
                continue
            name = element.get("handler")
            if name.startswith("."):
                name = "zoe.plone" + name
            module, _, attribute = name.rpartition(".")
            handler = getattr(importlib.import_module(module), attribute)
            registry.registerHandler(handler, (IDatabaseOpenedWithRoot,))
            test.addCleanup(
                registry.unregisterHandler, handler, (IDatabaseOpenedWithRoot,)
            )
            found += 1
    test.assertTrue(found, "zoe.plone subscribes to no IDatabaseOpenedWithRoot")


def stop_threads():
    """Stop the indexer threads that `IDatabaseOpenedWithRoot` started, and
    forget the root database of the event, as the end of a process does.

    ADR 0047 names no call that stops the threads: a process stops them at
    its exit. The human approves a change of this helper with the ADR.
    Until then, the helper stops each thread of `zoe.plone.indexing` and
    clears the root database that the module keeps, as the tests of the
    module `test_indexing` do.
    """
    from zoe.plone import indexing

    services = list(indexing._services.values())
    for service in services:
        service.stop()
    for service in services:
        if hasattr(service, "join"):
            service.join(TIMEOUT)
    indexing._services.clear()
    indexing._root.clear()


def queue_name(catalog):
    """Return the name of the queue of the catalog `catalog`."""
    from zoe.plone import indexing

    return indexing.queue_name(aq_base(catalog)._p_oid)


class Process:
    """The databases of one Zope process: a root database on a FileStorage,
    which has no KV namespaces, and a `zoe.Storage` mounted under it."""

    def __init__(self, directory):
        self.root = ZODB.DB(FileStorage(f"{directory}/Data.fs"), database_name="main")
        self.mounted = ZODB.DB(
            zoe.Storage(f"{directory}/zoe"),
            databases=self.root.databases,
            database_name=MOUNTED,
        )

    def close(self):
        transaction.abort()
        self.mounted.close()
        self.root.close()


class TestQueuedIndexingInAMountedStore(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        setRequest(self.layer["request"])
        self.addCleanup(clearRequest)
        site = getSite()
        self.addCleanup(setSite, site)
        switch_on(self)
        self.directory = tempfile.mkdtemp(prefix="zoe-plone-mounted-")
        self.addCleanup(shutil.rmtree, self.directory, True)
        # The event also wires portal_uidgenerator: give the class its
        # method back at the end of the test.
        patcher = mock.patch.object(
            UniqueIdGeneratorTool, "__call__", UniqueIdGeneratorTool.__call__
        )
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(stop_threads)

    def first_process(self, word):
        """Create the site in the mounted store and add a Document with the
        word `word` in its title, with no indexer thread. Return the number
        of keys in the queue of the site."""
        from zoe.plone import indexing

        process = Process(self.directory)
        try:
            connection = process.root.open()
            try:
                app = Application()
                connection.root()["Application"] = app
                app.acl_users._doAddUser(SITE_OWNER_NAME, "qisecret", ["Manager"], [])
                folder = Folder(MOUNTED)
                connection.get_connection(MOUNTED).add(folder)
                app._setObject(MOUNTED, folder)
                transaction.commit()
                app = makerequest(connection.root()["Application"])
                zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
                from Products.CMFPlone.factory import addPloneSite

                # The site of the layer is not a site of this process.
                setSite(None)
                indexing.uninstall()
                try:
                    addPloneSite(
                        app[MOUNTED],
                        "site",
                        setup_content=False,
                        extension_ids=("plone.app.contenttypes:default",),
                    )
                    transaction.commit()
                finally:
                    indexing.install()
                site = app[MOUNTED]["site"]
                catalog = aq_base(site.portal_catalog)
                self.assertIs(catalog._p_jar.db(), process.mounted)
                setSite(site)
                site.invokeFactory("Document", "doc", title=f"Mounted {word}")
                transaction.commit()
                queue = zoe.kv.Namespace(catalog._p_jar, "durable", queue_name(catalog))
                keys = len(queue.scan())
                found = site.portal_catalog.unrestrictedSearchResults(
                    SearchableText=word
                )
                self.assertEqual(len(found), 0)
                return keys
            finally:
                transaction.abort()
                connection.close()
        finally:
            zope_testing.logout()
            setSite(None)
            process.close()

    def test_the_thread_applies_the_queue_of_a_mounted_site_after_a_restart(self):
        word = "qimountedx"
        keys = self.first_process(word)
        self.assertGreater(keys, 0, "the add queued no operation")

        # The restart: the same files, the mount point in the configuration
        # of Zope, and the event of Zope. The process makes no save.
        process = Process(self.directory)
        self.addCleanup(process.close)
        # The threads stop before the databases close.
        self.addCleanup(stop_threads)
        dbtab = App.config.getConfiguration().dbtab
        patcher = mock.patch.dict(dbtab.mount_paths, {MOUNT_POINT: MOUNTED})
        patcher.start()
        self.addCleanup(patcher.stop)
        subscribe_to_the_opening(self)
        notify(DatabaseOpenedWithRoot(process.root))

        def state():
            """Return the keys of the queue and the paths of the catalog
            that finds the word, in a new connection."""
            tm = transaction.TransactionManager()
            connection = process.root.open(transaction_manager=tm)
            try:
                app = makerequest(connection.root()["Application"])
                catalog = app[MOUNTED]["site"].portal_catalog
                base = aq_base(catalog)
                queue = zoe.kv.Namespace(base._p_jar, "durable", queue_name(base))
                brains = catalog.unrestrictedSearchResults(SearchableText=word)
                return len(queue.scan()), sorted(b.getPath() for b in brains)
            finally:
                tm.abort()
                connection.close()

        deadline = time.monotonic() + TIMEOUT
        left = state()
        while left[0] and time.monotonic() < deadline:
            time.sleep(0.1)
            left = state()
        self.assertEqual(
            left,
            (0, ["/mounted/site/doc"]),
            f"the thread did not apply the queue in {TIMEOUT} s",
        )
