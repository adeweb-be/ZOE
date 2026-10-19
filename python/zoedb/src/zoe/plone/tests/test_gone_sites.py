"""Unit tests of `zoe.plone.merges.gone_sites`, on stand-ins for the
objects of Zope.

A path is gone only when its walk from the Zope root stops at an object of
the database of the site that holds no item of the next id. The walk keeps
a path that it cannot tell about: a path that stops in another database, as
below a mount point, a path through an object that is not a container, a
walk that fails, a path that is not absolute, and a missing item of a
container with a tree of items (a BTreeFolder2) whose tree has a record that
does not load. A `ConflictError` goes to the caller.

`just plone zoe backends -t test_gone_sites` runs this module.
"""

import unittest

import transaction
import ZODB
from BTrees.OOBTree import OOBTree
from ZODB.MappingStorage import MappingStorage
from ZODB.POSException import ConflictError, POSKeyError

from zoe.plone import merges


class Database:
    pass


class Jar:
    def __init__(self, database):
        self.database = database

    def db(self):
        return self.database


class Container:
    """An object with items, in the database of `jar`."""

    def __init__(self, jar, **items):
        self._p_jar = jar
        self.items = items

    def _getOb(self, name, default):
        return self.items.get(name, default)


class Site(Container):
    def __init__(self, jar, top):
        super().__init__(jar)
        self.top = top

    def getPhysicalRoot(self):
        return self.top


class Broken:
    def _getOb(self, name, default):
        raise RuntimeError("the walk fails")


class Conflicting:
    def _getOb(self, name, default):
        raise ConflictError("a conflict")


class Tree(Container):
    """A container with a tree of items, whose `_getOb` reads a record of
    the tree that does not load as a missing item, as BTreeFolder2 does."""

    def __init__(self, jar, tree):
        super().__init__(jar)
        self._tree = tree

    def _getOb(self, name, default):
        try:
            return self._tree[name]
        except KeyError:
            return default


class Unloadable(MappingStorage):
    """A storage on which the records of `missing` do not load."""

    missing = frozenset()

    def load(self, oid, version=""):
        if oid in self.missing:
            raise POSKeyError(oid)
        return super().load(oid, version)

    def loadBefore(self, oid, tid):
        if oid in self.missing:
            raise POSKeyError(oid)
        return super().loadBefore(oid, tid)


def unloadable_tree(test):
    """Return a tree of a database whose records, but the record of the
    tree, do not load."""
    storage = Unloadable()
    database = ZODB.DB(storage)
    test.addCleanup(database.close)
    connection = database.open()
    test.addCleanup(connection.close)
    tree = OOBTree()
    for index in range(1000):
        tree[f"item{index:04d}"] = index
    connection.root()["tree"] = tree
    transaction.commit()
    oids = set()
    bucket = tree._firstbucket
    while bucket is not None:
        oids.add(bucket._p_oid)
        bucket = bucket._next
    connection.cacheMinimize()
    storage.missing = frozenset(oids)
    return connection.root()["tree"]


class TestGoneSites(unittest.TestCase):
    def setUp(self):
        self.main = Jar(Database())
        self.mounted = Jar(Database())
        self.top = Container(self.main)
        self.site = Site(self.main, self.top)
        self.top.items.update(
            plone=self.site,
            leaf=object(),
            broken=Broken(),
            mnt=Container(self.mounted),
        )
        self.other = Site(self.mounted, self.top)
        self.top.items["mnt"].items["other"] = self.other
        self.modes = {
            "/plone": "on",
            "/gone": "off",
            "/mnt/other": "off",
            "/mnt/gone": "off",
            "/leaf/site": "off",
            "/broken/site": "off",
            "relative": "off",
        }

    def test_a_site_of_the_root_database_drops_only_its_gone_paths(self):
        self.assertEqual(merges.gone_sites(self.site, self.modes, "/plone"), ["/gone"])

    def test_a_site_below_a_mount_point_drops_only_the_gone_paths_there(self):
        self.assertEqual(
            merges.gone_sites(self.other, self.modes, "/mnt/other"), ["/mnt/gone"]
        )

    def test_a_site_without_a_zope_root_drops_nothing(self):
        class Unwrapped(Container):
            pass

        unwrapped = Unwrapped(self.main)
        self.assertEqual(merges.gone_sites(unwrapped, self.modes, "plone"), [])

    def test_a_tree_whose_record_does_not_load_keeps_the_path(self):
        self.top.items["bt"] = Tree(self.main, unloadable_tree(self))
        modes = {"/plone": "on", "/bt/live": "off"}
        self.assertEqual(merges.gone_sites(self.site, modes, "/plone"), [])

    def test_a_tree_without_the_item_drops_the_path(self):
        self.top.items["bt"] = Tree(self.main, OOBTree({"live": self.site}))
        modes = {"/plone": "on", "/bt/live": "off", "/bt/gone": "off"}
        self.assertEqual(merges.gone_sites(self.site, modes, "/plone"), ["/bt/gone"])

    def test_a_conflict_goes_to_the_caller(self):
        self.top.items["conflicting"] = Conflicting()
        modes = {"/plone": "on", "/conflicting/site": "off"}
        with self.assertRaises(ConflictError):
            merges.gone_sites(self.site, modes, "/plone")
