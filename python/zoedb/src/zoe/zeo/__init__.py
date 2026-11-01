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

`kv_get`, `kv_scan`, `kv_generation`, `kv_set` and `kv_delete` give the
KV namespaces of the store, as `zoe.Storage` does (ADR 0043). They need a
server that accepted `kv` on the connection: `kv_supported()` tells. Without
it, each raises `ZODB.POSException.Unsupported`. `zoe.kv.Namespace` gives a
namespace to a connection of a database.

When the package `opentelemetry` is installed, each connection also declares
`trace_context`. When the server accepts it, each call that the client makes
inside a span carries the W3C traceparent of that span: a fifth element of
the message, a map with the key `traceparent`. The span of the call on the
server then joins the trace of the client. A call outside a span has four
elements. The client exports no span: the application configures
OpenTelemetry.

`%import zoe` gives the ZConfig section `<zoeclient>`, which opens this class.
Its `<ssl>` section gives the files of TLS: the client builds its TLS from
them, as ZEO's `<clientstorage>` does.

`ClientStorage` runs on the native client of `zoe._zoe`, built on the Rust
crate `zoe-client`, and not on the asyncio client of ZEO. This is the
default. `native=False` keeps the asyncio client of ZEO inside. A subclass
of `ClientStorage` keeps the asyncio client of ZEO, because it can change
its methods. The native client is still an instance of
`ZEO.ClientStorage.ClientStorage`, and no thread of ZEO starts. It connects
as ZEO does: to a `(host, port)` address, to the path of a unix domain
socket, or to the first server of a list that opens, with TLS from the
`<ssl>` section of `<zoeclient>`. It checks the server against the CA
certificates of `authenticate`, a file or a directory, else against those
of the system, as ZEO does. It takes no `ssl` context: TLS runs in
Rust, and a context does not give back the files of its certificate and
key. It runs several transactions at once, each on its own connection of a
pool, with one more connection that orders the invalidations (ADR 0036). It
keeps a client cache of `cache_size` bytes, with the semantics of the cache
of ZEO (ADR 0034): in memory, or in a file with `client` and `var`, or
`cache`. It has blobs in both modes of ZEO: a blob cache in `blob_dir`, or
a blob directory that it shares with the server (`shared_blob_dir`). It
resolves in the client the conflicts that a vote lists, as
`ZEO.ClientStorage` does. When it loses its server, it connects again in
the background and verifies its cache, as ZEO does. It declares
`sequences` and `kv`, and `trace_context` when the package `opentelemetry`
is installed. It sends the traceparent as the client of ZEO does. It has the
rest of the storage API of `ZEO.ClientStorage`: the iterators, `history`,
`record_iternext`, `pack`, `undoInfo` and `undoLog`, `server_status`,
`deleteObject`, `restore`, `prefetch`, `sync` with `server_sync`,
`new_addr` and `client_label`. It speaks only the msgpack protocol `M5`: a
stock ZEO server needs `msgpack true`. When the caller gives no `native`
and the open with `wait` finds a stock ZEO server that speaks only the
pickle protocol `Z5`, the instance takes the asyncio client of ZEO inside
and logs a warning.

The module `_asyncio` holds `ClientStorage` and the patches of the client
of ZEO, and `_native` the native client. This package gives their names.
"""

# The public names, and the private names that the tests and the
# documentation of the package read.
from zoe.zeo._asyncio import _PROPAGATOR as _PROPAGATOR
from zoe.zeo._asyncio import EXTENSIONS, ClientStorage, Sequence
from zoe.zeo._asyncio import _traceparent as _traceparent
from zoe.zeo._native import _NATIVE_EXTENSIONS as _NATIVE_EXTENSIONS
from zoe.zeo._native import _POOL_BOUND as _POOL_BOUND
from zoe.zeo._native import _Iid as _Iid
from zoe.zeo._native import _native_address as _native_address
from zoe.zeo._native import _native_cache_path as _native_cache_path
from zoe.zeo._native import _native_tls as _native_tls
from zoe.zeo._native import _NativeCache as _NativeCache
from zoe.zeo._native import _NativeClientStorage as _NativeClientStorage
from zoe.zeo._native import _NativeServer as _NativeServer
from zoe.zeo._native import _serial as _serial

__all__ = ["EXTENSIONS", "ClientStorage", "Sequence"]
