"""Acceptance test of the session backend of `zoe.plone` (ADR 0043).

With the backend, the sessions of Zope (`Products.Sessions`, the
`session_data_manager` of the Zope root and `REQUEST.SESSION`) live in the
ephemeral namespace of the store, with a TTL, not in the object graph. A
session write then is no content write. The test checks, on a Zope root in
a `zoe.Storage`:

- the tests of the session data manager of Products.Sessions
  (`Products.Sessions.tests.testSessionDataManager`) pass with the backend
  installed, on a Zope root in a `zoe.Storage`;
- a session write commits with its transaction: a commit writes it, and
  another connection reads it; an abort drops it. The commit holds no
  record: the session writes no object;
- a session expires after the timeout of its container, without a pack,
  and no record of the store ever held its value.

The objects of the suite of Products.Sessions are a browser id manager, a
session data manager and the session data container at the path that the
manager names. The suite makes them with `_populate`. The helpers
`backend_on` and `populate` name the backend and the container of the
backend. The card does not name them. The ADR of the card names them; the
human approves a change of these helpers with the ADR.

The suite of Products.Sessions needs Products.TemporaryFolder, its test
dependency, which gives `Products.Transience`. The tests of the browser id
manager and of the initialization of the product do not touch a session
container, so this module does not run them.

The Zope root needs the interpreter of a Plone instance. So this module is
in the package, with the other tests of `zoe.plone`: `just plone zoe
backends -t test_sessions` runs only this module. The expiry test waits
for the timeout of one minute of its container.
"""

import contextlib
import shutil
import tempfile
import time
import unittest

import transaction
from Products.Sessions.tests import testSessionDataManager as suite
from ZODB.DB import DB
from ZODB.utils import p64, u64

# The timeout of the container of the expiry test, in minutes. A container
# of Products.Transience takes whole minutes.
TIMEOUT_MINUTES = 1
# The longest time that the expiry test waits for its session to expire, in
# seconds: the timeout, the period of a container of Products.Transience,
# and a margin.
EXPIRY_WAIT = 180
# The value that the tests write in a session. The test looks for it in the
# records of the store.
MARKER = "a value of a zoe session"


def backend_on(test):
    """Install the session backend of `zoe.plone` until the end of `test`.

    The card does not name how a process installs the backend. The ADR of
    the card names it. The human approves a change of this helper with the
    ADR. Until then, the helper calls `install()` of the module
    `zoe.plone.sessions`, and `uninstall()` at the end of the test, as the
    tests of queued indexing do with `zoe.plone.indexing`.
    """
    try:
        from zoe.plone import sessions
    except ImportError as error:
        test.fail(f"zoe.plone has no session backend: {error}")
    sessions.install()
    test.addCleanup(sessions.uninstall)


def populate(app, use_temporary_folder=False, timeout_mins=20):
    """Put the objects of the suite of Products.Sessions into `app`, a Zope
    root, and commit: the browser id manager, the session data manager, and
    the session data container at `/temp_folder/temp_transient_container`,
    with a timeout of `timeout_mins` minutes.

    The ADR of the card names the container of the backend. The human
    approves a change of this helper with the ADR. Until then, the helper
    runs `_populate` of the suite, which adds a stock
    `TransientObjectContainer`, and sets its timeout. The installed backend
    keeps the sessions of that container in the namespace.
    """
    suite._populate(app, use_temporary_folder)
    container = app.temp_folder[suite.toc_name]
    if container.getTimeoutMinutes() != timeout_mins:
        container.setTimeoutMinutes(timeout_mins)
        transaction.commit()


def zoe_db(test, use_temporary_folder=False, timeout_mins=20):
    """Return a database on a new `zoe.Storage` whose Zope root holds the
    objects of the suite, until the end of `test`."""
    from OFS.Application import Application

    import zoe

    directory = tempfile.mkdtemp(prefix="zoe-sessions-")
    test.addCleanup(shutil.rmtree, directory, ignore_errors=True)
    db = DB(zoe.Storage(f"{directory}/zoe"), pool_size=60)
    test.addCleanup(db.close)
    connection = db.open()
    try:
        app = Application()
        connection.root()["Application"] = app
        transaction.savepoint(optimistic=True)
        populate(app, use_temporary_folder, timeout_mins)
    finally:
        transaction.abort()
        connection.close()
    return db


def records_after(storage, tid):
    """Return the oid of each record that the store committed after `tid`."""
    return [
        record.oid
        for transaction_ in storage.iterator(p64(u64(tid) + 1))
        for record in transaction_
    ]


def on_the_backend(case, use_temporary_folder):
    """Return a subclass of `case`, a test case of the suite of
    Products.Sessions, whose tests run with the backend installed, on a
    Zope root in a `zoe.Storage`.

    The suite keeps its database in `stuff["db"]`, and its `_getDB` makes
    one only when none is there. So the subclass puts the database on the
    `zoe.Storage` there first; the `tearDown` of the suite removes it.
    """

    def setUp(self):
        backend_on(self)
        suite.stuff["db"] = zoe_db(self, use_temporary_folder)
        case.setUp(self)

    return type(
        f"{case.__name__}OnTheBackend",
        (case,),
        {"setUp": setUp, "__module__": __name__},
    )


TestSessionManagerOnTheBackend = on_the_backend(suite.TestSessionManager, False)
TestSessionManagerWithTemporaryFolderOnTheBackend = on_the_backend(
    suite.TestSessionManagerWithTemporaryFolder, True
)


class TestSessionBackend(unittest.TestCase):
    def setUp(self):
        backend_on(self)
        self.db = zoe_db(self, timeout_mins=TIMEOUT_MINUTES)

    @contextlib.contextmanager
    def zope(self):
        """Yield the Zope root of a new connection, with a request, in the
        transaction of this thread. Abort and close at the end."""
        from Testing.makerequest import makerequest

        connection = self.db.open()
        try:
            transaction.begin()
            yield makerequest(connection.root()["Application"])
        finally:
            transaction.abort()
            connection.close()

    def new_session(self, **values):
        """Make a session with `values`, and commit. Return its browser id
        and the last tid before the commit."""
        last = self.db.storage.lastTransaction()
        with self.zope() as app:
            session = app.session_data_manager.getSessionData()
            for key, value in values.items():
                session[key] = value
            token = app.browser_id_manager.getBrowserId()
            transaction.commit()
        return token, last

    def session(self, token):
        """Return the values of the session `token` in a new connection, or
        None when it does not exist."""
        with self.zope() as app:
            session = app.session_data_manager.getSessionDataByKey(token)
            return None if session is None else dict(session.items())

    def test_a_session_write_commits_with_its_transaction_and_writes_no_object(
        self,
    ):
        token, last = self.new_session(committed=MARKER)
        self.assertEqual(self.session(token), {"committed": MARKER})
        # The commit wrote no object.
        self.assertEqual(records_after(self.db.storage, last), [])

        # A write that the transaction aborts is gone.
        with self.zope() as app:
            session = app.session_data_manager.getSessionDataByKey(token)
            session["aborted"] = MARKER
            transaction.abort()
        self.assertEqual(self.session(token), {"committed": MARKER})

        # A write of a session that exists commits too, and writes no
        # object either.
        last = self.db.storage.lastTransaction()
        with self.zope() as app:
            session = app.session_data_manager.getSessionDataByKey(token)
            session["second"] = MARKER
            transaction.commit()
        self.assertEqual(self.session(token), {"committed": MARKER, "second": MARKER})
        self.assertEqual(records_after(self.db.storage, last), [])

    def test_an_expired_session_is_gone_without_a_pack(self):
        token, _ = self.new_session(expiring=MARKER)
        self.assertEqual(self.session(token), {"expiring": MARKER})
        started = time.monotonic()
        while self.session(token) is not None:
            if time.monotonic() - started > EXPIRY_WAIT:
                self.fail(f"the session is still there after {EXPIRY_WAIT} s")
            time.sleep(5)
        # The store keeps no revision of the session: no record of any
        # transaction holds its value. The test runs no pack.
        held = [
            record.oid
            for transaction_ in self.db.storage.iterator()
            for record in transaction_
            if record.data and MARKER.encode() in record.data
        ]
        self.assertEqual(held, [])
