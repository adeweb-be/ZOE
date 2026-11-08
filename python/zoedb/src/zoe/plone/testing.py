"""A plone.testing layer that puts a ZOE storage under the Plone fixtures.

The `Startup` layer of plone.testing (`plone.testing.zope.STARTUP`) opens a
`DemoStorage` without a base, so its base is a `MappingStorage`. The layers
of Plone stack their own `DemoStorage` on top of it. The layer of this
module gives that `DemoStorage` a ZOE storage as its base. The rest of the
stack does not change, so the suites run unchanged.

- `install()` uses a `zoe.Storage` in a new directory.
- `install(zeo="host:port")` uses a `ZEO.ClientStorage` of the `zoe serve`
  at that address.

`ZOE_PLONE_BACKENDS` names the backends of `zoe.plone` that the suites run
on, separated by commas: `locking`, `indexing`, `registry` or `security`. `install`
installs them, the guard of a stock registry (ADR 0050) and the patch of
the mutable property sheet of PlonePAS (ADR 0046, section 9), which an
instance installs through `configure.zcml`.
The suites of Plone run without `indexing`: they search the catalog right
after a change, in the same transaction. The tests of queued indexing
install it themselves.
With `security`, the site of the fixture of plone.app.testing gets the
`ZoeSecurityIndex` in the transaction that creates it (ADR 0061); without
it, the suites keep the stock index and run the stock methods of CMFCore.
`just plone zoe backends` keeps `locking`, so only the tests of
`zoe.plone.tests.test_security_index` run with the index: they install the
backend and put the index into their own sites.

Call `install` before zope.testrunner sets up the layers, for example in
the `initialization` of the test script. It makes the layer a base of
`STARTUP`. zope.testrunner then sets up the layer before `STARTUP` and
tears it down after `STARTUP`. Only the `plone` extra and plone.testing
make this module importable.

`ZOE_SITE` is a layer over the fixture of plone.app.testing whose database
is a `zoe.Storage` itself, with no `DemoStorage` on it: a Plone site
`plone` with plone.app.contenttypes, in a store of its own. The tests that
need the catalog mirror of the store use it. Its resources are `zoeDB`,
the database, and `zoeStorage`, its storage. The module makes the layer at
its first use, so it imports plone.app.testing only then.

`ZOE_SERVE_SITE` is the same site in a database whose storage is a
`zoe.zeo.ClientStorage` with the native client inside, of a `zoe serve`
on a store of its own, with no `DemoStorage` on it: the server holds the
catalog mirror. Its resources are `zoeDB`, `zoeStorage` and `zoeServer`,
the `zoe.testing.ZoeServer`. The server is the `zoe` binary that
`ZOE_BINARY` names.
"""

import shutil
import tempfile

import transaction
from plone.testing import Layer, zodb
from plone.testing import zope as zope_layers
from ZODB.DB import DB
from ZODB.DemoStorage import DemoStorage


class BaseStorage(Layer):
    """Open a base storage, and make it the base of the `DemoStorage` of
    `STARTUP`.

    `setUp` opens the storage in a new directory. While the layer is set up,
    `plone.testing.zodb.stackDemoStorage` without a database stacks a
    `DemoStorage` on this storage. With a database, it does not change.
    `tearDown` closes the storage and removes the directory.
    """

    def open(self, directory):
        """Open the base storage in `directory`."""
        raise NotImplementedError

    def setUp(self):
        self._directory = tempfile.mkdtemp(prefix="zoe-plone-")
        self._storage = self.open(self._directory)
        self._stack = zodb.stackDemoStorage
        zodb.stackDemoStorage = self.stackDemoStorage

    def tearDown(self):
        zodb.stackDemoStorage = self._stack
        self._storage.close()
        shutil.rmtree(self._directory, ignore_errors=True)
        del self._storage, self._stack, self._directory

    def stackDemoStorage(self, db=None, name=None):
        """Return a database on a new `DemoStorage` named `name`.

        Without `db`, the base is the storage of the layer. When the
        `DemoStorage` closes, the base stays open: the layers above close
        their storages many times. With `db`, the base is the storage of
        `db`, as plone.testing does.
        """
        if db is not None:
            return self._stack(db, name)
        storage = DemoStorage(name=name, base=self._storage, close_base_on_close=False)
        return DB(storage)


class ZoeStorage(BaseStorage):
    """The base storage is a `zoe.Storage` in a new directory."""

    def open(self, directory):
        import zoe

        return zoe.Storage(f"{directory}/zoe")


class ZoeServe(BaseStorage):
    """The base storage is a `ZEO.ClientStorage` of `zoe serve`.

    `address` is the address of the server, as `host:port`. The client keeps
    its blob cache in the directory of the layer. zoe serve speaks the
    msgpack protocol of ZEO, so the client needs msgpack.
    """

    address = None

    def open(self, directory):
        from ZEO.ClientStorage import ClientStorage

        host, port = self.address.rsplit(":", 1)
        return ClientStorage(
            (host, int(port)),
            blob_dir=f"{directory}/blobs",
            shared_blob_dir=False,
        )


ZOE_STORAGE = ZoeStorage()
ZOE_SERVE = ZoeServe()


def install(zeo=None):
    """Make a ZOE storage the base storage of the Plone fixtures.

    Without `zeo`, the base is a `zoe.Storage` (`ZOE_STORAGE`). With `zeo`,
    as `host:port`, it is a ZEO client of the `zoe serve` at that address
    (`ZOE_SERVE`). Return the layer.
    """
    from zoe.plone import backends_from_environment, properties, registry

    backends_from_environment()
    # plone.app.testing loads no ZCML of zoe.plone, so the layer installs
    # the guard of a stock registry and the patch of the mutable property
    # sheet, as configure.zcml does in an instance.
    registry.install_guard()
    properties.install()
    if zeo is None:
        layer = ZOE_STORAGE
    else:
        layer = ZOE_SERVE
        layer.address = zeo
    startup = zope_layers.STARTUP
    if layer not in startup.__bases__:
        startup.__bases__ = (layer, *startup.__bases__)
    return layer


class ZoeSite(Layer):
    """A Plone site in a database whose storage is a `zoe.Storage`, with no
    `DemoStorage` on it.

    `setUp` opens the storage in a new directory, adds a Zope application
    with the site owner of plone.app.testing, and a Plone site `plone` with
    the profile of plone.app.contenttypes. `tearDown` closes the database
    and removes the directory.
    """

    def open(self, directory):
        """Open the storage of the site in `directory`."""
        import zoe

        return zoe.Storage(f"{directory}/zoe")

    def setUp(self):
        from OFS.Application import Application
        from plone.app.testing import SITE_OWNER_NAME, SITE_OWNER_PASSWORD
        from plone.testing import zope as zope_testing
        from Products.CMFPlone.factory import addPloneSite
        from Testing.makerequest import makerequest
        from zope.component.hooks import getSite, setSite

        self._directory = tempfile.mkdtemp(prefix="zoe-plone-site-")
        storage = self.open(self._directory)
        db = DB(storage)
        site = getSite()
        connection = db.open()
        try:
            app = Application()
            connection.root()["Application"] = app
            app.acl_users._doAddUser(
                SITE_OWNER_NAME, SITE_OWNER_PASSWORD, ["Manager"], []
            )
            transaction.commit()
            app = makerequest(connection.root()["Application"])
            zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
            setSite(None)
            addPloneSite(
                app,
                "plone",
                setup_content=False,
                extension_ids=("plone.app.contenttypes:default",),
            )
            transaction.commit()
        finally:
            transaction.abort()
            zope_testing.logout()
            setSite(site)
            connection.close()
        self["zoeDB"] = db
        self["zoeStorage"] = storage

    def tearDown(self):
        self["zoeDB"].close()
        del self["zoeDB"], self["zoeStorage"]
        shutil.rmtree(self._directory, ignore_errors=True)


class ZoeServeSite(ZoeSite):
    """The Plone site of `ZoeSite` in a database whose storage is a
    `zoe.zeo.ClientStorage` with the native client inside, of a `zoe serve`
    on a new store, with no `DemoStorage` on it.

    `setUp` starts the server, then makes the site. `tearDown` closes the
    database, then stops the server.
    """

    def open(self, directory):
        import zoe.zeo
        from zoe.testing import ZoeServer

        server = ZoeServer(f"{directory}/zoe").start(60)
        self["zoeServer"] = server
        return zoe.zeo.ClientStorage(
            server.address,
            native=True,
            blob_dir=f"{directory}/blobs",
            shared_blob_dir=False,
            wait_timeout=60,
        )

    def tearDown(self):
        self["zoeDB"].close()
        server = self["zoeServer"]
        del self["zoeDB"], self["zoeStorage"], self["zoeServer"]
        server.stop()
        shutil.rmtree(self._directory, ignore_errors=True)


_layers = {}


def __getattr__(name):
    # The layer depends on plone.app.testing, which the module imports only
    # when a test asks for it.
    made = {
        "ZOE_SITE": (ZoeSite, "ZoeSite"),
        "ZOE_SERVE_SITE": (ZoeServeSite, "ZoeServeSite"),
    }
    if name in made:
        if name not in _layers:
            from plone.app.testing import PLONE_FIXTURE

            factory, layer = made[name]
            _layers[name] = factory(bases=(PLONE_FIXTURE,), name=layer)
        return _layers[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
