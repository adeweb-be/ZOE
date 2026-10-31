"""The native client inside `zoe.zeo.ClientStorage`.

`_NativeClientStorage` runs on `zoe._zoe.Client`, the binding of the Rust
crate `zoe-client`, and not on the asyncio client of ZEO. It is still an
instance of `ZEO.ClientStorage.ClientStorage`, and no thread of ZEO starts.
The package `zoe.zeo` tells what it does.
"""

import concurrent.futures
import contextvars
import logging
import os
import threading
import time
import weakref

import ZEO.interfaces
import ZODB.blob
import ZODB.event
import ZODB.interfaces
import zope.interface
from ZEO.ClientStorage import BlobCacheLayout, _lock_blob
from ZEO.ClientStorage import ClientStorage as ZEOClientStorage
from ZEO.Exceptions import ClientDisconnected
from ZODB.POSException import (
    POSKeyError,
    ReadOnlyError,
    StorageTransactionError,
    Unsupported,
)
from ZODB.utils import z64

from zoe import zodb
from zoe.zeo._asyncio import (
    _KV,
    _LOCK_INTENT,
    EXTENSIONS,
    ClientStorage,
    _traceparent,
)

# The native client logs under the name of the package, as the tests
# and the filters of the logs expect.
logger = logging.getLogger("zoe.zeo")

# The native client logs the events of its connection under the logger of
# `ZEO.ClientStorage`, with the words of ZEO, so a filter of the logs of
# ZEO keeps them.
_zeo_logger = logging.getLogger("ZEO.ClientStorage")

# The size of the cache of ZEO when the caller gives none.
_MB = 1024**2

# The extensions that the native client declares: those of the client of
# ZEO, and `server_writes` (ADR 0049). With it, the vote of the server can
# write a record that the transaction did not store, and the finish gives
# every Connection of the process every oid of the commit. The asyncio
# client does not keep that promise, so it does not declare it.
_SERVER_WRITES = "server_writes"
_NATIVE_EXTENSIONS = (*EXTENSIONS, _SERVER_WRITES)

# The protocols of a server for which a `ClientStorage` without `native`
# takes the asyncio client of ZEO inside: the pickle protocol of a stock
# ZEO server without `msgpack true`. The client of ZEO speaks it.
_ZEO_PROTOCOLS = (b"Z5",)

# The number of oids that the native client asks for at a time.
_NEW_OIDS = 100

# The seconds between two attempts to connect again, when the caller gives
# no `disconnect_poll`: the `connect_poll` of the client of ZEO.
_DISCONNECT_POLL = 1

# The longest time that `close` waits for the thread of the invalidations.
_JOIN_TIMEOUT = 30

# The interfaces of the server that `ZEO.ClientStorage` declares too.
_SERVER_INTERFACES = (
    ZODB.interfaces.IStorageRestoreable,
    ZODB.interfaces.IStorageIteration,
    ZODB.interfaces.IStorageUndoable,
    ZODB.interfaces.IStorageCurrentRecordIteration,
    ZODB.interfaces.IBlobStorage,
    ZODB.interfaces.IExternalGC,
)

# The most connections of the native client that commit at once. A
# transaction that finds them all busy waits for a free one. The listener
# connection comes on top. The threads cells of zodbshootout run 64 threads.
_POOL_BOUND = 64


def _as_bytes(value):
    """Return the bytes of a user name or a description."""
    return value.encode("utf-8") if isinstance(value, str) else bytes(value)


def _native_tls(section, server_hostname):
    """Return the TLS of the native client from an `<ssl>` section.

    The tuple holds the certificate or None, the key or None, the CA file
    or directory of `authenticate` or None, `check-hostname`, and the
    server name: `server_hostname`, else the `server-hostname` of the
    section, else None. As in `ZEO.zconfig.ssl_config`, no `certificate`
    gives a client without a certificate, which ignores `key` and
    `password-function`. No `authenticate` and `DYNAMIC` that is not a
    directory give None: the client then checks the server against the CA
    certificates of the system. Raise `Unsupported` for an encrypted key,
    which the native client does not read.
    """
    certificate = key = None
    if section.certificate:
        if section.password_function:
            raise Unsupported(
                "the native client reads only a key that is not encrypted: "
                "leave password-function out of <ssl>"
            )
        certificate = os.path.abspath(section.certificate)
        key = os.path.abspath(section.key) if section.key else None
    auth = section.authenticate
    if auth == "DYNAMIC" and not os.path.isdir(auth):
        auth = None
    return (
        certificate,
        key,
        os.path.abspath(auth) if auth else None,
        bool(section.check_hostname),
        server_hostname or section.server_hostname or None,
    )


# The calls on the iterators of the server. The first arg of a call, or
# the reply of a start, is an iterator id.
_ITERATOR_CALLS = frozenset(
    {"iterator_start", "iterator_next", "iterator_record_start", "iterator_record_next"}
)
_ITERATOR_STARTS = frozenset({"iterator_start", "iterator_record_start"})


class _Iid(int):
    """An iterator id of the server, with the generation of the listener
    that gave it.

    The server of each listener numbers the iterators of its connection
    from 0. So two ids are equal only with the same generation, and a call
    with the id goes only to the listener of its generation.
    """

    def __new__(cls, iid, generation):
        self = super().__new__(cls, iid)
        self.generation = generation
        return self

    def __eq__(self, other):
        return isinstance(other, _Iid) and (int(self), self.generation) == (
            int(other),
            other.generation,
        )

    def __ne__(self, other):
        return not self == other

    def __hash__(self):
        return hash((int(self), self.generation))


def _serial(serial):
    """Return the bytes of the serial of a store.

    ZEO's tests give the serial of a new object as the str `'\\0' * 8`, and
    ZEO's server takes the UTF-8 bytes of a str. No serial is the zero tid.
    """
    if not serial:
        return z64
    if isinstance(serial, str):
        return serial.encode("utf-8")
    return serial


def _remove_files(blobs):
    """Remove the file of each (oid, file) of `blobs` that exists."""
    for _, filename in blobs:
        try:
            os.remove(filename)
        except FileNotFoundError:
            pass


def _native_cache_path(cache, var, client, storage):
    """Return the path of the cache file of the native client, or None.

    With `client`, the name follows the name of ZEO, `<client>-<storage>`
    in `var` (the current directory by default), with the extension `.zoec`
    in place of `.zec`: the native client never reads or writes the cache
    file of ZEO. A str `cache` is the path of the file of ZEO: its `.zec`
    becomes `.zoec`, and a path without `.zec` or `.zoec` gets `.zoec`.
    Without `client` and `cache`, the cache stays in memory. A cache object
    raises `Unsupported`: the cache of the native client is in Rust.
    """
    if isinstance(cache, str):
        if cache.endswith(".zoec"):
            return cache
        if cache.endswith(".zec"):
            return cache[: -len(".zec")] + ".zoec"
        return cache + ".zoec"
    if cache is not None:
        raise Unsupported(
            "the native client takes the path of a cache file as cache, "
            "not a cache object"
        )
    if client:
        return os.path.join(var or os.getcwd(), f"{client}-{storage}.zoec")
    return None


def _native_address(addr):
    """Return an address for the native client.

    A `(host, port)` address with an empty host is on `127.0.0.1`, as for
    ZEO. The path of a unix domain socket stays as it is.
    """
    if isinstance(addr, tuple) and len(addr) == 2:
        host, port = addr
        return (host or "127.0.0.1", port)
    return addr


class _NativeCache:
    """The `_cache` of the native client: its client cache, in Rust.

    `maxsize`, `path`, `len()`, `contents()` and `clear()` give what those
    of `ZEO.cache.ClientCache` give: the size of the cache, the path of its
    file or None, the number of its entries, the oid and the tid of each
    current entry, and the drop of every entry.
    """

    def __init__(self, native, maxsize, path):
        self._native = native
        self.maxsize = maxsize
        self.path = path

    def __len__(self):
        return len(self._native.cache_entries())

    def contents(self):
        """Yield the oid and the start tid of each current entry."""
        for oid, start, end in self._native.cache_entries():
            if end is None:
                yield oid, start

    def clear(self):
        """Drop every entry of the cache. zodbshootout calls it."""
        self._native.cache_clear()


# The words of ZEO for how the verification of the cache ended, by the
# kinds of the native client.
_VERIFY_RESULTS = {
    "kept": "Cache up to date",
    "quick": "quick verification",
    "dropped": "cache too old, clearing",
    "failed": "cache too old, clearing",
    "ahead": "Cache newer than server",
}


class _NativeConnection:
    """The connection of the native client to its server, as
    `notify_connected` gets it, in place of the `ClientIO` of ZEO.

    `get_peername()` gives the address of the server, `protocol_version`
    the protocol, `is_read_only()` tells if the connection registered
    read-only, and `verify_result` tells how the verification of the cache
    ended, in the words of ZEO. `call_async` sends a call without a reply.
    """

    def __init__(self, storage, address, read_only, verify_result):
        self._storage = storage
        self._address = address
        self._read_only = read_only
        self.protocol_version = storage.protocol_version
        self.verify_result = verify_result

    def get_peername(self):
        return self._address

    def is_read_only(self):
        return self._read_only

    def call_async(self, method, args):
        self._storage._async(method, *args)


class _NativeServer:
    """The `_server` of the native client, in place of the client thread of
    ZEO: its calls that do not need asyncio.

    `call` and `async_` send a call of the server, as `_call` and `_async`
    do. `timeout` is the longest wait for a connection. `client` is the
    current `_NativeConnection`, or None before the first connection.
    """

    def __init__(self, storage, timeout):
        self._storage = storage
        self.timeout = timeout
        self.client = None

    def call(self, method, *args, wait=True, timeout=None):
        return self._storage._call(method, *args, wait=wait)

    def async_(self, method, *args):
        self._storage._async(method, *args)

    def is_connected(self):
        return self._storage.is_connected()

    def is_read_only(self):
        return self._storage.is_read_only()


def _dropping_of(storage):
    """Return the function that the native client calls before it drops
    every entry of the cache of `storage`. It holds `storage` by a weak
    reference, so the native client makes no cycle with it."""
    ref = weakref.ref(storage)

    def dropping(reason):
        storage = ref()
        if storage is not None:
            storage._cache_dropping(reason)

    return dropping


class _NativeClientStorage(zodb._IntentOwners, ClientStorage):
    """`zoe.zeo.ClientStorage` with the native client inside.

    It takes the arguments of `ZEO.ClientStorage`. It ignores the arguments
    that ZEO 6 ignores too. `native` must stay true. `ssl` must stay None:
    the TLS comes from the `<ssl>` section of `<zoeclient>`.

    With several addresses, the client connects to all of them and keeps
    one server: with `read_only_fallback`, a writable server before a
    read-only one. On a read-only server, it tries a writable server every
    `disconnect_poll` seconds, and takes it when one opens.

    With `wait`, the open waits at most `wait_timeout` seconds for a
    server. Without it, the open does not wait: the client connects in the
    background. When the client loses its server, it connects again in the
    background, at once, then every `disconnect_poll` seconds (1 by
    default). While it has no server, a call that needs the server waits
    for one at most `wait_timeout` seconds, as ZEO does, then raises
    `ClientDisconnected`. A transaction that began before the loss fails
    at its next step. After each connection, it verifies its cache as
    `verify` of ZEO does: through `getInvalidations`, or it drops the
    cache. Then it calls `notify_connected(conn, info)`, and at each loss
    `notify_disconnected()`, as ZEO does. It refuses a server whose last
    transaction is before its own, and logs it as critical. It logs these
    events under the logger `ZEO.ClientStorage`. `min_disconnect_poll`,
    `max_disconnect_poll` and `drop_cache_rather_verify` change nothing,
    as for ZEO 6.

    Each transaction runs on its own connection of a pool of at most
    `_POOL_BOUND` connections, which open on demand. One more connection,
    the listener, orders the tids. A thread of the client gives the
    invalidations of the other clients to the DB, in tid order, once each.
    `tpc_finish` returns, and runs its callback, at the place of its tid in
    that order. `lastTransaction()` gives a tid only after the DB got its
    invalidations or the callback of its commit ran.

    The client cache holds `cache_size` bytes of records, 20 MB by default.
    0 turns it off. The client applies each tid to the cache before the DB
    gets it. `_cache` gives `maxsize`, `path`, `len()`, `contents()` and
    `clear()`, as the cache of ZEO does. Without `client` and `cache`, the
    cache is in memory. With `client`, it is the file
    `<client>-<storage>.zoec` in `var`, or in the current directory; a str
    `cache` names the file of ZEO, whose `.zec` becomes `.zoec`. The open
    never changes a file that is not a cache file of ZOE: it raises
    `ValueError`. The file survives the process: at the open, the client
    verifies it against the server as after a reconnect, and with `wait`
    the open waits for that verification, inside the same `wait_timeout`.
    A second open of the same file, in this process or another, raises
    `zc.lockfile.LockError`, as for ZEO.

    The blobs work as in `ZEO.ClientStorage`, with its code for the blob
    cache. With `blob_dir` and no `shared_blob_dir`, `blob_dir` is a blob
    cache in the layout `zeocache`. `loadBlob` downloads a blob that the
    cache does not hold, under the file lock of its directory, so threads
    and processes download it once. The download writes `<blob>.dl` and
    renames it when the whole blob came. `storeBlob` moves the file into
    the cache and uploads it in chunks, and `tpc_finish` gives it its name
    under the new tid. `blob_cache_size` and `blob_cache_size_check` prune
    the cache in a thread, as ZEO does. With `shared_blob_dir`, the server
    takes the file of `storeBlob` from the shared directory by rename, and
    `loadBlob` reads the shared directory.

    `tpc_vote` resolves the conflicts that the vote lists with
    `tryToResolveConflict`, on the thread of the transaction, stores each
    result and votes again, as ZEO does, for at most nine rounds. A vote
    that still lists a conflict after the ninth round raises
    `ConflictError`. It returns the oids that the server or the client
    resolved. The client cache keeps the pickle that the client resolved,
    and never the pickle of an oid that the server resolved. An error of
    the resolution raises, drops the cache entry of its oid and leaves the
    transaction to `tpc_abort`.

    When `zoe.zeo.ClientStorage` made the instance without `native`, and
    the open with `wait` finds a server that speaks only a protocol of
    `_ZEO_PROTOCOLS`, the instance becomes a `zoe.zeo.ClientStorage` with
    the asyncio client of ZEO inside, opened with the same arguments. It
    logs a warning that names the protocol.

    The other calls of the server go through `_call` and `_async`, on the
    listener, as on the one connection of `ZEO.ClientStorage`, and raise
    what `ZEO.ClientStorage` raises for the same reply. So the iterators,
    `history`, `record_iternext`, `pack`, `undoInfo`, `undoLog` and
    `server_status` are those of `ZEO.ClientStorage`. An iterator of the
    server lives on the listener. After the drop of its Python iterator,
    the next end of a transaction releases it with `iterator_gc`, as ZEO
    does, and the loss of the listener forgets it. `deleteObject`,
    `restore` and `undo` go on the connection of their transaction.

    `prefetch(oids, tid)` loads each oid that the cache does not hold for
    `tid`, all at once, and returns before the replies, which fill the
    cache. `sync()` with `server_sync` pings the listener, and returns when
    the DB got each commit that the server published before the reply. Each
    connection sends `client_label` to the server. `new_addr` gives the
    addresses of the next connection: the client keeps its server until it
    loses it. `len()` and `getSize()` read the last sizes that the server
    sent.
    """

    # The native client speaks only the msgpack protocol of ZEO 6.
    protocol_version = b"M5"

    def __init__(
        self,
        addr,
        storage="1",
        cache_size=20 * _MB,
        name="",
        wait_timeout=None,
        disconnect_poll=None,
        read_only=0,
        read_only_fallback=0,
        blob_dir=None,
        shared_blob_dir=False,
        blob_cache_size=None,
        blob_cache_size_check=10,
        client_label=None,
        cache=None,
        ssl=None,
        ssl_server_hostname=None,
        client=None,
        var=None,
        min_disconnect_poll=1,
        max_disconnect_poll=None,
        wait=True,
        drop_cache_rather_verify=True,
        credentials=None,
        server_sync=False,
        username=None,
        password=None,
        realm=None,
        native=True,
        early_locks=False,
        early_lock_classes=None,
        _ssl_section=None,
    ):
        if not (native or native is None):
            raise ValueError("this class runs the native client: native is true")
        # A bad list of hot classes raises before the client connects.
        self._early_lock_classes = zodb.hot_classes(early_locks, early_lock_classes)
        given = dict(locals())
        fall_back = self.__dict__.pop("_fall_back", False) and wait
        cache_path = _native_cache_path(cache, var, client, storage)
        if ssl is not None:
            raise Unsupported(
                "the native client takes no ssl context: give the files of "
                "TLS in the <ssl> section of <zoeclient>"
            )
        tls = None
        if _ssl_section is not None:
            tls = _native_tls(_ssl_section, ssl_server_hostname)
        if isinstance(addr, int):
            addr = ("127.0.0.1", addr)
        self.__name__ = name or str(addr)
        addr = self._normalize_addr(addr)
        # ZEO's tests read `_addr` to open a second client.
        self._addr = addr
        self._storage = storage
        self._read_only = bool(read_only)
        self._read_only_fallback = bool(read_only_fallback)
        self._server_addr = None
        self._client_label = None
        self._info = {
            "length": 0,
            "size": 0,
            "name": "ZEO Client",
            "supportsUndo": 0,
            "interfaces": (),
        }
        self._db = None
        self._oids = []
        self._oids_lock = threading.Lock()
        # A child of `os.fork()` must not use the oids of its parent.
        self._pid = os.getpid()
        self._setup_blobs(
            blob_dir, shared_blob_dir, blob_cache_size, blob_cache_size_check
        )
        self._client_label = client_label
        self.server_sync = bool(server_sync)
        # The iterators of the server, as ZEO keeps them. The lock covers
        # both.
        self._iterators = weakref.WeakValueDictionary()
        self._iterator_ids = set()
        self._iterator_lock = threading.Lock()
        # The open transactions, each with its transaction of the native
        # client. The lock covers the check and the change of the dict.
        self._transactions = {}
        # The blob files that each open transaction stored, as (oid, file).
        # The lock of `_transactions` covers it too.
        self._transaction_blobs = {}
        self._transactions_lock = threading.Lock()
        # A thread that gives a tid to the DB marks itself here. Its own
        # call of `lastTransaction()` does not wait for that tid.
        self._local = threading.local()
        self._delivery = None
        self._server = _NativeServer(self, wait_timeout or 30)
        # Set while the DB has the server of the client: the thread of the
        # deliveries sets it at each connection and clears it at the loss.
        self._operational = threading.Event()
        self._closed = False
        # The answer of `kv_supported` from the last server.
        self._kv_accepted = False

        from zoe import _zoe

        # Set once `_native` and `_cache` exist: the handlers of `StaleCache`
        # can read the cache from then on.
        self._native_ready = threading.Event()
        started = time.monotonic()
        try:
            self._native = _zoe.Client(
                [_native_address(one) for one in addr],
                storage,
                read_only=self._read_only,
                read_only_fallback=self._read_only_fallback,
                extensions=list(_NATIVE_EXTENSIONS),
                wait_timeout=float(wait_timeout or 30),
                pool_bound=_POOL_BOUND,
                cache_size=cache_size,
                tls=tls,
                traceparent=_traceparent,
                wait=bool(wait),
                disconnect_poll=float(disconnect_poll or _DISCONNECT_POLL),
                client_label=client_label,
                cache_path=cache_path,
                dropping=_dropping_of(self),
                fall_back_on=list(_ZEO_PROTOCOLS if fall_back else ()),
            )
        except _zoe.OtherProtocol as error:
            self._open_zeo_client(given, error.args[0])
            return
        self._cache = _NativeCache(self._native, cache_size, cache_path)
        self._native_ready.set()
        self._init_intents()
        if self._early_lock_classes:
            zodb.install()
        try:
            if wait:
                # The connect and the verification of a cache file share one
                # wait.
                elapsed = time.monotonic() - started
                self._native.ready(max(self._server.timeout - elapsed, 0))
            # Without `wait`, the event of the first connection takes the
            # server, so the client takes it once for each connection.
            if wait and self._native.is_connected():
                self._take_server(self._verify_result(self._check_cache_file()))
            else:
                logger.info(
                    "%s native client connects to storage %r in the background",
                    self.__name__,
                    storage,
                )
            self._delivery = threading.Thread(
                target=self._deliver_forever,
                name=f"{self.__name__} zoe native client invalidations",
                daemon=True,
            )
            self._delivery.start()
        except BaseException:
            self._native.close()
            raise

    def _open_zeo_client(self, given, protocol):
        """Become a `ClientStorage` with the asyncio client of ZEO inside,
        opened with the arguments `given` of this open. The server speaks
        `protocol`, which the client of ZEO speaks and the native client
        does not."""
        logger.warning(
            "%s the server speaks the protocol %r, not M5: the client of ZEO "
            "runs inside; set msgpack true on the server for the native client",
            self.__name__,
            protocol,
        )
        args = {
            key: value
            for key, value in given.items()
            if key not in ("self", "native", "_ssl_section", "__class__")
        }
        # The client of ZEO takes no early locks. The locks are advisory, so
        # the client goes on without them.
        if args.pop("early_locks") and args.pop("early_lock_classes"):
            logger.warning(
                "%s the client of ZEO takes no early lock: early-locks on "
                "needs a server that speaks M5",
                self.__name__,
            )
        args.pop("early_lock_classes", None)
        self.__dict__.clear()
        self.__class__ = ClientStorage
        ClientStorage.__init__(
            self, native=False, _ssl_section=given["_ssl_section"], **args
        )

    @property
    def _is_read_only(self):
        """Tell if the server registered the client read-only."""
        native = self.__dict__.get("_native")
        if native is None:
            return self._read_only
        return native.read_only()

    def _take_server(self, verify_result=None):
        """Take the server of the native client: call `notify_connected`
        with its connection and its info, as ZEO does at each connection."""
        conn = _NativeConnection(
            self, self._native.address(), self._native.read_only(), verify_result
        )
        self._server.client = conn
        self.notify_connected(conn, self._native.info())
        self._operational.set()

    def notify_connected(self, conn, info):
        """Take the info and the address of a new connection, as ZEO does.

        The native client verified its cache before the call. The log gives
        the address: the canonical name comes at the first read of
        `_server_addr`.
        """
        address = conn.get_peername()
        if address is not None:
            self.set_server_addr(address)
        self.protocol_version = conn.protocol_version
        _zeo_logger.info(
            "%s %s to storage: %s",
            self.__name__,
            "Reconnected" if self._connection_generation else "Connected",
            self._server_address,
        )
        self._connection_generation += 1
        self._info.update(info)
        # Of the interfaces of the server that ZEO declares, the client
        # declares those that it does not have yet, in one call.
        served = self._info["interfaces"]
        declared = [
            iface
            for iface in _SERVER_INTERFACES
            if (iface.__module__, iface.__name__) in served
            and not iface.providedBy(self)
        ]
        if declared:
            zope.interface.alsoProvides(self, *declared)
        # The locks are advisory: without the extension, the client takes no
        # early lock, and says so once for the connection. A read-only
        # client writes nothing, so it takes no early lock.
        classes = self.__dict__.get("_early_lock_classes")
        if classes and _LOCK_INTENT not in self._info.get("extensions", ()):
            logger.warning(
                "%s the server did not accept the extension lock_intent: "
                "early-locks on takes no early lock",
                self.__name__,
            )
            classes = None
        self._hot_classes = None if self._is_read_only else classes

    @property
    def early_lock_classes(self):
        """The hot classes of early locks on the server of the last
        connection, or None: without early locks, before the first
        connection, on a read-only client, or when the server did not accept
        the extension `lock_intent`."""
        return self.__dict__.get("_hot_classes")

    def notify_disconnected(self):
        """The client lost its server: forget the iterators of the server,
        as ZEO does."""
        _zeo_logger.info(
            "%s Disconnected from storage: %r", self.__name__, self._server_address
        )
        self._iterator_gc(True)
        self._connection_generation += 1

    def _verify_result(self, kind):
        """Return how the verification of the cache ended, in the words of
        ZEO, from a kind of the native client or None. A cache without
        entries gives "empty cache", as for ZEO."""
        if kind in ("dropped", "failed", "ahead"):
            return _VERIFY_RESULTS[kind]
        if not len(self._cache):
            return "empty cache"
        return _VERIFY_RESULTS.get(kind, "Cache up to date")

    def set_server_addr(self, addr):
        """Keep the address of the server.

        The first read of `_server_addr` gives the canonical name of the
        server, as `set_server_addr` of ZEO makes it. The lookup of that
        name is slow, so the open does not wait for it.
        """
        self.__dict__["_server_address"] = addr
        self.__dict__["_canonical_addr"] = None

    @property
    def _server_address(self):
        """The address of the server as the client got it, or None."""
        return self.__dict__.get("_server_address")

    @property
    def _server_addr(self):
        """The address of the server with its canonical name, or None."""
        canonical = self.__dict__.get("_canonical_addr")
        if canonical is None:
            address = self.__dict__.get("_server_address")
            if address is None:
                return None
            # This sets `_server_addr`, through the setter below.
            ZEOClientStorage.set_server_addr(self, address)
            canonical = self.__dict__["_canonical_addr"]
        return canonical

    @_server_addr.setter
    def _server_addr(self, value):
        self.__dict__["_canonical_addr"] = value

    def _setup_blobs(self, blob_dir, shared, cache_size, cache_size_check):
        """Set the blob directory up as `ZEO.ClientStorage` does."""
        self.blob_dir = blob_dir
        self.shared_blob_dir = shared
        if blob_dir is None:
            self.fshelper = None
        elif shared:
            self.fshelper = ZODB.blob.FilesystemHelper(blob_dir)
        else:
            # The layout of ZEO's blob cache, so either client reads it.
            if "zeocache" not in ZODB.blob.LAYOUTS:
                ZODB.blob.LAYOUTS["zeocache"] = BlobCacheLayout()
            self.fshelper = ZODB.blob.FilesystemHelper(blob_dir, layout_name="zeocache")
            self.fshelper.create()
        # The lock covers the count of the bytes that came into the cache.
        self._blob_lock = threading.Lock()
        self._blob_cache_size = cache_size
        self._blob_data_bytes_loaded = 0
        if cache_size is not None:
            if not 0 <= cache_size_check < 100:
                raise ValueError("blob_cache_size_check is a percent below 100")
            self._blob_cache_size_check = cache_size * cache_size_check // 100
            self._check_blob_size()

    def _blob_bytes_came(self, size):
        """Count `size` bytes that came into the blob cache. Prune the cache
        in a thread when the count passes the check size, as ZEO does."""
        with self._blob_lock:
            self._blob_data_bytes_loaded += size
            self._check_blob_size(self._blob_data_bytes_loaded)

    def _check_process(self):
        """Raise `RuntimeError` in a child of `os.fork()`.

        The check comes before any lock: a thread of the parent can hold a
        lock at the fork, and the child would wait for it for ever.
        """
        if os.getpid() != self._pid:
            raise RuntimeError(
                "the client was opened in another process, before a fork"
            )

    # The calls of the server, on the listener.

    def _wait(self, timeout=None):
        """Wait for a server, as ZEO does before a call.

        Wait at most `timeout` seconds, `_server.timeout` by default, then
        raise `ClientDisconnected`. After `close`, raise at once.
        """
        if timeout is None:
            timeout = self._server.timeout
        deadline = time.monotonic() + timeout
        while not self._native.is_connected():
            left = deadline - time.monotonic()
            if self._closed or left <= 0:
                raise ClientDisconnected("the client has no server")
            # The native client connects before the thread of the
            # deliveries sets the event, and the event stays set for a
            # moment after the loss: wait in short steps.
            if self._operational.wait(min(left, 0.1)):
                time.sleep(min(left, 0.01))

    def _with_server(self, call, timeout=None):
        """Return `call()`. Without a server, wait for one as ZEO does and
        call again. A call that loses its server raises."""
        try:
            return call()
        except ClientDisconnected:
            if self._closed or self._native.is_connected():
                raise
        self._wait(timeout)
        return call()

    def _call(self, method, *args, timeout=None, wait=True):
        # As ZEO, wait at most `timeout` for a server before the call.
        self._check_process()
        if not wait:
            # As ZEO, return a future of the reply. A thread waits for it,
            # in the context of the caller, so the call keeps its span.
            future = concurrent.futures.Future()
            context = contextvars.copy_context()

            def run():
                if not future.set_running_or_notify_cancel():
                    return
                try:
                    result = context.run(self._call, method, *args)
                except BaseException as error:
                    future.set_exception(error)
                else:
                    future.set_result(result)

            threading.Thread(
                target=run, name=f"{self.__name__} zoe native call", daemon=True
            ).start()
            return future
        # The code of ZEO for the blob cache downloads a blob with this
        # call, under the lock of the blob.
        if not self._native.is_connected():
            self._wait(timeout)
        if method == "sendBlob":
            return self._send_blob(*args)
        if method in _ITERATOR_CALLS:
            return self._iterator_call(method, args)
        return self._native.call(method, args)

    def _iterator_call(self, method, args):
        """Call `method` on the listener of the generation of its iterator
        id, and give the id of a new iterator the generation of the listener
        that answered. An id of a lost listener raises the `KeyError` of an
        id that the server does not know, and sends nothing."""
        generation = None
        if args and isinstance(args[0], _Iid):
            generation = args[0].generation
            args = (int(args[0]), *args[1:])
        reply, generation = self._native.call_on(method, args, generation)
        if method in _ITERATOR_STARTS and isinstance(reply, int):
            return _Iid(reply, generation)
        return reply

    def _send_blob(self, oid, serial):
        """Download the blob of `oid` at `serial` into the blob cache."""
        self._check_process()
        filename = self.fshelper.getBlobFilename(oid, serial)
        size = self._native.send_blob(oid, serial, filename)
        self._blob_bytes_came(size)

    def _async(self, method, *args):
        self._check_process()
        self._native.call_async(method, args)

    def new_addr(self, addr):
        self._check_process()
        self._native.new_addr(
            [_native_address(one) for one in self._normalize_addr(addr)]
        )
        self._addr = addr

    def ping(self):
        self._call("ping")

    def sync(self):
        if not self.server_sync:
            return
        self._check_process()
        # A thread that gives a tid to the DB does not wait for itself.
        self._native.sync(not getattr(self._local, "delivering", False))

    # The iterators of the server, as ZEO keeps them, with a lock: several
    # transactions end at once.

    def _setup_iterator(self, factory, iid, *args):
        iterator = factory(self, iid, *args)
        with self._iterator_lock:
            self._iterators[iid] = iterator
            self._iterator_ids.add(iid)
        return iterator

    def _forget_iterator(self, iid):
        with self._iterator_lock:
            self._iterators.pop(iid, None)
            self._iterator_ids.discard(iid)

    def _iterator_gc(self, disconnected=False):
        with self._iterator_lock:
            if not self._iterator_ids:
                return
            if disconnected:
                for iterator in self._iterators.values():
                    iterator._iid = -1
                self._iterators.clear()
                self._iterator_ids.clear()
                return
            iids = self._iterator_ids - set(self._iterators)
            # ZEO's tests read the time of the last collection.
            self._iterators._last_gc = time.time()
            if not iids:
                return
            self._iterator_ids -= iids
        # Each id goes only to the listener of its generation. The server of
        # a lost listener dropped its iterators.
        generations = {}
        for iid in iids:
            generation = getattr(iid, "generation", None)
            generations.setdefault(generation, []).append(int(iid))
        try:
            for generation, ids in generations.items():
                self._check_process()
                self._native.call_async("iterator_gc", (ids,), generation)
        except ClientDisconnected:
            # The server dropped every iterator of the connection.
            self._iterator_gc(True)

    # The calls of a transaction.

    def deleteObject(self, oid, serial, txn):
        native = self._check_trans(txn, "deleteObject")
        native.delete_object(oid, _serial(serial))

    def restore(self, oid, serial, data, version, prev_txn, transaction):
        if version:
            raise Unsupported("versions are not supported")
        native = self._check_trans(transaction, "restore")
        native.restore(oid, serial, data, prev_txn)

    def undo(self, trans_id, txn):
        self._check_trans(txn, "undo").undo(trans_id)

    # The info of the server: `get_info` and the last `info` call.

    def _update_info(self):
        info = self._native.info()
        if info:
            self._info.update(info)

    def __len__(self):
        self._update_info()
        return self._info["length"]

    def getSize(self):
        self._update_info()
        return self._info["size"]

    # The blobs. `loadBlob`, `openCommittedBlobFile` and
    # `temporaryDirectory` are those of `ZEO.ClientStorage`.

    def storeBlob(self, oid, serial, data, blobfilename, version, txn):
        if version:
            raise Unsupported("versions are not supported")
        native = self._check_trans(txn, "storeBlob")
        # Take the file at once, as ZEO does: into the directory of the oid,
        # under a temporary name that ends in `.tmp-`.
        self.fshelper.getPathForOID(oid, create=True)
        fd, target = self.fshelper.blob_mkstemp(oid, serial)
        os.close(fd)
        target += "-"
        ZODB.blob.rename_or_copy_blob(blobfilename, target)
        os.remove(target[:-1])
        with self._transactions_lock:
            self._transaction_blobs.setdefault(txn, []).append((oid, target))
        data = _as_bytes(data)
        serial = _serial(serial)
        if self.shared_blob_dir:
            native.store_blob_shared(oid, serial, data, os.path.basename(target))
        else:
            native.store_blob(oid, serial, data, target)

    def _blobs_of(self, txn):
        """Return the blob files that txn stored, and forget them."""
        with self._transactions_lock:
            return self._transaction_blobs.pop(txn, [])

    def _cache_blobs(self, blobs, tid):
        """Give each uploaded blob file its name in the cache under `tid`.

        This is `_update_blob_cache` of ZEO: each rename runs under the
        lock of the blob. Then count the bytes for the pruning.
        """
        size = 0
        for oid, filename in blobs:
            size += os.stat(filename).st_size
            self.fshelper.getPathForOID(oid, create=True)
            target = self.fshelper.getBlobFilename(oid, tid)
            lock = _lock_blob(target)
            try:
                ZODB.blob.rename_or_copy_blob(filename, target)
            finally:
                lock.close()
        if blobs:
            self._blob_bytes_came(size)

    def prefetch(self, oids, tid):
        self._check_process()
        self._native.prefetch(list(oids), tid)

    # The state of the connection.

    def is_connected(self, test=False):
        return self._native.is_connected()

    def is_read_only(self):
        return self._is_read_only

    isReadOnly = is_read_only

    def close(self):
        self._check_process()
        self._closed = True
        self._native.close()
        delivery = self._delivery
        if delivery is not None and delivery is not threading.current_thread():
            delivery.join(_JOIN_TIMEOUT)
        if self._check_blob_size_thread is not None:
            self._check_blob_size_thread.join()

    # The invalidations.

    def _deliver_forever(self):
        """Give each delivery of the native client to the DB until the close.

        The native client gives the commits of the other clients in tid
        order, once each, a mark when it lost its listener, and each change
        of the connection. It gives the next one only after this thread
        confirmed the last one.
        """
        native = self._native
        while True:
            try:
                delivery = native.next_delivery()
            except Exception:
                logger.exception("%s the invalidations stopped", self.__name__)
                # Without this thread, the pool delivers no more tids. Close
                # the client, so a waiting call fails and does not hang.
                try:
                    native.close()
                except Exception:
                    logger.exception("%s the close failed", self.__name__)
                return
            if delivery is None:
                return
            self._local.delivering = True
            try:
                kind = delivery[0]
                if kind == "connected":
                    self._connected(delivery[1])
                elif kind == "refused":
                    self._refused(delivery[1])
                elif kind == "stale":
                    self._operational.clear()
                    # The iterators of the server went with the listener.
                    self.notify_disconnected()
                    # The client lost its listener: any object can be stale.
                    if self._db is not None:
                        self._db.invalidateCache()
                elif self._db is not None:
                    self._db.invalidate(delivery[1], delivery[2])
            except Exception:
                logger.exception("%s the DB refused an invalidation", self.__name__)
            finally:
                self._local.delivering = False
                native.delivered()

    def _connected(self, verification):
        """The native client connected, after the verification of its
        cache: `kept`, `quick` or `dropped`."""
        # A pool that opened without a server verifies its cache file at its
        # first connection.
        check = self._check_cache_file()
        if verification == "dropped":
            # The server does not keep the commits after the last tid of
            # the client: its cache is empty, and any object can be stale.
            self._stale_cache()
        if check is not None and self._connection_generation == 0:
            verification = check
        self._take_server(self._verify_result(verification))

    # True when `_cache_dropping` notified the drop that the next call of
    # `_stale_cache` reports.
    _stale_notified = False

    def _cache_dropping(self, reason):
        """The native client drops every entry of its cache when this call
        returns. Notify `StaleCache` and log it now, as ZEO does before its
        drop, so a handler of the event sees the entries.

        It runs on a thread of its own. At the open, it waits until the
        storage has its cache.
        """
        self._native_ready.wait(self._server.timeout)
        self._notify_stale(f"getInvalidations failed: {reason}" if reason else "")
        self._stale_notified = True

    def _notify_stale(self, text):
        """Notify `StaleCache` and log the dropped cache as ZEO does."""
        try:
            ZODB.event.notify(ZEO.interfaces.StaleCache(self))
        except Exception:
            _zeo_logger.exception("%s sending StaleCache event", self.__name__)
        if text:
            _zeo_logger.critical("%s dropping stale cache: %s", self.__name__, text)
        else:
            _zeo_logger.critical("%s dropping stale cache", self.__name__)

    def _stale_cache(self, text=""):
        """The client dropped its cache: any object of the DB can be stale.
        Notify `StaleCache` and log it, unless `_cache_dropping` did it
        before the drop."""
        if self._stale_notified:
            self._stale_notified = False
        else:
            self._notify_stale(text)
        if self._db is not None:
            self._db.invalidateCache()

    def _check_cache_file(self):
        """Log how the verification of the cache file ended, once, and
        return its kind, or None.

        A server that does not keep the commits after the file, or a failed
        `getInvalidations`, drops the file as ZEO drops a stale cache. A
        file ahead of the server is dropped too, where ZEO refuses the
        server: the DB has no state yet. So is a file whose last
        transaction the server does not hold: the server has another
        history. A verification that ends after the wait of the open is
        logged at the next connection.
        """
        check = self._native.take_cache_check()
        if check is None:
            return None
        kind, text = check
        if kind in ("ahead", "other"):
            _zeo_logger.critical(
                "%s Client cache is out of sync with the server. The native "
                "client drops its cache file. Verify that this is expected: %s",
                self.__name__,
                text,
            )
        elif kind == "dropped":
            self._stale_cache()
        elif kind == "failed":
            self._stale_cache(f"getInvalidations failed: {text}")
        return kind

    def _refused(self, text):
        """The native client refused a server that is behind it."""
        _zeo_logger.critical(
            "%s Client cache is out of sync with the server. The client "
            "refuses the server and tries again later. Verify that this is "
            "expected: %s",
            self.__name__,
            text,
        )

    def lastTransaction(self):
        self._check_process()
        # A thread that gives a tid to the DB gets the last tid at once. Any
        # other thread waits for the tid that the DB gets now, if any.
        wait = not getattr(self._local, "delivering", False)
        return self._native.last_transaction(wait)

    # The loads.

    def load(self, oid, version=""):
        # The current entry of the cache answers, as for ZEO.
        result = self._with_server(lambda: self._native.load(oid))
        if result is None:
            raise POSKeyError(oid)
        return result

    def loadBefore(self, oid, tid):
        return self._with_server(lambda: self._native.load_before(oid, tid))

    def loadSerial(self, oid, serial):
        return self._with_server(lambda: self._native.load_serial(oid, serial))

    def getTid(self, oid):
        return self.load(oid)[1]

    def new_oid(self):
        self._check_process()
        if self._is_read_only:
            raise ReadOnlyError()
        with self._oids_lock:
            if not self._oids:
                oids = self._with_server(lambda: self._native.new_oids(_NEW_OIDS))
                self._oids = list(reversed(oids))
            return self._oids.pop()

    def _sequence_next(self, name):
        return self._native.sequence_next(name)

    # The KV namespaces.

    def kv_supported(self):
        """Tell if the server accepted `kv` on the connection. While the
        client has no server, give the answer of the last server: the calls
        then raise `ClientDisconnected`, and a caller does not take the path
        of a storage without namespaces for that time."""
        connected = self._native.is_connected()
        extensions = self._native.extensions()
        # Keep only an answer of a listener: a list that is not empty, or an
        # empty list that the client read while it stayed connected.
        if extensions or (connected and self._native.is_connected()):
            self._kv_accepted = _KV in extensions
        return self._kv_accepted

    def kv_get(self, flavour, name, key):
        return self._with_server(lambda: self._native.kv_get(flavour, name, key))

    def kv_scan(self, flavour, name, start=None, stop=None, limit=1000):
        return self._with_server(
            lambda: self._native.kv_scan(flavour, name, start, stop, limit)
        )

    def kv_generation(self, name):
        return self._with_server(lambda: self._native.kv_generation(name))

    def kv_set(self, flavour, name, key, value, ttl, transaction):
        native = self._check_trans(transaction, "kv_set")
        native.kv_set(flavour, name, key, value, ttl)

    def kv_delete(self, flavour, name, key, transaction):
        self._check_trans(transaction, "kv_delete").kv_delete(flavour, name, key)

    # The owners of the early write-intent locks of `zodb._IntentOwners`
    # (ADR 0045). The first lock of an owner leases a connection of the
    # pool, which it keeps until `unlock_intent` or the `tpc_begin` that
    # adopts its locks. A server that did not accept the extension
    # `lock_intent` gives `Unsupported`.

    def _new_owner(self):
        return self._native.intents()

    def _take_intents(self, owner, oids):
        self._check_process()
        if _LOCK_INTENT not in self._info.get("extensions", ()):
            raise Unsupported("the server does not serve the extension lock_intent")
        return self._with_server(lambda: self._native.lock_intent(owner, oids))

    def _release_intents(self, owner):
        self._check_process()
        self._native.unlock_intent(owner)

    # The two-phase commit.

    def _check_trans(self, trans, meth):
        """Return the native transaction of trans."""
        if self._is_read_only:
            raise ReadOnlyError()
        native = self._transactions.get(trans) if trans is not None else None
        if native is None:
            raise StorageTransactionError("Transaction not committing", meth, trans)
        return native

    def tpc_transaction(self):
        """Return the transaction that began last and did not end, or None."""
        with self._transactions_lock:
            return next(reversed(self._transactions), None)

    def tpc_begin(self, txn, tid=None, status=" "):
        self._check_process()
        if self._is_read_only:
            raise ReadOnlyError()
        with self._transactions_lock:
            if txn in self._transactions:
                raise StorageTransactionError(
                    "Duplicate tpc_begin calls for same transaction"
                )
            # The entry holds the place until the native transaction exists.
            self._transactions[txn] = None
        try:
            if isinstance(status, bytes):
                status = status.decode("ascii")
            # The transaction adopts the intent locks of the owner that the
            # thread bound, or of the default owner.
            owner = self._begin_owner()
            native = self._native.begin(
                _as_bytes(txn.user),
                _as_bytes(txn.description),
                txn.extension,
                tid,
                status,
                owner,
            )
        except BaseException:
            with self._transactions_lock:
                self._transactions.pop(txn, None)
            raise
        with self._transactions_lock:
            self._transactions[txn] = native

    def store(self, oid, serial, data, version, txn):
        if version:
            raise Unsupported("versions are not supported")
        self._check_trans(txn, "store").store(oid, _serial(serial), data)

    def checkCurrentSerialInTransaction(self, oid, serial, transaction):
        native = self._check_trans(transaction, "checkCurrentSerialInTransaction")
        native.check_current_serial(oid, _serial(serial))

    def tpc_vote(self, txn):
        # The native client resolves each conflict that the vote lists with
        # `tryToResolveConflict`, on this thread, as ZEO does.
        native = self._check_trans(txn, "tpc_vote")
        return native.vote(self.tryToResolveConflict) or None

    def tpc_finish(self, txn, f=lambda tid: None):
        self._check_process()
        native = self._check_trans(txn, "tpc_finish")
        tid = None
        try:
            # The finish returns at the turn of its tid: the DB got every
            # tid below it. The client gives no later tid to the DB until
            # `done`, so `f(tid)` runs at the place of the tid.
            tid = native.finish()
            self._local.delivering = True
            try:
                # The server wrote an oid that the transaction did not
                # store: every Connection gets every oid of the commit in
                # one invalidation, before `lastTransaction()` gives the
                # tid. Two calls would let a Connection see a part of the
                # commit.
                oids = native.unstored_commit()
                if oids is not None and self._db is not None:
                    self._db.invalidate(tid, oids)
                f(tid)
            finally:
                self._local.delivering = False
                native.done()
        finally:
            blobs = self._blobs_of(txn)
            self.tpc_end(txn)
            # The commit holds the blobs once the finish gives a tid, also
            # when the callback raises.
            if tid is None:
                _remove_files(blobs)
            elif not self.shared_blob_dir:
                self._cache_blobs(blobs, tid)
            self._iterator_gc()
        return tid

    def tpc_abort(self, txn, timeout=None):
        self._check_process()
        native = self._transactions.get(txn) if txn is not None else None
        if native is None:
            return
        try:
            native.abort()
        except ClientDisconnected:
            logger.debug("%s ClientDisconnected in tpc_abort() ignored", self.__name__)
        finally:
            blobs = self._blobs_of(txn)
            self.tpc_end(txn)
            # The server ended the transaction, so it no longer reads these
            # files. In a shared directory, the server already moved each
            # file that it staged.
            _remove_files(blobs)
            self._iterator_gc()

    def tpc_end(self, txn):
        if txn is not None:
            with self._transactions_lock:
                self._transactions.pop(txn, None)
