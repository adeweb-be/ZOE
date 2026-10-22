# Protocol

`zoe serve` speaks the ZEO 6 protocol, so an unmodified `ZEO.ClientStorage` connects. This page gives the methods of the protocol, what `zoe serve` does with each group, and the ZOE extensions. The [technical specification](../spec/technical-specification.md) gives the full contract, and `docs/spikes/zeo-wire.md` the shapes of the messages.

## Who talks to whom

| Client | Server | Works | Notes |
| --- | --- | --- | --- |
| stock `ZEO.ClientStorage` | `zoe serve` | yes | The client needs the package `msgpack`: `zoe serve` speaks only the msgpack protocol `M5`. The extra `zoedb[plone]` installs `msgpack`. The client declares no extension, so it gets no ZOE extension. |
| `zoe.zeo.ClientStorage`, native | `zoe serve` | yes | It declares the ZOE extensions. |
| `zoe.zeo.ClientStorage`, native | stock ZEO 6 server with `msgpack true` | yes | The server refuses the third argument of `register`, and the client registers again with two. No ZOE extension. |
| `zoe.zeo.ClientStorage`, no `native` | stock ZEO 6 server that speaks only `Z5` | yes | With `wait`, the client takes the asyncio client of ZEO inside, and logs a warning. |
| `zoe.zeo.ClientStorage`, `native=True` | stock ZEO 6 server that speaks only `Z5` | no | The connection fails. |
| ZEO 5 client or server | any | no | ZOE targets ZODB 6 and ZEO 6 only (see [Compatibility](compatibility.md)). |

## The wire

- A frame is a 4-byte big-endian length and a payload.
- The server sends `M5` first. The client answers `M5`, or `ruok` for a JSON status and a close.
- Every other payload is a msgpack array `[id, flags, method, args]`. A connection that got the extension `trace_context` can add a fifth element, its trace context.
- An oid or a tid is 8 bytes. A serial can also be a str whose UTF-8 bytes are 8 bytes, as ZEO's tests send it.
- An unknown method, a call before `register`, a frame that is cut or is not a message, and a frame longer than `--max-frame-size` close the connection.
- The server ignores the heartbeat, id -1, except that it is a frame for `--idle-timeout`.

## Compatibility matrix

The methods that the `ZEOStorage` of ZEO 6 exposes, and the ZOE extensions:

| Group | Methods | Status | Notes |
| --- | --- | --- | --- |
| handshake | `register`, `get_info`, `set_client_label`, `ping`, `server_status`, `lastTransaction` | as ZEO | `register(storage_id, read_only)` selects a named storage. A ZOE client adds a third argument: the extensions that it declares. A name that the server does not serve fails with `ValueError`. A register to write on a read-only store fails with `ReadOnlyError`, and the connection stays open. `get_info` gives `supportsUndo` true, `supports_record_iternext` true, the interfaces of `zoe.Storage`, and `extensions` for a connection that declared some. `server_status` has the keys of ZEO 6 in its order. `set_client_label` is accepted and not kept. |
| reads | `loadBefore`, `loadSerial`, `getTid`, `getInvalidations`, `history`, `record_iternext` | as ZEO | `getInvalidations(tid)` reads the transaction index, so a restart keeps the answer. It gives nil past `--invalidation-queue-size` transactions, unless the tid is younger than `--invalidation-age`. `get_size_info`, which a stock server lacks, gives `{length, size}`. |
| commit | `tpc_begin`, `storea`, `vote`, `tpc_finish`, `tpc_abort`, `checkCurrentSerialInTransaction`, `deleteObject`, `new_oids` | as ZEO | `vote` returns the oid of each conflict that the server resolved (tiers 1 and 2), and a map `{oid, serials, data}` for each conflict that the client resolves (tier 3). The client votes again with the resolved states. A stale read-current serial fails the vote with `ReadConflictError`. `tpc_finish` replies only after the commit is durable. |
| blobs | `storeBlobStart`, `storeBlobChunk`, `storeBlobEnd`, `storeBlobShared`, `sendBlob` | as ZEO | `storeBlobShared` needs `--shared-blob-dir`. `sendBlob` checks the size and the hash of the blob before it sends it, in chunks of 64 KiB. |
| copy and backup | `restorea`, `iterator_start`, `iterator_next`, `iterator_record_start`, `iterator_record_next`, `iterator_gc` | as ZEO | For `zodbconvert`, backups and `zodbverify`. A connection keeps at most 1,000 open iterators. |
| maintenance | `pack` | as ZEO | Online: the other calls go on while it runs. A read-only connection packs, as in ZEO, and a read-only store refuses. |
| undo | `undo`, `undoa`, `undoInfo`, `undoLog` | as ZEO | An undo that no tier merges fails the vote with `MultipleUndoErrors`. |
| extensions | `sequence_next`, `kv_get`, `kv_scan`, `kv_generation`, `kv_set`, `kv_delete`, `lock_intent`, `unlock_intent` | ZOE only | Only after the client declared the extension in `register`. Another connection gets `ValueError`, and stays open. |

After each commit, every other client of the storage gets one `invalidateTransaction(tid, oids)`, also with an empty list, as ZEO 6.2 does. The client that committed gets `info {length, size}`, then the reply of its `tpc_finish`, and no invalidation.

## Extensions

A client declares the extensions that it wants as a third argument of `register`: `register(storage_id, read_only, ["sequences", "kv"])`. The server accepts the names that it serves, once each, in order, and leaves out the others without an error. `get_info` gives the accepted names under the key `extensions`. A third argument that is not a list of strings fails `register` with `TypeError`, and the connection stays open and unregistered. A declaration holds for its connection only.

| Extension | Methods | What it does |
| --- | --- | --- |
| `sequences` | `sequence_next(name)` | The next value of the sequence `name` of the store, an int: unique and strictly increasing per store and per name, from 1. It runs outside the transaction, so an abort keeps the value. A restart of the server skips at most 100 values of a name. |
| `kv` | `kv_get(flavour, name, key)`, `kv_scan(flavour, name, start, stop, limit)`, `kv_generation(name)`, `kv_set(flavour, name, key, value, ttl)`, `kv_delete(flavour, name, key)` | The KV namespaces of the store (ADR 0043). `flavour` is `"ephemeral"` or `"durable"`. The reads run outside the transaction. `kv_set` and `kv_delete` have no reply, stage into the open transaction, and an error fails the vote. A scan gives at most 1,000 pairs. |
| `lock_intent` | `lock_intent(oids)`, `unlock_intent()` | The early write-intent locks (ADR 0044, part B). `lock_intent` takes the lock of each oid in ascending order, within the lock timeout, and returns the current serial of each one. The next `tpc_begin` of the session adopts the locks. A session that holds them 60 seconds without a `tpc_begin` loses them. |
| `trace_context` | none | Each message can carry a fifth element, a map whose key `traceparent` holds a W3C traceparent. The span of the call joins the trace of the client. A fifth element on a connection without the extension closes the connection. |
| `server_writes` | none | The client promises to give every Connection of its process the oids of its commit, also an oid that the vote wrote without a store. Only then can the split merge write in the vote the new bucket of the other side (ADR 0048). The native client of `zoe.zeo.ClientStorage` declares it. |

`zoe.zeo.ClientStorage` declares `sequences`, `lock_intent` and `kv`, `trace_context` when the package `opentelemetry` is installed, and `server_writes` with the native client.
