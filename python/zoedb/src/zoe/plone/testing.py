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
on, separated by commas: `locking` or `indexing`. `install` installs them.
The suites of Plone run without `indexing`: they search the catalog right
after a change, in the same transaction. The tests of queued indexing
install it themselves.

Call `install` before zope.testrunner sets up the layers, for example in
the `initialization` of the test script. It makes the layer a base of
`STARTUP`. zope.testrunner then sets up the layer before `STARTUP` and
tears it down after `STARTUP`. Only the `plone` extra and plone.testing
make this module importable.
"""

import shutil
import tempfile

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
    from zoe.plone import backends_from_environment

    backends_from_environment()
    if zeo is None:
        layer = ZOE_STORAGE
    else:
        layer = ZOE_SERVE
        layer.address = zeo
    startup = zope_layers.STARTUP
    if layer not in startup.__bases__:
        startup.__bases__ = (layer, *startup.__bases__)
    return layer
