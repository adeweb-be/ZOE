"""Unit tests of the reads of the registry backend of zoe.plone (ADR 0050):
the ranges of a view, a row that is not a new pickle of its object, and
the early return of the guard.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, as `just plone zoe backends` runs them. They install
the backend themselves.
"""

import pickle
import unittest
from unittest import mock

import transaction
from Acquisition import aq_base
from BTrees.OOBTree import OOBTree
from plone.app.testing import PLONE_FUNCTIONAL_TESTING

from zoe.plone import registry as backend
from zoe.plone.registry_rows import FIELD, dumps, loads, record_key
from zoe.plone.tests.test_registry_backend import TITLE, namespace


class TestRegistryReads(unittest.TestCase):
    layer = PLONE_FUNCTIONAL_TESTING

    def setUp(self):
        backend.install()
        self.addCleanup(backend.uninstall)
        self.portal = self.layer["portal"]
        self.registry = self.portal.portal_registry

    def moved(self):
        """Move the registry of the site and commit. Return it."""
        backend.move(self.registry)
        transaction.commit()
        return self.registry

    # The ranges of a view.

    def test_a_range_of_a_view_gives_what_an_oobtree_gives(self):
        records = self.moved().records
        names = list(records.keys())
        tree = OOBTree({name: None for name in names})
        middle = names[len(names) // 2]
        bounds = [
            None,
            "",
            "a",
            names[0],
            middle,
            middle + "\x00",
            middle[:-1],
            names[-1],
            "\U0010ffff",
        ]
        for view in (records._fields, records._values):
            for low in bounds:
                for high in bounds:
                    self.assertEqual(
                        view.keys(low, high), list(tree.keys(low, high)), (low, high)
                    )
            for key in bounds:
                for method in ("minKey", "maxKey"):
                    try:
                        expected = getattr(tree, method)(key)
                    except ValueError:
                        with self.assertRaises(ValueError):
                            getattr(view, method)(key)
                    else:
                        self.assertEqual(getattr(view, method)(key), expected)

    # A row that is not a new pickle of its object.

    def stage_row(self, registry, key, data):
        """Stage `data` as the row `key` of the moved `registry`, as a write
        does, and commit."""
        state = backend._state(aq_base(registry)._records)
        state.bump()
        state.namespace.set(key, data)
        transaction.commit()

    def test_a_row_of_another_pickle_stays_after_a_read(self):
        registry = self.moved()
        key = record_key(FIELD, TITLE)
        canonical = namespace(registry).get(key)
        other = pickle.dumps(loads(canonical), protocol=4)
        self.assertNotEqual(other, canonical)
        self.stage_row(registry, key, other)
        counter = aq_base(registry)._records.counter
        title = registry.records[TITLE].field.title
        transaction.commit()
        self.assertEqual(aq_base(registry)._records.counter, counter)
        self.assertEqual(namespace(registry).get(key), other)
        # A change in place of such a field is still a change.
        registry.records[TITLE].field.title = title + " changed"
        transaction.commit()
        self.assertGreater(aq_base(registry)._records.counter, counter)
        self.assertEqual(loads(namespace(registry).get(key)).title, title + " changed")

    def test_a_read_of_a_row_of_the_move_pickles_the_field_once_and_writes_nothing(
        self,
    ):
        registry = self.moved()
        key = record_key(FIELD, TITLE)
        row = namespace(registry).get(key)
        counter = aq_base(registry)._records.counter
        with mock.patch.object(backend, "dumps", wraps=dumps) as counted:
            registry.records[TITLE].field.title  # noqa: B018
            transaction.commit()
        # The move writes a pickle that a new pickle of the field gives.
        self.assertEqual(counted.call_count, 1)
        self.assertEqual(aq_base(registry)._records.counter, counter)
        self.assertEqual(namespace(registry).get(key), row)

    def test_each_field_row_of_the_move_is_a_new_pickle_of_its_object(self):
        registry = self.moved()
        fields = {}
        cursor = None
        while True:
            page = namespace(registry).scan(cursor)
            if not page:
                break
            fields.update((key, data) for key, data in page if key[:1] == FIELD)
            cursor = page[-1][0] + b"\x00"
        self.assertTrue(fields)
        other = sorted(
            key for key, data in fields.items() if dumps(loads(data)) != data
        )
        self.assertEqual(other, [], f"{len(other)} of {len(fields)} rows differ")

    # The early return of the guard.

    def test_the_guard_reads_the_roots_once_in_a_transaction(self):
        transaction.commit()
        registry = aq_base(self.registry)
        with mock.patch.object(
            backend, "_holds_its_bucket", wraps=backend._holds_its_bucket
        ) as checked:
            registry.records  # noqa: B018
            first = checked.call_count
            registry.records  # noqa: B018
            registry.records  # noqa: B018
            self.assertEqual(first, 2)
            self.assertEqual(checked.call_count, first)
            # A root that the transaction changed is checked again.
            registry._records._values._p_changed = True
            registry.records  # noqa: B018
            self.assertEqual(checked.call_count, first + 2)
        transaction.abort()
        with mock.patch.object(
            backend, "_holds_its_bucket", wraps=backend._holds_its_bucket
        ) as checked:
            # A new transaction reads the roots current again.
            registry.records  # noqa: B018
            self.assertEqual(checked.call_count, 2)
        jar = registry._p_jar
        for tree in (registry._records._fields, registry._records._values):
            self.assertIn(tree._p_oid, jar._readCurrent)
        transaction.abort()
