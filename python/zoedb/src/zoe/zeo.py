"""The ZOE client: `zoe.zeo.ClientStorage`, a drop-in for `ZEO.ClientStorage`.

`ClientStorage` is a subclass of `ZEO.ClientStorage.ClientStorage`, with the
same arguments. Each connection declares the extensions of `EXTENSIONS` in a
third argument of `register`. A ZOE server accepts the names that it serves
and gives them back under the key `extensions` of `get_info`. A stock ZEO
server takes two arguments only: it refuses the third with `TypeError`, and
the connection registers again with two arguments. The client then works as
`ZEO.ClientStorage` does.

`sequence(name).next()` gives the next value of the sequence `name` of the
store, an `int`, as `zoe.Storage` does: unique and strictly increasing per
store and per name, not part of a transaction. It needs a server that
accepted `sequences` on the connection. Without it, `next()` raises
`ZODB.POSException.Unsupported`.

`%import zoe` gives the ZConfig section `<zoeclient>`, which opens this class.
"""

from ZEO.asyncio.client import ClientIO, ClientThread, Fallback, Protocol
from ZEO.asyncio.futures import run_coroutine_threadsafe
from ZEO.ClientStorage import ClientStorage as ZEOClientStorage
from ZODB.POSException import ReadOnlyError, Unsupported

__all__ = ["EXTENSIONS", "ClientStorage", "Sequence"]

# The extensions that each connection declares in `register`.
EXTENSIONS = ("sequences",)


class _Protocol(Protocol):
    """A connection to one server. It declares `EXTENSIONS` in `register`."""

    # The connection sends the declaration until a server refuses it.
    declares = True

    async def _register(self, read_only):
        if self.declares:
            try:
                return await self.server_call(
                    "register", self.storage_key, read_only, list(EXTENSIONS)
                )
            except TypeError:
                # A stock server takes two arguments. Its connection stays
                # open and unregistered, so the client registers again.
                self.declares = False
        return await self.server_call("register", self.storage_key, read_only)

    async def verify_connection(self):
        # This is `Protocol.verify_connection` of ZEO, with `_register`.
        async with self.client.register_lock:
            try:
                try:
                    server_tid = await self._register(
                        False if self.read_only is Fallback else self.read_only
                    )
                except ReadOnlyError:
                    if self.read_only is not Fallback:
                        raise
                    self.read_only = True
                    server_tid = await self._register(True)
                else:
                    if self.read_only is Fallback:
                        self.read_only = False
            except Exception as exc:
                self.client.register_failed(self, exc)
            else:
                await self.client.register(self, server_tid)


class _ClientIO(ClientIO):
    """The client in the network thread. Its connections are `_Protocol`."""

    def try_connecting(self):
        # This is `ClientIO.try_connecting` of ZEO, with `_Protocol`.
        if not self.closed:
            self.protocols = [
                _Protocol(
                    self.loop,
                    addr,
                    self,
                    self.storage_key,
                    self.read_only,
                    self.connect_poll,
                    ssl=self.ssl,
                    ssl_server_hostname=self.ssl_server_hostname,
                )
                for addr in self.addrs
            ]


class _ClientThread(ClientThread):
    """The network thread. It runs a `_ClientIO`."""

    def set_options(
        self,
        addrs,
        wrapper,
        cache,
        storage_key,
        read_only,
        timeout=30,
        disconnect_poll=1,
        **kwargs,
    ):
        super().set_options(
            addrs,
            wrapper,
            cache,
            storage_key,
            read_only,
            timeout,
            disconnect_poll,
            **kwargs,
        )
        self._io_args = (addrs, wrapper, cache, storage_key, read_only, disconnect_poll)
        self._io_kwargs = kwargs

    def setup_delegation(self, loop):
        # This is `ClientRunner.setup_delegation` of ZEO, with `_ClientIO`.
        self.loop = loop
        self.client = client = _ClientIO(loop, *self._io_args, **self._io_kwargs)
        self.call_sync_co = client.call_sync_co
        self.load_before_co = client.load_before_co

        def io_call(coro, wait=True):
            future = run_coroutine_threadsafe(coro, loop)
            try:
                return future.result() if wait else future
            finally:
                del future

        self.io_call = io_call


class Sequence:
    """A named counter of the store of a `ClientStorage`.

    `next()` gives values that are unique and strictly increasing per store
    and per name, from 1.
    """

    def __init__(self, storage, name):
        self._storage = storage
        self._name = name

    def next(self):
        """Return the next value of the sequence.

        The value is not part of a transaction: an abort does not give it
        back. Each restart of the server can skip up to 100 values. Raise
        `Unsupported` when the server did not accept `sequences`.
        """
        return self._storage._sequence_next(self._name)


class ClientStorage(ZEOClientStorage):
    """`ZEO.ClientStorage` with the ZOE extensions that the server accepts."""

    def __init__(self, *args, _client_factory=_ClientThread, **kw):
        super().__init__(*args, _client_factory=_client_factory, **kw)

    def notify_connected(self, conn, info):
        super().notify_connected(conn, info)
        # A server that got no declaration gives no key `extensions`. Then
        # remove the key of an earlier connection. The key stays during the
        # update, so a call of `next()` meanwhile does not fail.
        if "extensions" not in info:
            self._info.pop("extensions", None)

    def sequence(self, name):
        """Return the sequence `name` of the store."""
        if not isinstance(name, str):
            raise TypeError(
                f"the name of a sequence is a str, not {type(name).__name__}"
            )
        return Sequence(self, name)

    def _sequence_next(self, name):
        if "sequences" not in self._info.get("extensions", ()):
            raise Unsupported("the server does not serve the extension sequences")
        return self._call("sequence_next", name)
