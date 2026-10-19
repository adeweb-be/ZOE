"""Acceptance tests of the cost of a read of a moved registry of the
registry backend of zoe.plone (ADR 0050): the work of `keys(min, max)` and
of `values()`.

They run with `bin/test -s zoe.plone` of a Plone instance, on the layer of
`zoe.plone.testing`, as `just plone zoe backends` runs them. They install
the backend themselves.
"""

import collections
import unittest
from unittest import mock

import transaction
from plone.app.testing import PLONE_FUNCTIONAL_TESTING

from zoe.plone import registry as backend
from zoe.plone import registry_rows


class Bound(str):
    """A bound of `keys(min, max)` that counts the comparisons with it.

    A comparison of a name with an instance of this subclass of str calls
    a method of this class first, also when the name is on the left.
    """

    compared = 0

    def __lt__(self, other):
        Bound.compared += 1
        return str.__lt__(self, other)

    def __le__(self, other):
        Bound.compared += 1
        return str.__le__(self, other)

    def __gt__(self, other):
        Bound.compared += 1
        return str.__gt__(self, other)

    def __ge__(self, other):
        Bound.compared += 1
        return str.__ge__(self, other)


class TestRegistryReadCost(unittest.TestCase):
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

    def test_keys_of_a_range_compare_a_number_of_names_that_grows_as_a_log(self):
        registry = self.moved()
        names = list(registry.records.keys())
        count = len(names)
        self.assertGreater(count, 100)
        low, high = names[count // 2], names[count // 2 + 3]
        Bound.compared = 0
        found = registry.records.keys(Bound(low), Bound(high))
        compared = Bound.compared
        self.assertEqual(found, [name for name in names if low <= name <= high])
        limit = 4 * (count.bit_length() + 1) + len(found)
        self.assertLessEqual(
            compared,
            limit,
            f"keys(min, max) compared {compared} times for {count} names",
        )

    def test_values_and_a_commit_pickle_each_field_once(self):
        self.moved()
        pickled = []
        dump = registry_rows._Pickler.dump

        def counted(pickler, obj):
            pickled.append(obj)
            return dump(pickler, obj)

        with mock.patch.object(registry_rows._Pickler, "dump", counted):
            records = self.registry.records.values()
            transaction.commit()
        self.assertTrue(records)
        fields = [obj for obj in pickled if not isinstance(obj, str)]
        times = collections.Counter(map(id, fields))
        twice = sum(1 for count in times.values() if count > 1)
        self.assertEqual(
            twice, 0, f"{twice} of {len(times)} fields pickled more than once"
        )
