"""The helpers that the tests of `zoe.plone` share.

zope.testrunner reads no `conftest.py`, so the tests import this module.
It holds no test.

- `copy_records`, `copied_store` and `open_site` copy the site of a layer
  to a new `zoe.Storage`, as the tests that need the tiers of the store
  do.
- `run_round` and `round_errors` run the rounds of the acceptance tests
  of concurrent creations.
- `add_site` adds a second Plone site to the Zope root.
- `RegistryBackend` is the setup of the tests of the registry backend.
"""

import collections
import contextlib
import shutil
import tempfile
import threading

import transaction
from Acquisition import aq_base
from plone.app.testing import PLONE_SITE_ID, TEST_USER_NAME, login, logout
from Testing.makerequest import makerequest
from ZODB.Connection import TransactionMetaData
from ZODB.DB import DB
from ZODB.serialize import referencesf
from ZODB.utils import get_pickle_metadata, load_current, z64
from zope.component.hooks import getSite, setSite
from zope.globalrequest import clearRequest, getRequest, setRequest

# The record of the registry that the tests of the registry write.
TITLE = "plone.site_title"


def describe(error):
    """Return the text of `error` for a failure message."""
    return f"{type(error).__name__}: {error}"


def wire_ids(test):
    """Give `portal_uidgenerator` its ids from a sequence of the store, as
    `zoe.plone` does when Zope opens its database, until the end of
    `test`."""
    from Products.CMFUid.UniqueIdGeneratorTool import UniqueIdGeneratorTool

    from zoe.plone import uidgenerator

    stock = UniqueIdGeneratorTool.__call__
    uidgenerator.install()
    if UniqueIdGeneratorTool.__call__ is not stock:
        test.addCleanup(setattr, UniqueIdGeneratorTool, "__call__", stock)


def copy_records(source, target):
    """Copy the current record of each object that the root of `source`
    reaches to `target`, an empty storage.

    Each record keeps its oid and its serial. The records of one serial go
    in one restore transaction under that tid, in the order of the tids. So
    the `_p_mtime` of each object stays the same, and Dexterity gives each
    schema the name that the catalog holds.
    """
    records = collections.defaultdict(list)
    seen = set()
    todo = [z64]
    while todo:
        oid = todo.pop()
        if oid in seen:
            continue
        seen.add(oid)
        data, serial = load_current(source, oid)
        records[serial].append((oid, data))
        todo.extend(referencesf(data))
    for serial in sorted(records):
        meta = TransactionMetaData()
        meta.note("copy of the site of the layer")
        target.tpc_begin(meta, serial, " ")
        try:
            for oid, data in records[serial]:
                if get_pickle_metadata(data) == ("ZODB.blob", "Blob"):
                    blob = source.loadBlob(oid, serial)
                    target.restoreBlob(oid, serial, data, blob, None, meta)
                else:
                    target.restore(oid, serial, data, "", None, meta)
            target.tpc_vote(meta)
        except BaseException:
            target.tpc_abort(meta)
            raise
        target.tpc_finish(meta)


def new_directory(test, prefix):
    """Make a new temporary directory whose name starts with `prefix`.
    The directory goes at the end of `test`. Return its path."""
    directory = tempfile.mkdtemp(prefix=prefix)
    test.addCleanup(shutil.rmtree, directory, ignore_errors=True)
    return directory


def copy_into(source, target):
    """Copy the records of the storage `source` to `target`, an empty
    storage, with `copy_records`. When the copy fails, close `target` and
    raise. Return `target`."""
    try:
        copy_records(source, target)
    except BaseException:
        target.close()
        raise
    return target


def copied_store(test, source, prefix, **options):
    """Copy the records of the storage `source` to a new `zoe.Storage`,
    opened with the keyword arguments `options`, in a new directory whose
    name starts with `prefix`. The directory goes at the end of `test`.
    Return the storage, open, and the path of its store."""
    import zoe

    path = f"{new_directory(test, prefix)}/zoe"
    return copy_into(source, zoe.Storage(path, **options)), path


def open_site(test, options, prefix, pool_size, user=None):
    """Copy the site of the layer of `test` to a new `zoe.Storage`, opened
    with the keyword arguments `options`, in a new directory whose name
    starts with `prefix`. Return the site on that store, on a connection of
    this thread, until the end of the test. With `user`, that user is
    logged in.

    `test.db` is the database of the store, with `pool_size` connections,
    and `test.portal` the site.
    """
    storage, _ = copied_store(test, test.layer["zodbDB"].storage, prefix, **options)
    test.db = DB(storage, pool_size=pool_size)
    test.addCleanup(test.db.close)
    connection = test.db.open()
    test.addCleanup(connection.close)
    test.addCleanup(setRequest, getRequest())
    test.addCleanup(setSite, getSite())
    test.addCleanup(transaction.abort)
    app = makerequest(connection.root()["Application"])
    setRequest(app.REQUEST)
    portal = app[PLONE_SITE_ID]
    setSite(portal)
    if user is not None:
        login(portal, user)
    test.portal = portal
    return portal


def run_round(test, count, change, timeout):
    """Run one round on the database `test.db`: `count` threads, each with
    a connection of its own and logged in as the test user, call
    `change(site, turn)` from one snapshot. Then they commit, one after the
    other, in the order of their turns. Return the error of each turn that
    raised, by turn.

    Each thread must end within `timeout` seconds. Then the connection of
    this thread sees the commits of the round.
    """
    db = test.db
    changed = threading.Barrier(count, timeout=timeout)
    done = [threading.Event() for _ in range(count)]
    errors = {}

    def run(turn):
        connection = db.open()
        try:
            app = makerequest(connection.root()["Application"])
            setRequest(app.REQUEST)
            site = app[PLONE_SITE_ID]
            setSite(site)
            login(site, TEST_USER_NAME)
            change(site, turn)
            # Each turn changes the site from the snapshot of its
            # connection. Then the turns commit.
            changed.wait()
            if turn:
                done[turn - 1].wait(timeout)
            transaction.commit()
        except BaseException as error:
            errors[turn] = error
        finally:
            done[turn].set()
            transaction.abort()
            logout()
            setSite(None)
            clearRequest()
            connection.close()

    threads = [threading.Thread(target=run, args=(turn,)) for turn in range(count)]
    for thread in threads:
        thread.start()
    for thread in threads:
        thread.join(timeout)
        test.assertFalse(thread.is_alive())
    transaction.begin()
    return errors


def round_errors(create_round, rounds):
    """Run `create_round(number)` for each of `rounds` rounds. Return the
    errors of each round that had one, as text, by round and turn."""
    failed = {}
    for number in range(rounds):
        errors = create_round(number)
        if errors:
            failed[number] = {
                turn: describe(error) for turn, error in sorted(errors.items())
            }
    return failed


def add_site(app, site_id, extension_ids=()):
    """Add a Plone site with the id `site_id` and the profiles
    `extension_ids` to the Zope root `app`, and commit. The current site
    stays. Return the new site."""
    from Products.CMFPlone.factory import addPloneSite

    site = getSite()
    try:
        addPloneSite(app, site_id, extension_ids=extension_ids)
    finally:
        setSite(site)
    transaction.commit()
    return app[site_id]


@contextlib.contextmanager
def opened(db):
    """Yield the Zope root of a new connection of `db` and its transaction
    manager, a manager of its own. Abort and close the connection at the
    end."""
    manager = transaction.TransactionManager()
    connection = db.open(transaction_manager=manager)
    try:
        manager.begin()
        yield connection.root()["Application"], manager
    finally:
        manager.abort()
        connection.close()


def namespace(registry):
    """Return the namespace of the moved `registry`."""
    import zoe.kv

    registry = aq_base(registry)
    return zoe.kv.Namespace(registry._p_jar, "durable", registry._records.name)


class RegistryBackend:
    """The setup of a test case of the registry backend of `zoe.plone`.

    A test case takes it as a base before `unittest.TestCase`. `setUp`
    installs the backend until the end of the test. `portal` is the site
    of the layer, `db` the database of the layer, and `registry` the
    registry of the site. `moved()` moves the registry.
    """

    def setUp(self):
        from zoe.plone import registry as backend

        backend.install()
        self.addCleanup(backend.uninstall)
        self.portal = self.layer["portal"]
        self.db = self.layer["zodbDB"]
        self.registry = self.portal.portal_registry

    def moved(self):
        """Move the registry of the site and commit. Return it."""
        from zoe.plone import registry as backend

        backend.move(self.registry)
        transaction.commit()
        return self.registry
