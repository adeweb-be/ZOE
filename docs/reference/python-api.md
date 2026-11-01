# Python API

The `zoedb` wheel gives the package `zoe`. The extension module `zoe._zoe` holds the Rust code. This page lists the public names of each module. The docstrings of the modules give the details.

| Module | What it gives |
| --- | --- |
| `zoe` | `Storage`, `WrongThread`, `__version__` |
| `zoe.storage` | the module of `zoe.Storage` and `zoe.WrongThread` |
| `zoe.zeo` | `ClientStorage`, `Sequence`, `EXTENSIONS` |
| `zoe.kv` | `Namespace`, `scan_all`, `SCAN_LIMIT`, `MAX_KEY`, `MAX_TTL` |
| `zoe.policy` | `MergePolicy`, `validate`, `install`, `read`, and the constants of a merge policy |
| `zoe.zodb` | `LockingConnection`, `hot_classes`, `install` |
| `zoe.config` | the ZConfig data types of `<zoe>` and `<zoeclient>`, and `resolve_uri` of the `zoe://` URI (see [zope.conf sections](zope-conf.md)) |
| `zoe.serve` | `main`, the entry point of `python -m zoe.serve -C FILE` |
| `zoe.testing` | the test layers and `ZoeServer` |
| `zoe.plone` | the Plone integration, with the extra `zoedb[plone]` |

`import zoe` does not import ZODB. `zoe.Storage` and `zoe.WrongThread` import `zoe.storage`, and so ZODB, at their first use.

## `zoe.Storage`

```python
zoe.Storage(
    path, *, read_only=False, lock_timeout=None, blob_dir=None, pack_gc=True,
    compression=None, structural_merges=None, structural_merge_classes=None,
    posting_dicts=None, resolution_log=None, early_locks=False,
    early_lock_classes=None,
)
```

A ZODB storage in the directory `path`, in this process. The open creates the directory when it is missing, except for a read-only storage. The `<zoe>` section and the `zoe://` URI open it with the same values.

| Argument | Default | Value |
| --- | --- | --- |
| `path` | necessary | The directory of the store. |
| `read_only` | `False` | Refuse `tpc_begin`, `new_oid`, `store`, `pack`, `restore`, `storeBlob`, `restoreBlob` and `temporaryDirectory` with `ReadOnlyError`. |
| `lock_timeout` | `None`, the default of the store: 10 seconds | The longest time, in seconds, that a vote waits for the lock of an object. A longer wait raises `ConflictError`. |
| `blob_dir` | `None`, `blobs` in `path` | The directory of the blob files, in ZODB's BushyLayout. The store does not record it. A store with blobs refuses a `blob_dir` without the layout file of its blobs with `StorageError`. |
| `pack_gc` | `True` | With `False`, a pack removes old revisions only, as the `pack-gc` of FileStorage does. |
| `compression` | `None`, the default of the store: `"none"` | `"none"`, `"zstd"` or `"zstd:LEVEL"`. Another value raises `ValueError`. |
| `structural_merges` | `None`, `"on"` | The mode of tier 1: `"on"`, `"shadow"` or `"off"`. |
| `structural_merge_classes` | `None` | A dict of class, as `module.name`, to its own mode of tier 1. |
| `posting_dicts` | `None`, `"off"` | The mode of the posting-dict rule and the first-key rule of ADR 0042. |
| `resolution_log` | `None` | A file that gets one JSON line for each result of tier 1. A shadow mode without it raises `ValueError`. |
| `early_locks` | `False` | Turn on the early write-intent locks of ADR 0044, part B, for the classes of `early_lock_classes`. |
| `early_lock_classes` | `None` | The hot classes, each as `module.name`. Each must be a leaf record. A class of tier 1 raises `ValueError`. |

It implements `IBlobStorageRestoreable`, `IStorageCurrentRecordIteration`, `IStorageIteration`, `IStorageUndoable` and `IExternalGC`, and the interfaces that they extend: `IStorage`, `IBlobStorage` and `IStorageRestoreable`. Its methods beyond these interfaces:

| Method | What it does |
| --- | --- |
| `sequence(name)` | Return the sequence `name` of the store. Its `next()` gives an `int`, unique and strictly increasing per store and per name. It is not part of a transaction, so an abort does not give the value back. Each reopen can skip up to 100 values. |
| `kv_supported()` | Return `True`: a `zoe.Storage` has KV namespaces (ADR 0043). |
| `kv_get(flavour, name, key)` | Return the last committed value of `key` in the namespace `name` of `flavour`, `"ephemeral"` or `"durable"`, or `None`. |
| `kv_scan(flavour, name, start=None, stop=None, limit=1000)` | Return the pairs `(key, value)` in the byte order of the keys, from `start` (included) to `stop` (excluded), at most `limit` and at most 1,000. |
| `kv_generation(name)` | Return the generation of the durable namespace `name`: the 8 bytes of the tid of the last commit that wrote it. |
| `kv_set(flavour, name, key, value, ttl, transaction)` | Stage a write in `transaction`. An ephemeral key needs `ttl`, in seconds, more than 0 and at most 2^32 - 1. A durable key takes `None`. |
| `kv_delete(flavour, name, key, transaction)` | Stage a delete in `transaction`. |
| `lock_intent(oids, owner=None)` | Take the early write-intent lock of each oid, in ascending order, and return their current serials. |
| `unlock_intent(owner=None)` | Release every early write-intent lock of the owner. |
| `new_intent_owner()` | Return a new owner of early write-intent locks. |
| `bind_intent_owner(owner)` | Make `owner` the owner of the intent locks that the next `tpc_begin` of the calling thread adopts. `None` gives the default owner back. |
| `cleanup()` | Close the storage and remove the store directory and the blob directory, as `cleanup` of FileStorage does. |

Use `zoe.kv.Namespace` for the KV namespaces in an application: it joins the transaction of a connection.

The open installs handlers of SIGTERM and SIGHUP that close the stores of the process, each only when the action of the signal is the default (ADR 0031).

## `zoe.WrongThread`

A subclass of `StorageTransactionError`. Only the thread that calls `tpc_begin` can call `store`, `checkCurrentSerialInTransaction` and `tpc_vote` in the transaction. Another thread gets `WrongThread`, and the call changes nothing. Any thread can call `tpc_finish` and `tpc_abort`.

## `zoe.zeo`

| Name | What it is |
| --- | --- |
| `ClientStorage(*args, native=None, early_locks=False, early_lock_classes=None, **kw)` | A subclass of `ZEO.ClientStorage.ClientStorage`, with the same arguments. `<zoeclient>` opens it. By default, the native client of the Rust crate `zoe-client` runs inside. `native=False` keeps the asyncio client of ZEO. A subclass of `ClientStorage` keeps the asyncio client of ZEO, and `native=True` on it raises `TypeError`. `early_locks=True` needs the native client. |
| `ClientStorage.sequence(name)` | The sequence `name` of the store, a `Sequence`. It needs a server that accepted the extension `sequences`. |
| `ClientStorage.kv_supported()`, `kv_get`, `kv_scan`, `kv_generation`, `kv_set`, `kv_delete` | The KV namespaces of the store, as on `zoe.Storage`. They need a server that accepted the extension `kv`. Without it, each raises `ZODB.POSException.Unsupported`, and `kv_supported()` returns `False`. |
| `Sequence(storage, name)` | A named counter of the store of a `ClientStorage`. `next()` returns the next value. Without the extension `sequences`, `next()` raises `Unsupported`. |
| `EXTENSIONS` | The extensions that each connection declares: `sequences`, `lock_intent`, `kv`, and `trace_context` when the package `opentelemetry` is installed. The native client also declares `server_writes`. |

The native client speaks only the msgpack protocol `M5`. A stock ZEO server needs `msgpack true`. When the caller gives no `native` and the open with `wait` finds a stock ZEO server that speaks only `Z5`, the instance takes the asyncio client of ZEO and logs a warning.

## `zoe.kv`

`Namespace(connection, flavour, name)` is the namespace `name` of `flavour`, `"ephemeral"` or `"durable"`, of the store of the database of `connection`. The store is a `zoe.Storage`, a `zoe.zeo.ClientStorage` whose server accepted `kv`, or a `DemoStorage` whose chain of bases ends at one of them. Another storage raises `Unsupported`. A key is bytes, or a str that the namespace writes as UTF-8. A value is bytes.

| Method | What it does |
| --- | --- |
| `get(key, default=None)` | Return the value of `key`, or `default`. A write of the current transaction comes first. |
| `scan(start=None, stop=None, limit=1000)` | Return the pairs `(key, value)` in the byte order of the keys, with the writes of the current transaction, at most `limit` and at most 1,000. |
| `generation()` | Return the generation of a durable namespace: the 8 bytes of the tid of the last commit that wrote it. |
| `set(key, value, ttl=None)` | Write `value` in the current transaction. An ephemeral key needs `ttl`, in seconds. A durable key takes none. |
| `delete(key)` | Delete `key` in the current transaction. |

The namespaces keep no history: a read gives the last committed value, not the value at the snapshot of the connection. The writes commit with the objects of the transaction, and an abort drops them.

## `zoe.policy`

The merge policy of a store (ADR 0046) is the persistent object `root['zoe.merge_policy']`. It holds plain values only, so the core reads it in Rust, in-process and in `zoe serve`.

| Name | What it is |
| --- | --- |
| `MergePolicy` | The persistent class of the policy. Write it with `install`. |
| `validate(policy)` | Return a copy of `policy` with plain values only, or raise `ValueError` when it breaks the rules of a policy. |
| `install(connection, policy)` | Write `policy` as the merge policy of the database of `connection`, in its current transaction. Return its new version. |
| `read(connection)` | Return the merge policy as a dict, with its `version`, or `None`. |
| `NAME` | `"zoe.merge_policy"`, the key of the policy in the root. |
| `MODES` | `("on", "shadow", "off")`. |
| `STRATEGIES` | `("strict", "lww", "max", "sum", "union", "append", "keys")`. |
| `SWITCHES` | `("structural_merges", "posting_dicts", "split_merges", "folder_orders")`, the switches of the policy. |
| `ORDER_CLASS` | `"persistent.list.PersistentList"`, the class of the order of a folder of plone.folder. |
| `ENTRY_KEYS` | `("tier1", "tier2", "default", "fields", "coupled", "blocked", "options")`, the keys of the entry of a class. |
| `OPTIONS` | `{"append": ("state",)}`, the options of each strategy. |

A policy is a dict with the key `classes` and, as switches, `structural_merges`, `posting_dicts`, `split_merges` and `folder_orders`. A switch of the policy wins over the key of the same name of the store. `split_merges` and `folder_orders` are off without the switch.

## `zoe.zodb`

| Name | What it is |
| --- | --- |
| `LockingConnection` | A ZODB `Connection` that takes the early write-intent locks of the hot classes of its storage. |
| `hot_classes(early_locks, classes)` | Return the hot classes of a storage, as a frozenset of `module.name`, or `None` when `early_locks` is false. |
| `install()` | Put the methods of `LockingConnection` on `ZODB.Connection.Connection`, once per process. A storage with early locks calls it at its open. |

## `zoe.testing`

Test support. It needs the test extra of ZODB, and ZEO for the layers of the server.

| Name | What it is |
| --- | --- |
| `STORAGE_LAYER`, `StorageLayer` | A layer of zope.testrunner. In each test, the default storage of ZODB is a `zoe.Storage` in a new directory. |
| `StorageTestCase` | The base class of the storage suites of ZODB. |
| `ZoeServer(path=None, args=(), env=None, log=None)` | A `zoe serve` process for a test. `ZoeServer.binary()` gives the binary: `ZOE_BINARY`, else the debug build of the repository. `start`, `stop` and `kill` drive it. As a context manager, it starts the server and kills it at the end. |
| `ZEO_LAYER`, `ZeoLayer` | A layer in which the test forker of ZEO starts `zoe serve` in place of a ZEO server. |
| `native_client(address, **kw)` | Open a `zoe.zeo.ClientStorage` with the native client inside. |
| `NATIVE_LAYER`, `NativeLayer` | A layer in which `ZEO.ClientStorage.ClientStorage` and `<zoeclient>` open the native client. |

## `zoe.plone`

The Plone integration, with the extra `zoedb[plone]`. Plone loads its `configure.zcml` through the `plone.autoinclude` entry point of the wheel. The profiles are `zoe.plone:default` and `zoe.plone:uninstall`. The behaviour `zoe.plone.history` gives a type the append-only workflow history. The view `@@zoe-indexing` on the site gives the status of the queue of queued indexing.

| Module | Public names |
| --- | --- |
| `zoe.plone` | `backends_from_environment()`: install the backends that `ZOE_PLONE_BACKENDS` names. |
| `zoe.plone.uidgenerator` | `install`, `database_opened`, `next_id`, `sequence_name`, `SEQUENCE`, `BLOCK`: the ids of `portal_uidgenerator` from a sequence of the store. |
| `zoe.plone.report` | `mark`, `request_started`, `KEY`: the mark `zoe.request_method` of the writes-on-GET report. |
| `zoe.plone.merges` | `derive`, `update_policy`, `import_merge_policy`, `compose`, `settle`, `apply_overrides`, `class_entries`, `invariant_fields`, `lowest_mode`, `fti_modified`, `DEFAULT_MODE`, `FIELDS`, `OVERRIDES`: the merge policy of a site, from its Dexterity types (ADR 0046). |
| `zoe.plone.history` | `WorkflowHistory`, `IAppendOnlyHistory`, `ensure`, `move`, `move_histories`, `restore`, `restore_histories`, `import_histories`, `uninstall`, `policy`, `ENTRY`, `BATCH`: the append-only workflow history. |
| `zoe.plone.properties` | `PrincipalProperties`, `install`, `installed`, `uninstall`, `move`, `move_properties`, `restore`, `restore_properties`, `import_properties`, `policy`, `ENTRY`, `MARK`, `BATCH`: the properties of the members and groups, one record for each principal. |
| `zoe.plone.locking` | `install`, `installed`, `uninstall`, `NAMESPACE`: the locks of plone.locking in an ephemeral namespace. `locking.zcml` or `ZOE_PLONE_BACKENDS=locking` installs it. |
| `zoe.plone.indexing` | `Indexer`, `install`, `installed`, `uninstall`, `status`, `QUEUE`, `COOKIE`, `LEASE`, `FAILED`, `BATCH_SIZE`: queued indexing (ADR 0047). `indexing.zcml` or `ZOE_PLONE_BACKENDS=indexing` installs it. |
| `zoe.plone.registry` | `MovedRecords`, `move`, `move_back`, `copy_rows`, `reset`, `install`, `installed`, `uninstall`, `install_guard`, `uninstall_guard`: the registry backend (ADR 0043, part B). `registry.zcml` or `ZOE_PLONE_BACKENDS=registry` installs it. |
| `zoe.plone.registry_rows` | `Cache`, `dumps`, `loads`, `encode_marker`, `decode_marker`, `namespace_name`, `new_id`, `record_key`, `scan_rows`, `LAYOUT`, `MARKER`, `PREFIX`: the rows of the registry backend. |
| `zoe.plone.testing` | `install(zeo=None)`, `ZOE_STORAGE`, `ZOE_SERVE`: a plone.testing layer that puts a ZOE storage under the Plone fixtures. |
