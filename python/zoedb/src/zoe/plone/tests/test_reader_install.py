"""Acceptance test of the reader of ZOE in a Zope start with `zoe.plone`
(ADR 0057).

The test starts Zope in a child interpreter, as a Plone instance starts:
a `zope.conf` with a `<zoe>` storage, and the `site.zcml` of an instance,
which loads Plone and, through plone.autoinclude, the ZCML of `zoe.plone`.
At the end of the start, Zope sends `IDatabaseOpenedWithRoot`.

- With the default of `native-reader`, every connection of the main
  database has the reader: the connection that the pool holds from the
  start, and a new connection that `db.open()` makes.
- With `native-reader off`, no connection has the reader.

The layers of plone.app.testing load no ZCML through plone.autoinclude, so
the test starts Zope itself. It needs the interpreter of a Plone instance:
`bin/test -s zoe.plone` of the instance of the zoe scenario runs it, as
`just plone zoe backends -t test_reader_install` does.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest

# The longest time of one Zope start, in seconds.
TIMEOUT = 600

# The site.zcml that plone.recipe.zope2instance writes for an instance.
SITE_ZCML = """\
<configure
    xmlns="http://namespaces.zope.org/zope"
    xmlns:meta="http://namespaces.zope.org/meta"
    xmlns:five="http://namespaces.zope.org/five">

  <include package="Products.Five" />
  <meta:redefinePermission from="zope2.Public" to="zope.Public" />

  <include files="package-includes/*-meta.zcml" />
  <five:loadProducts file="meta.zcml"/>

  <include files="package-includes/*-configure.zcml" />
  <five:loadProducts />

  <includeOverrides files="package-includes/*-overrides.zcml" />
  <five:loadProductsOverrides />

  <securityPolicy
      component="AccessControl.security.SecurityPolicy" />

</configure>
"""

ZOPE_CONF = """\
%import zoe
instancehome {instance}
<zodb_db main>
    <zoe>
      path {store}
{keys}    </zoe>
    mount-point /
</zodb_db>
"""


def start(conf):
    """Start Zope with the configuration file `conf`. Print, as JSON, the
    readers of the connections of its main database: each connection of
    the pool after the start, and two connections that `db.open()` gives
    then, the first from the pool and the second new."""
    from Zope2.Startup.run import configure_wsgi

    configure_wsgi(conf)
    import Zope2

    Zope2.startup_wsgi()
    import zoe.reader

    db = Zope2.DB
    pooled = list(db.pool)
    first = db.open()
    second = db.open()
    result = {
        "pooled": [isinstance(conn._reader, zoe.reader.Reader) for conn in pooled],
        "first_is_pooled": any(first is conn for conn in pooled),
        "first": isinstance(first._reader, zoe.reader.Reader),
        "second_is_pooled": any(second is conn for conn in pooled),
        "second": isinstance(second._reader, zoe.reader.Reader),
        "second_class": f"{type(second).__module__}.{type(second).__qualname__}",
    }
    second.close()
    first.close()
    db.close()
    print(json.dumps(result))


class TestReaderInstall(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.mkdtemp(prefix="zoe-reader-")
        self.addCleanup(shutil.rmtree, self.directory, True)

    def start_zope(self, *keys):
        """Start Zope on a new store with the keys `keys` of `<zoe>` in a
        child interpreter. Return the readers that the child prints."""
        instance = os.path.join(self.directory, "instance")
        os.makedirs(os.path.join(instance, "etc"))
        with open(os.path.join(instance, "etc", "site.zcml"), "w") as file:
            file.write(SITE_ZCML)
        conf = os.path.join(self.directory, "zope.conf")
        with open(conf, "w") as file:
            file.write(
                ZOPE_CONF.format(
                    instance=instance,
                    store=os.path.join(self.directory, "store"),
                    keys="".join(f"      {key}\n" for key in keys),
                )
            )
        # The child finds the eggs of the instance on the path of this
        # process. No ZOE_* variable of the caller reaches it.
        env = {k: v for k, v in os.environ.items() if not k.startswith("ZOE_")}
        env["PYTHONPATH"] = os.pathsep.join(sys.path)
        code = f"from {__name__} import start; start({conf!r})"
        result = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            env=env,
            cwd=self.directory,
            timeout=TIMEOUT,
        )
        self.assertEqual(result.returncode, 0, result.stderr[-4000:])
        return json.loads(result.stdout.strip().splitlines()[-1])

    def test_every_connection_has_the_reader_by_default(self):
        found = self.start_zope()
        self.assertTrue(found["pooled"])
        self.assertTrue(all(found["pooled"]), found)
        self.assertTrue(found["first_is_pooled"], found)
        self.assertTrue(found["first"], found)
        self.assertFalse(found["second_is_pooled"], found)
        self.assertTrue(found["second"], found)
        self.assertEqual(found["second_class"], "zoe.reader.Connection")

    def test_no_connection_has_the_reader_with_native_reader_off(self):
        found = self.start_zope("native-reader off")
        self.assertTrue(found["pooled"])
        self.assertFalse(any(found["pooled"]), found)
        self.assertTrue(found["first_is_pooled"], found)
        self.assertFalse(found["first"], found)
        self.assertFalse(found["second_is_pooled"], found)
        self.assertFalse(found["second"], found)
        self.assertEqual(found["second_class"], "ZODB.Connection.Connection")
