"""Unit tests of `zoe.plone.merges.gone_sites`, on stand-ins for the
objects of Zope.

A path is gone only when its walk from the Zope root stops at an object of
the database of the site that holds no item of the next id. The walk keeps
a path that it cannot tell about: a path that stops in another database, as
below a mount point, a path through an object that is not a container, a
walk that fails, and a path that is not absolute.

`just plone zoe backends -t test_gone_sites` runs this module.
"""

import unittest

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
