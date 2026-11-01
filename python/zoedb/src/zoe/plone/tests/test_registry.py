"""Acceptance test of the registry backend of `zoe.plone` (ADR 0043).

With the backend, `plone.registry` keeps the fields and the values of a
moved registry in the durable namespace of the store, not in the pickles
of the registry. The move is an explicit step: a registry keeps its stock
`OOBTree`s until it moves. The test checks, on Plone sites of the layers of
plone.app.testing on a ZOE store:

- the suites of plone.app.registry (`test_setup`, `test_controlpanel`,
  `test_exportimport`) pass with the backend installed and, on a layer with
  a Plone site, with the registry of the site moved before each test;
- a moved registry reads each record that it held before its move, and
  its objects in the store hold a small part of the bytes that they held;
- two sites in one database keep their registries apart: a write of one
  does not reach the other, and the move back of one leaves each record of
  the other;
- a copy of a moved registry into another store, through its records,
  raises `Unsupported` at a read and at a write, and never reads as empty;
  once the rows of the registry come back, it reads each record;
- a copy of a moved registry inside one store (an export and an import, as
  a zexp does) raises `Unsupported` at a read and at a write, and the
  registry that it copies keeps its values;
- a save of a control panel without the token of plone.protect changes no
  record: plone.protect sees the write of a moved registry. With the token,
  the save changes it (a guard of the form).

The helpers `backend_on`, `move`, `move_back` and `bring_rows_back` name
the backend, the move, the move back and the way the rows of a registry
come back into another store. The card does not name them. The ADR of the
card names them; the human approves a change of these helpers with the
ADR.

The layers of plone.app.testing need the interpreter of a Plone instance.
So this module is in the package, with the other tests of `zoe.plone` that
need a Plone site: `just plone zoe backends -t test_registry` runs only
this module.
"""

import contextlib
import io
import unittest
import urllib.parse

import transaction
from Acquisition import aq_base
from plone.app.registry.tests import test_controlpanel, test_exportimport, test_setup
from plone.app.testing import (
    PLONE_FUNCTIONAL_TESTING,
    PLONE_SITE_ID,
    SITE_OWNER_NAME,
    SITE_OWNER_PASSWORD,
)
from plone.testing.zope import Browser
from ZODB.DB import DB
from ZODB.POSException import Unsupported
from ZODB.serialize import referencesf
from ZODB.utils import load_current

from zoe.plone.tests import helpers
from zoe.plone.tests.helpers import TITLE, copied_store, opened

# The id of the second site of a database.
OTHER_SITE_ID = "other"


def backend_on(test):
    """Install the registry backend of `zoe.plone` until the end of `test`.

    The card does not name how a process installs the backend. The ADR of
    the card names it. The human approves a change of this helper with the
    ADR. Until then, the helper calls `install()` of the module
    `zoe.plone.registry`, and `uninstall()` at the end of the test, as the
    tests of queued indexing do with `zoe.plone.indexing`.
    """
    try:
        from zoe.plone import registry
    except ImportError as error:
        test.fail(f"zoe.plone has no registry backend: {error}")
    registry.install()
    test.addCleanup(registry.uninstall)


def move(registry):
    """Move `registry`, a registry of plone.app.registry in a database, to
    the durable namespace of the store. The caller commits.

    The card makes the move an explicit step. The ADR of the card names it.
    The human approves a change of this helper with the ADR. Until then,
    the helper calls `move(registry)` of `zoe.plone.registry`.
    """
    from zoe.plone import registry as backend

    backend.move(registry)


def move_back(registry):
    """Move `registry` back into the object graph. The caller commits.

    The ADR of the card names the move back. The human approves a change of
    this helper with the ADR. Until then, the helper calls
    `move_back(registry)` of `zoe.plone.registry`.
    """
    from zoe.plone import registry as backend

    backend.move_back(registry)


def bring_rows_back(source, copy):
    """Give `copy`, a copy of the moved registry `source` in another store,
    the rows of `source`. The caller commits the transaction of `copy`.

    The card does not name how the rows of a registry come back into
    another store. The ADR of the card names it. The human approves a
    change of this helper with the ADR. Until then, the helper calls
    `copy_rows(source, copy)` of `zoe.plone.registry`.
    """
    from zoe.plone import registry as backend

    backend.copy_rows(source, copy)


def records(registry):
    """Return each record of `registry` with its value, by name."""
    return {name: registry[name] for name in registry.records.keys()}


def stored_bytes(obj):
    """Return the bytes of the current records of `obj` and of each
    persistent object that it reaches, in the store of its connection."""
    storage = obj._p_jar.db().storage
    seen = set()
    total = 0
    todo = [obj._p_oid]
    while todo:
        oid = todo.pop()
        if oid in seen:
            continue
        seen.add(oid)
        data, _ = load_current(storage, oid)
        total += len(data)
        todo.extend(referencesf(data))
    return total


class TestRegistryBackend(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING
    maxDiff = None

    def setUp(self):
        self.app = self.layer["app"]
        self.portal = self.layer["portal"]
        backend_on(self)

    def add_site(self, site_id):
        """Add a second Plone site to the Zope root, and commit."""
        return helpers.add_site(self.app, site_id)

    def test_a_moved_registry_reads_each_record_and_leaves_its_pickles(self):
        registry = self.portal.portal_registry
        before = records(registry)
        size = stored_bytes(aq_base(registry))
        move(registry)
        transaction.commit()
        self.assertEqual(records(registry), before)
        with opened(self.layer["zodbDB"]) as (app, _):
            moved = app[PLONE_SITE_ID].portal_registry
            self.assertEqual(records(moved), before)
            # The fields and the values are in the namespace now.
            self.assertLess(stored_bytes(aq_base(moved)), size / 10)

    def test_two_sites_in_one_database_keep_their_registries_apart(self):
        other = self.add_site(OTHER_SITE_ID)
        one = self.portal.portal_registry
        two = other.portal_registry
        before_one = records(one)
        before_two = records(two)
        move(one)
        move(two)
        transaction.commit()
        one[TITLE] = "Site one"
        transaction.commit()
        two[TITLE] = "Site two"
        transaction.commit()
        expected_one = {**before_one, TITLE: "Site one"}
        expected_two = {**before_two, TITLE: "Site two"}
        with opened(self.layer["zodbDB"]) as (app, _):
            self.assertEqual(records(app[PLONE_SITE_ID].portal_registry), expected_one)
            self.assertEqual(records(app[OTHER_SITE_ID].portal_registry), expected_two)
        # The move back of one registry leaves each record of the other.
        move_back(one)
        transaction.commit()
        with opened(self.layer["zodbDB"]) as (app, _):
            self.assertEqual(records(app[PLONE_SITE_ID].portal_registry), expected_one)
            self.assertEqual(records(app[OTHER_SITE_ID].portal_registry), expected_two)

    def test_a_copy_into_another_store_raises_unsupported_until_its_rows_come_back(
        self,
    ):
        registry = self.portal.portal_registry
        move(registry)
        transaction.commit()
        registry[TITLE] = "Moved"
        transaction.commit()
        expected = records(registry)

        storage, _ = copied_store(
            self, self.layer["zodbDB"].storage, "zoe-registry-copy-"
        )
        db = DB(storage)
        self.addCleanup(db.close)

        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            # The copy never reads as empty and takes no write.
            with self.assertRaises(Unsupported):
                copy[TITLE]
            with self.assertRaises(Unsupported):
                copy[TITLE] = "Written to the copy"
                manager.commit()
        with opened(db) as (app, manager):
            copy = app[PLONE_SITE_ID].portal_registry
            bring_rows_back(registry, copy)
            manager.commit()
        with opened(db) as (app, _):
            self.assertEqual(records(app[PLONE_SITE_ID].portal_registry), expected)

    def test_a_copy_inside_one_store_raises_unsupported(self):
        registry = self.portal.portal_registry
        move(registry)
        transaction.commit()
        registry[TITLE] = "Original"
        transaction.commit()
        # An export and an import, as a zexp does.
        jar = self.portal._p_jar
        exported = io.BytesIO()
        jar.exportFile(aq_base(registry)._p_oid, exported)
        exported.seek(0)
        self.app._registry_copy = jar.importFile(exported)
        transaction.commit()

        with opened(self.layer["zodbDB"]) as (app, manager):
            copy = app._registry_copy
            with self.assertRaises(Unsupported):
                copy[TITLE]
            with self.assertRaises(Unsupported):
                copy[TITLE] = "Written to the copy"
                manager.commit()
        with opened(self.layer["zodbDB"]) as (app, _):
            self.assertEqual(app[PLONE_SITE_ID].portal_registry[TITLE], "Original")

    def test_a_control_panel_save_without_a_token_changes_no_record(self):
        registry = self.portal.portal_registry
        move(registry)
        transaction.commit()
        browser = Browser(self.app)
        browser.handleErrors = False
        browser.addHeader(
            "Authorization", f"Basic {SITE_OWNER_NAME}:{SITE_OWNER_PASSWORD}"
        )
        url = f"{self.portal.absolute_url()}/@@site-controlpanel"

        def fields(title):
            """Open the form, set the site title, and return the fields that
            a click on Save sends."""
            browser.open(url)
            browser.getControl(name="form.widgets.site_title").value = title
            form = next(
                form
                for form in browser._response.forms.values()
                if "form.widgets.site_title" in form.fields
            )
            return form.submit_fields(name="form.buttons.save")

        def site_title():
            with opened(self.layer["zodbDB"]) as (app, _):
                return app[PLONE_SITE_ID].portal_registry[TITLE]

        # With the token, the save changes the record.
        browser.post(url, urllib.parse.urlencode(fields("With the token")))
        self.assertEqual(site_title(), "With the token")
        # Without the token, plone.protect aborts the save.
        sent = [
            (name, value)
            for name, value in fields("Without the token")
            if name != "_authenticator"
        ]
        with contextlib.suppress(Exception):
            browser.post(url, urllib.parse.urlencode(sent))
        self.assertEqual(site_title(), "With the token")


def on_the_backend(case):
    """Return a subclass of `case`, a test case of plone.app.registry, whose
    tests run with the registry backend installed and, on a layer with a
    Plone site, with the registry of the site moved before the test."""

    def setUp(self):
        backend_on(self)
        try:
            portal = self.layer["portal"]
        except KeyError:
            portal = None
        if portal is not None:
            move(portal.portal_registry)
        case.setUp(self)

    return type(
        f"{case.__name__}OnTheBackend",
        (case,),
        {"setUp": setUp, "__module__": __name__},
    )


# The suites of plone.app.registry, on the backend.
for _module in (test_setup, test_controlpanel, test_exportimport):
    for _case in list(vars(_module).values()):
        if (
            isinstance(_case, type)
            and issubclass(_case, unittest.TestCase)
            and _case.__module__ == _module.__name__
            and any(name.startswith("test") for name in dir(_case))
        ):
            _subclass = on_the_backend(_case)
            globals()[_subclass.__name__] = _subclass
del _module, _case, _subclass
