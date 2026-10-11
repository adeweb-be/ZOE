# The ZEO 6 wire contract, recorded from a stock server

## Question

What do a stock ZEO 6 server and an unmodified `ZEO.ClientStorage` send each other over msgpack, for each method of the protocol compatibility matrix and each call from the server? Where are the protocol tables of the specification silent, and where do they differ from the stock code?

## Method

- Versions: ZEO 6.2 and ZODB 6.3 from `python/uv.lock`, msgpack 1.2.3, CPython 3.13. The server is `runzeo` in a child process, with `msgpack true`, on a FileStorage with a blob directory. The clients are `ZEO.ClientStorage` with no change.
- A TCP proxy between them (`zoe.testing.wire.Recorder`) cuts the byte stream into frames and decodes each frame as ZEO does (`msgpack.unpackb(raw=False, use_list=False)`). It records a frame before it forwards it.
- A driver (`zoe.testing.wire_scenarios`) runs seven scenarios. Each one writes a trace, one JSON line per frame: `connection`, `direction`, `id`, `flags`, `method`, `args` (Python reprs) and `raw` (the payload, in hex). The traces are in [`zeo-wire/`](zeo-wire/):

| Trace | What it does |
| --- | --- |
| `plain.jsonl` | two clients; handshake, reads, commits, invalidations, the five error forms, a conflict that the server resolves, blob upload and download, the iterators and `iterator_gc`, pack |
| `client-resolution.jsonl` | server with `client-conflict-resolution`: the conflict dicts of the vote and the second vote round |
| `copy.jsonl` | `copyTransactionsFrom`: `tpc_begin` with a tid, `restorea` |
| `shared-blobs.jsonl` | a shared blob directory: `storeBlobShared` |
| `read-only.jsonl` | a read-only server and `read_only_fallback`: `ReadOnlyError` on `register` and on `new_oids` |
| `unregistered.jsonl` | a call of `get_size_info`, and the `ruok` probe |
| `heartbeat.jsonl` | an idle client: the heartbeat frame |

The code (the proxy, the server helper and the driver) stays on the branch `card/ZOE-049`. Run it again with `python -m zoe.testing.wire_scenarios OUT [scenario...]`.

Below, `bin8` is a msgpack bin of 8 bytes (an oid or a tid), `bin` a bin of any length, `str` a msgpack str, `[...]` an array and `{...}` a map.

## Framing and handshake

- Each frame is a 4-byte big-endian unsigned length, then the payload. There is no other envelope.
- The server speaks first. Its first payload is the protocol, as raw bytes, not msgpack: `M5` (`M` for msgpack, `5` for the version). With pickle it is `Z5`.
- The client answers with its own protocol payload, raw: `M5`. The server closes the connection on a version it does not know.
- Every later payload is one msgpack array of four items: `[message_id, flags, method, args]`. `args` is an array, except in a reply.
- A client can send `ruok` as its protocol payload. The server then sends its own `M5` (it sends it on connect), then one JSON payload (not msgpack), `{"<storage id>": <server_status map>}`, and closes. `ruok` is not a method.

The handshake of a client, in `plain.jsonl`:

| From | id | flags | method | args | reply |
| --- | --- | --- | --- | --- | --- |
| server | | | | raw `M5` | |
| client | | | | raw `M5` | |
| client | 1 | `false` | `register` | `["1", 0]` | `bin8`, the last tid |
| client | 2 | `false` | `get_info` | `[]` | the info map, below |
| client | 0 | `true` | `set_client_label` | `[str]` | none |

- `register(storage_id, read_only)`: the storage id is a str. `read_only` is the int `0` here (the default of `ClientStorage` is `0`), and `True` or `False` in other paths. A server must accept both.
- When `register` returns nil, the client calls `lastTransaction` (`[]` → `bin8`). A stock server never returns nil, so a stock pair never sends `lastTransaction` in the handshake.
- When the client has a cache whose last tid is older than the tid of `register`, it calls `getInvalidations([tid])` before `get_info`. The reply is `[tid, [oid, ...]]`, or nil when the server cannot give the full list. Nil makes the client clear its cache.
- With `read_only_fallback`, a refused `register` (`ReadOnlyError`) is followed by `register(["1", true])` on the same connection.
- A call of any method other than `register` before `register` succeeds closes the connection (from the code; not recorded).

## Message ids and flags

| Message | id | flags |
| --- | --- | --- |
| a call of the client that waits for a reply | an int, 1 for the first call of a connection, then +1 | `false` (a msgpack bool) |
| `loadBefore` | `[oid, tid]`, an array of two `bin8` | `false` |
| an asynchronous call of the client | `0` | `true` |
| a call of the server (`invalidateTransaction`, `info`, `receiveBlob*`) | `0` | `true` |
| a reply | the id of the call, unchanged | the int `0` |
| an error reply | the id of the call, unchanged | the int `2` |
| the heartbeat of the client | `-1` | the int `0`, method `".reply"`, args nil |

- The client sends a bool as the flags of its calls, and the server sends an int in its replies. The server only tests the flags for truth. A server must accept a bool and an int there.
- The reply to `loadBefore` must carry the id array as it came, with bin items. The client finds its waiting call by the tuple `(oid, tid)`. A reply with another id leaves the call waiting forever. Two loads of the same `(oid, tid)` share one message.
- The client sends a heartbeat every 60 s, busy or idle (`heartbeat.jsonl`). The server ignores id `-1`. The server sends no heartbeat.
- Replies come in the order that the server finishes the calls. `vote` and `pack` can reply later than calls that came after them.

## Methods of the client

All calls below are from `plain.jsonl` unless the row says otherwise. "Async" means flags `true`, id `0` and no reply.

| Method | args | Reply |
| --- | --- | --- |
| `ping` | `[]` | nil |
| `server_status` | `[]` | map with str keys: `connections`, `loads`, `stores`, `commits`, `aborts`, `active_txns`, `conflicts`, `conflicts_resolved` (ints), `lock_time` (nil or float), `start` (str, `time.ctime()`), `waiting` (int), `timeout-thread-is-alive` (`"stub"` or a bool), `last-transaction` (str, 16 hex digits) |
| `get_info` | `[]` | `{length: int, size: int, name: str, supportsUndo: bool, supports_record_iternext: bool, interfaces: [[module, name], ...]}` |
| `lastTransaction` | `[]` | `bin8` |
| `new_oids` | `[]` (or `[n]`) | array of `n` `bin8`, `n` capped at 100, 100 by default |
| `loadBefore` | `[oid, tid]` | `[data, start, end]` with `end` nil for the current record; nil when no record is older than `tid` |
| `loadSerial` | `[oid, serial]` | `bin`, the data |
| `getTid` | `[oid]` | `bin8` |
| `history` | `[oid, size]` | array of maps: `time` (float), `user_name` (bin), `description` (bin), `tid` (bin8), `size` (int), and the keys of the transaction extension, merged into the same map |
| `record_iternext` | `[next or nil]` | `[oid, tid, data, next_oid or nil]` |
| `getInvalidations` | `[tid]` | `[last_tid, [oid, ...]]`, or nil |
| `tpc_begin` | `[txn_id, user, description, extension, tid or nil, status]` | async |
| `storea` | `[oid, serial, data, txn_id]` | async |
| `checkCurrentSerialInTransaction` | `[oid, serial, txn_id]` | async; a stale serial fails at `vote` |
| `deleteObject` | `[oid, serial, txn_id]` | async |
| `restorea` (`copy.jsonl`) | `[oid, serial, data, prev_txn or nil, txn_id]` | async |
| `vote` | `[txn_id]` | array: empty, or the oids that the server resolved, or (with client conflict resolution) the conflict maps |
| `tpc_finish` | `[txn_id]` | `bin8`, the committed tid |
| `tpc_abort` | `[txn_id]` | nil, also when the server has no transaction |
| `storeBlobStart` | `[]` | async |
| `storeBlobChunk` | `[bin]`, at most 59,000 bytes | async |
| `storeBlobEnd` | `[oid, serial, data, txn_id]` | async |
| `storeBlobShared` (`shared-blobs.jsonl`) | `[oid, serial, data, filename, txn_id]`; `filename` is a str, the base name of a file in the oid directory, ending in `.tmp-` | async |
| `sendBlob` | `[oid, serial]` | nil, after the `receiveBlob*` calls |
| `iterator_start` | `[start or nil, stop or nil]` | int, the iterator id |
| `iterator_next` | `[iid]` | `[tid, status, user, description, extension]`, nil at the end |
| `iterator_record_start` | `[txn_iid, tid]` | int, a new iterator id |
| `iterator_record_next` | `[iid]` | `[oid, tid, data, data_txn or nil]`, nil at the end |
| `iterator_gc` | `[[iid, ...]]` | async |
| `pack` | `[time, wait]`, `time` a float, `wait` the int 1 | nil when the pack is done |
| `set_client_label` | `[str]` | async |
| `new_oid` | `[]` | an error reply, `["builtins.AttributeError", [str]]`: the server registers the name but has no such method |
| `get_size_info` (`unregistered.jsonl`) | `[]` | no reply: the server closes the connection |

Details:

- `txn_id` is `id(txn)` in the client: an int that can need more than 32 bits. The server only compares it for equality with the id of `tpc_begin`.
- In `tpc_begin`, `user` and `description` are bin, `status` is a str (`" "`, or the status of a copied transaction), and `tid` is nil, or a `bin8` in a copy. The extension is a msgpack map of the extension dict, not its pickle: `{}` when empty, `{"note": "spike"}` in the traces. The stock server pickles the map that it decodes. The client cannot send an extension that msgpack cannot encode.
- `storeBlob` sends `storea` for the oid first, then `storeBlobStart`, the chunks and `storeBlobEnd`, all async. The server writes the chunks to one temporary file per connection.
- The calls with flags `true` get no reply even on error. When an async call raises in the server, the server closes the connection.
- `iterator_gc` goes out when the client has dropped an iterator before its end, at the next `tpc_abort` or `tpc_finish`. The iterator ids of transactions and of records share one counter per connection. An iterator at its end is gone on the server.
- `pack` replies only when the pack is done, then the server sends `info` to every client of the storage. Without the root object (oid 0), the FileStorage pack fails with `["builtins.KeyError", [bin8]]`.

## Commit and the calls of the server

One commit of the first client, while a second client is connected (`plain.jsonl`):

| From | id | flags | method | args |
| --- | --- | --- | --- | --- |
| client 1 | 0 | `true` | `tpc_begin` | `[txn_id, bin, bin, {...}, nil, " "]` |
| client 1 | 0 | `true` | `storea` | `[oid, serial, data, txn_id]` |
| client 1 | 6 | `false` | `vote` | `[txn_id]` |
| server → 1 | 6 | `0` | `.reply` | `[]` |
| client 1 | 7 | `false` | `tpc_finish` | `[txn_id]` |
| server → 1 | 0 | `true` | `info` | `[{length: int, size: int}]` |
| server → 1 | 7 | `0` | `.reply` | `bin8` |
| server → 2 | 0 | `true` | `invalidateTransaction` | `[tid, [oid, ...]]` |

- The committing client gets `info` before the reply of `tpc_finish`, and it gets no `invalidateTransaction`. The other clients of the storage get `invalidateTransaction`.
- `info` carries a map; the client merges it into its info dict.
- `sendBlob` (connection 3 of `plain.jsonl`): the server sends `receiveBlobStart [oid, serial]`, one `receiveBlobChunk [oid, serial, bin]` per 59,000 bytes, `receiveBlobStop [oid, serial]`, then the reply nil. The client needs the file before the reply arrives, so this order is part of the contract.

## Errors

An error reply has flags `2` and args `[class, value]`. `class` is the str `module.Name` of the exception. `value` is the `__dict__` of the exception as a map when it is not empty, else its `args` as an array.

| Exception | Recorded reply | Where |
| --- | --- | --- |
| `POSKeyError` | `["ZODB.POSException.POSKeyError", [oid]]` | `loadBefore` of an unknown oid |
| `ConflictError` | `["ZODB.POSException.ConflictError", {message: "database conflict error", oid: bin8, class_name: str, serials: [bin8, bin8]}]` | `vote` after a `storea` with a stale serial |
| `ReadConflictError` | `["ZODB.POSException.ReadConflictError", {message: "database read conflict error", oid: bin8, class_name: nil, serials: [bin8, bin8]}]` | `vote` after `checkCurrentSerialInTransaction` with a stale serial |
| `ReadOnlyError` | `["ZODB.POSException.ReadOnlyError", []]` | `register` with `read_only` false on a read-only server; `new_oids` on a read-only connection |
| `StorageTransactionError` | `["ZODB.POSException.StorageTransactionError", [nil, 1]]` | `vote` with no transaction: the args are `(current txn id or nil, given txn id)` |
| `KeyError` | `["builtins.KeyError", [bin8]]` | `pack` with no root object |
| `AttributeError` | `["builtins.AttributeError", [str]]` | `new_oid` |

- The `serials` of both conflicts are `[committed, read]`. The map has no `data`.
- The client builds `ConflictError` and `ReadConflictError` from the keys `message`, `oid` and `serials`, which it requires, and `class_name`, which it reads if present. It builds `POSKeyError`, `ReadOnlyError`, `StorageTransactionError`, `KeyError` and `TypeError` as `Class(*value)`, so their value must be an array. It also knows `BTreesConflictError` (`p1`, `p2`, `p3`, `reason`) and `MultipleUndoErrors` (`_errs`). Any other class becomes `ZEO.Exceptions.ServerException(class, value)`.
- A failed `vote` has already aborted the transaction on the server. The `tpc_abort` that follows gets nil.

## Conflicts resolved on the client

With `client-conflict-resolution` on (`client-resolution.jsonl`), a store with a stale serial does not fail the vote:

| From | id | flags | method | args |
| --- | --- | --- | --- | --- |
| client | 8 | `false` | `vote` | `[txn_id]` |
| server | 8 | `0` | `.reply` | `[{oid: bin8, serials: [committed, read], data: bin}]` |
| client | 9 | `false` | `loadSerial` | `[oid, read]` |
| client | 10 | `false` | `loadSerial` | `[oid, committed]` |
| client | 0 | `true` | `storea` | `[oid, committed, resolved data, txn_id]` |
| client | 11 | `false` | `vote` | `[txn_id]` |
| server | 11 | `0` | `.reply` | `[]` |

- `data` is the pickle that the client stored. The client loads the two other states with `loadSerial` and runs `_p_resolveConflict` itself.
- The stock client handles these maps in any vote reply. Nothing in `register` or `get_info` turns it on: a stock client always accepts them.
- Between the two votes, the stock server aborts the transaction in its storage and releases the commit lock. The second `vote` replays every store of the transaction.
- Without this option, the server resolves the conflict itself when it can, and `vote` returns the resolved oids: `[bin8]` in `plain.jsonl`.

## What the server announces

`get_info` lists in `interfaces` the `(module, name)` pairs of every interface that the storage provides, in the order of `providedBy(storage).__iro__`, each pair an array of two str. For a FileStorage with blobs: `IBlobStorageRestoreable`, `IBlobStorage`, `IStorageRestoreable`, `IStorageIteration`, `IStorageUndoable`, `IStorageCurrentRecordIteration`, `IExternalGC` and `IStorage`, all in `ZODB.interfaces`, then `zope.interface.Interface`. The client provides `IStorageRestoreable`, `IStorageIteration`, `IStorageUndoable`, `IStorageCurrentRecordIteration`, `IBlobStorage` and `IExternalGC` only when the pair is in the list. So the list decides, for example, whether ZODB gives the client blobs.

## msgpack rules that the stock client imposes

- It decodes with `raw=False`: oids, tids, pickles, blob chunks, user and description must be bin, and method names, class names, the storage id, map keys and `status` must be str. A str where the client expects bytes breaks the comparisons of oids and tids.
- It decodes with `use_list=False`: every array becomes a tuple.
- msgpack 1.x refuses map keys that are not str or bin (`strict_map_key`). A map with int keys fails to decode in the client.
- The server encodes with `use_bin_type=True`, and the client too.

## Where the specification is silent or differs

| Point | Specification | Stock ZEO 6.2 |
| --- | --- | --- |
| Frames | "length-prefixed msgpack … `[message_id, async_flag, method, args]`" | the first frame of each side is the raw protocol `M5`, not msgpack; the flags are a bool in calls and an int (0, or 2 for an error) in replies |
| `ruok` | a handshake method | a protocol payload that replaces `M5`; the answer is JSON, then a close |
| `get_size_info` | a read method | not registered: a call closes the connection; it reaches clients only as the argument of `info` |
| Methods that the matrix does not list | silent | registered: `lastTransaction`, `loadSerial` (used by client conflict resolution), `record_iternext`, `getTid`, `new_oid` (an `AttributeError` reply), `undoa`, `undoLog`, `undoInfo` |
| Unknown method | silent | the server closes the connection, with no reply |
| Error replies | silent | flags 2, `[class, __dict__ or args]`, as in "Errors" |
| An error in an async call | silent | the server closes the connection |
| Message ids | "replies carry the message id" | ints, `[oid, tid]` for `loadBefore`, `0` for async calls, `-1` for the heartbeat; the reply must echo the id with the same msgpack types |
| Heartbeat | silent | the client sends `[-1, 0, ".reply", nil]` every 60 s; the server ignores it |
| `vote` with client resolution | "for a client that negotiated `client_resolve`, the committed states of the unresolved ones for a second vote round", "with the locks still held" (section 5) | a server option; the conflict map carries the client's own pickle (`data`), not the committed state; a stock client accepts the maps with no negotiation; the stock server releases the commit lock between the rounds |
| Invalidations | "each connection task forwards one `invalidateTransaction` message to its client" | the committing client gets none; it gets `info` before its `tpc_finish` reply |
| `getInvalidations` | "`None` when the gap is longer than `invalidation-queue-size` (default 100,000 tids)" | nil, as written; the stock default queue is 100 transactions; for a tid older than the queue, nil |
| Extension of `tpc_begin` | silent | a msgpack map of the dict, which the server pickles; never the pickle bytes |
| `register` | `register(storage_id, read_only)`, and the client declares `extensions` there | two args only in the stock client; `read_only` arrives as an int or a bool; a stock server has no third argument |
| `get_info` | advertises `supportsUndo` and `extensions` | `length`, `size`, `name`, `supportsUndo`, `supports_record_iternext`, `interfaces`; no `extensions` key; the `interfaces` list turns client features on |
| Blob chunks | "`sendBlob` streams the file in 64 KiB chunks" | the stock server and client use 59,000-byte chunks; the client accepts any size |
| `iterator_gc`, `iterator_record_*` | listed | the shapes above; the ids share one counter per connection |

## Proposed subsection for the specification: "Wire contract"

To add under "Server mode", after "Connection handling":

> **Wire contract.** A frame is a 4-byte big-endian length and a payload. The server sends `M5` first, as raw bytes; the client answers `M5`, or `ruok` for a JSON status and a close. Every other payload is a msgpack array `[id, flags, method, args]`, encoded with bin types and decoded with str keys and arrays as tuples. A call that waits has an int id (or `[oid, tid]` for `loadBefore`) and flags false; an asynchronous call, in either direction, has id 0 and flags true and gets no reply. A reply echoes the id with `.reply` and flags 0, or flags 2 and `[class, __dict__ or args]` for an error. The server ignores id -1, the heartbeat of the client. An unknown method, a call before `register`, and an error in an asynchronous call close the connection. The method shapes are those of `docs/spikes/zeo-wire.md`, and the traces in `docs/spikes/zeo-wire/` are the reference for the protocol tests of `zoe serve`.

## Verdict

The stock contract is small and regular, and the traces fix every shape that the server cards need. Four points change what the server cards must build:

1. `ruok` and `get_size_info` are not methods. The matrix must move `ruok` to the handshake payload and drop `get_size_info` as a client method. A server that answers `get_size_info` is harmless; a client never sends it.
2. The matrix must add `lastTransaction`, `loadSerial`, `record_iternext` and `getTid`. `loadSerial` is on the path of every conflict that the client resolves.
3. The error reply, the closes and the id forms above are the contract. A test must check them byte for byte against the traces.
4. Client conflict resolution already exists in stock ZEO, with no negotiation. Tier 4 of section 5 can use it for stock clients. The difference is that the stock server releases the lock between the rounds.

## Not tested

- The pickle protocol (`Z5`) and protocols older than 5.
- `prefetch`, and replies out of order under load.
- Undo (`undoa`, `undoLog`, `undoInfo`), which is out of scope.
- A `vote` that waits for the commit lock (a delayed reply), and `transaction_timeout`.
- TLS.
- `BTreesConflictError` and `MultipleUndoErrors` replies; their shapes come from the client code only.
- Python 3.12, 3.14 and 3.14t: the traces come from 3.13 only.

## The acceptance test

`python/zoedb/tests/server/test_wire_capture.py` cannot pass against stock ZEO 6.2 and an unmodified client. The second test passes. The first test fails, and the question is on the card:

1. `storage._server.get_size_info()` and `storage._server.getInvalidations(tid)`: `storage._server` is a `ZEO.asyncio.client.ClientThread`, which has neither method (`AttributeError`). `storage._call("getInvalidations", tid)` works.
2. `get_size_info` is not a registered method. Through `storage._call`, the server closes the connection and the call raises `ClientDisconnected`.
3. A stock pair never sends `lastTransaction` in that test, and `list(storage.iterator())` sends neither `iterator_record_start`, `iterator_record_next` nor `iterator_gc`. The coverage assertion fails for these four.
4. After the stale-serial `ConflictError`, the test does not call `tpc_abort`. The commit lock of `ClientStorage` is a `threading.Lock` that `tpc_begin` takes, so the next `commit` in the same thread waits forever.
5. `storage.pack` fails with `KeyError`, because the test never stores the root object (oid 0).

`zoe.testing.wire_scenarios.plain` shows a sequence with stock calls that covers every method of the test.
