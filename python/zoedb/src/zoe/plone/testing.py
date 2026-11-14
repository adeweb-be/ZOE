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
`zoe.plone.tests.test_security_index` and `test_security_backend` run with
the index: they install the backend and put the index into their own
sites. `ZOE_PLONE_BACKENDS=security just plone zoe suites -s zoe.plone -t
TestTheFixtureOfTheSuites` checks that the site of the fixture holds it.

`ZOE_PLONE_TEXT=compat` (or `install(text="compat")`) replaces the
ZCTextIndex indexes `SearchableText`, `Title` and `Description` of each
Plone site that `Products.CMFPlone.factory.addPloneSite` creates with
`ZoeTextIndex` indexes in the mode `compat` (ADR 0060, section 11), with
`install_text` of `zoe.plone.textindex`, in the transaction that creates
the site. It needs the backend `catalog` in `ZOE_PLONE_BACKENDS`. On the
`DemoStorage` of a layer, each text query then sends the texts of the
stack to the ZOE storage below, which answers from them alone. `just
plone zoe catalog text` runs the suites so. With it, `ZOE_PLONE_TEXT_LOG`
names a directory where each process writes what the replay of the text
queries reads (`plonebench.catalog.text_replay`): the texts that each
`ZoeTextIndex` stages, in `texts-<pid>.jsonl`, and the query log of the
probe `plonebench_catalog` of the harness, in `catalog-<pid>.jsonl`. The
probe must be on the path of the process.

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

`ZOE_TEXT_SITE` is the site of `ZOE_SITE` in a store of its own, with the
backend `catalog` installed and the profile `zoe.plone:text` imported:
`SearchableText`, `Title` and `Description` are `ZoeTextIndex` indexes
(ADR 0060). The layer loads `text.zcml` of `zoe.plone` in a stacked
registry, so the profile is there. The mirror window of its store holds
`TEXT_WINDOW_TIDS` tids, so a test reaches a snapshot outside the window
with a few commits. Its resources are those of `ZOE_SITE`. `tearDown`
uninstalls the backend when `setUp` installed it.

`ZOE_SERVE_SITE` is the same site in a database whose storage is a
`zoe.zeo.ClientStorage` with the native client inside, of a `zoe serve`
on a store of its own, with no `DemoStorage` on it: the server holds the
catalog mirror. Its resources are `zoeDB`, `zoeStorage` and `zoeServer`,
the `zoe.testing.ZoeServer`. The server is the `zoe` binary that
`ZOE_BINARY` names.
"""

import json
import os
import shutil
import tempfile
import threading

import transaction
from plone.testing import Layer, zca, zodb
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

    The layer comes after `LAYER_CLEANUP` of plone.testing, which clears
    the global registry. So `setUp` registers the handlers of the security
    backend again when `install` installed it, and `tearDown` removes them.
    """

    defaultBases = (zca.LAYER_CLEANUP,)

    def open(self, directory):
        """Open the base storage in `directory`."""
        raise NotImplementedError

    def setUp(self):
        from zope.component import getGlobalSiteManager

        from zoe.plone import security

        self._handlers = security._handlers() if security.installed() else []
        registry = getGlobalSiteManager()
        for handler, required in self._handlers:
            registry.registerHandler(handler, required)
        self._directory = tempfile.mkdtemp(prefix="zoe-plone-")
        self._storage = self.open(self._directory)
        self._stack = zodb.stackDemoStorage
        zodb.stackDemoStorage = self.stackDemoStorage

    def tearDown(self):
        from zope.component import getGlobalSiteManager

        registry = getGlobalSiteManager()
        for handler, required in self._handlers:
            registry.unregisterHandler(handler, required)
        del self._handlers
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


#: The variable of the mode of the text indexes of the sites of the suites.
TEXT = "ZOE_PLONE_TEXT"

#: The variable of the directory of the documents and the searches of the
#: suites, for the replay of the text queries.
TEXT_LOG = "ZOE_PLONE_TEXT_LOG"


def install(zeo=None, text=None):
    """Make a ZOE storage the base storage of the Plone fixtures.

    Without `zeo`, the base is a `zoe.Storage` (`ZOE_STORAGE`). With `zeo`,
    as `host:port`, it is a ZEO client of the `zoe serve` at that address
    (`ZOE_SERVE`). `text`, else `ZOE_PLONE_TEXT`, names the mode of the
    `ZoeTextIndex` indexes that replace the ZCTextIndex indexes of each
    new Plone site: `compat`. Return the layer.
    """
    from zoe.plone import backends_from_environment, properties, registry

    backends_from_environment()
    text = text or os.environ.get(TEXT, "").strip() or None
    if text is not None:
        replace_text_indexes(text)
    log = os.environ.get(TEXT_LOG, "").strip()
    if log:
        record_texts(log)
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


def replace_text_indexes(mode):
    """Make each Plone site that `addPloneSite` creates hold `ZoeTextIndex`
    indexes in the mode `mode` in the place of its ZCTextIndex indexes,
    with `install_text`, in the transaction that creates the site. A
    second call changes only the mode."""
    from Products.CMFPlone import factory

    from zoe.plone import catalog, textindex

    if not catalog.installed():
        raise RuntimeError(
            f"{TEXT} needs the backend catalog: add catalog to ZOE_PLONE_BACKENDS"
        )
    textindex._mode(mode)
    original = getattr(factory.addPloneSite, "_zoe_original", factory.addPloneSite)

    def addPloneSite(*args, **kwargs):
        site = original(*args, **kwargs)
        # The new objects of the site get their database, so the indexes
        # can stage their texts there.
        transaction.savepoint(optimistic=True)
        textindex.install_text(site.portal_setup, mode=mode)
        _register_query_parser()
        return site

    addPloneSite._zoe_original = original
    factory.addPloneSite = addPloneSite


def _register_query_parser():
    """Register the query parser of plone.restapi for `ZoeTextIndex`, as
    `text.zcml` does. The layers load no ZCML of zoe.plone, and the
    cleanup layer of plone.testing clears the global registry. So the
    registration goes into the registry that the layer which creates the
    site pushed. Do nothing without plone.restapi."""
    try:
        from plone.restapi.search.query import ZCTextIndexQueryParser
    except ImportError:
        return
    from zope.component import provideAdapter
    from zope.interface import Interface

    from zoe.plone.textindex import ZoeTextIndex

    provideAdapter(ZCTextIndexQueryParser, (ZoeTextIndex, Interface, Interface))


_text_log = {}


def record_texts(directory):
    """Write the texts that each `ZoeTextIndex` stages, and the query log of
    the probe `plonebench_catalog`, under `directory`, for the replay of
    the text queries. Each text is one JSON line `{"i": index, "t": text}`.
    A second call does nothing."""
    from zoe.plone import textindex

    if _text_log:
        return
    os.makedirs(directory, exist_ok=True)
    pid = os.getpid()
    try:
        import plonebench_catalog
    except ImportError as e:
        raise RuntimeError(
            f"{TEXT_LOG} needs the probe plonebench_catalog on the path"
        ) from e
    plonebench_catalog.install(os.path.join(directory, f"catalog-{pid}.jsonl"))
    out = open(os.path.join(directory, f"texts-{pid}.jsonl"), "a", buffering=1)
    lock = threading.Lock()
    stage = textindex._Deltas.set

    def staging(self, catalog, index, rid, language, text):
        if text is not None:
            line = json.dumps({"i": index, "t": text}, ensure_ascii=False)
            with lock:
                out.write(line + "\n")
        return stage(self, catalog, index, rid, language, text)

    textindex._Deltas.set = staging
    _text_log["out"] = out


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


#: The tids of the mirror window of the store of `ZOE_TEXT_SITE`.
TEXT_WINDOW_TIDS = 4


class ZoeTextSite(ZoeSite):
    """The site of `ZoeSite` with the backend `catalog` and the profile
    `zoe.plone:text`."""

    def open(self, directory):
        import zoe

        return zoe.Storage(f"{directory}/zoe", catalog_window_tids=TEXT_WINDOW_TIDS)

    def setUp(self):
        from plone.app.testing import SITE_OWNER_NAME
        from plone.testing import zope as zope_testing
        from Products.GenericSetup.registry import _profile_registry
        from Testing.makerequest import makerequest
        from zope.component.hooks import getSite, setSite
        from zope.configuration import xmlconfig

        import zoe.plone
        from zoe.plone import catalog

        self["configurationContext"] = zca.stackConfigurationContext(
            self.get("configurationContext"), name="ZoeTextSite"
        )
        zca.pushGlobalRegistry()
        known = set(_profile_registry.listProfiles())
        xmlconfig.file("text.zcml", zoe.plone, context=self["configurationContext"])
        self._profiles = set(_profile_registry.listProfiles()) - known
        self._backend = not catalog.installed()
        catalog.install()
        super().setUp()
        db = self["zoeDB"]
        self._root = catalog._root.get("db")
        catalog._root["db"] = db
        catalog.register(db)
        site = getSite()
        connection = db.open()
        try:
            app = makerequest(connection.root()["Application"])
            zope_testing.login(app["acl_users"], SITE_OWNER_NAME)
            setSite(app["plone"])
            app["plone"].portal_setup.runAllImportStepsFromProfile(
                "profile-zoe.plone:text"
            )
            transaction.commit()
        finally:
            transaction.abort()
            zope_testing.logout()
            setSite(site)
            connection.close()

    def tearDown(self):
        from Products.GenericSetup.registry import _profile_registry

        from zoe.plone import catalog

        super().tearDown()
        if self._root is None:
            catalog._root.pop("db", None)
        else:
            catalog._root["db"] = self._root
        if self._backend:
            catalog.uninstall()
        for profile in self._profiles:
            product, name = profile.split(":", 1)
            _profile_registry.unregisterProfile(name, product)
        zca.popGlobalRegistry()
        del self["configurationContext"]
        del self._backend, self._root, self._profiles


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
        "ZOE_TEXT_SITE": (ZoeTextSite, "ZoeTextSite"),
    }
    if name in made:
        if name not in _layers:
            from plone.app.testing import PLONE_FIXTURE

            factory, layer = made[name]
            _layers[name] = factory(bases=(PLONE_FIXTURE,), name=layer)
        return _layers[name]
    raise AttributeError(f"module {__name__!r} has no attribute {name!r}")
