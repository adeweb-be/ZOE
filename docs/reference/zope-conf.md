# zope.conf sections and the `zoe://` URI

`%import zoe` loads the ZConfig component of the `zoedb` wheel, `component.xml`. It declares five section types: `<zoe>` and `<zoeclient>`, which a `<zodb_db>` section takes as its storage, the `<ssl>` section of `<zoeclient>`, and `<zoe-server>` with its `<storage NAME>` subsections, which `python -m zoe.serve -C FILE` reads.

The keys are in kebab-case. ZConfig keeps a `#` after a value as part of the value, so put a comment on a line of its own.

A boolean key takes `true`, `false`, `yes`, `no`, `on` or `off`, in any case: the `boolean` of ZConfig. It does not take `1`, `0`, `y`, `n`, `t` or `f`, which the options of `zoe` take. So `read-only 1` in `<zoe-server>` exits with 2, but `zoe serve --read-only=1` runs.

## `<zoe>`

A ZOE storage in this process: `zoe.Storage` (see [Python API](python-api.md)). It mirrors `<filestorage>`.

```
%import zoe
<zodb_db main>
  <zoe>
    path  var/zoe
    blob-dir  var/blobstorage
    lock-timeout  10s
  </zoe>
  mount-point /
</zodb_db>
```

Each key has an environment twin: `ZOE_` and the key in upper case, with `_` for `-`. One key breaks this rule: the twin of `structural-merge-class` is `ZOE_STRUCTURAL_MERGE_CLASSES`, which takes `CLASS=MODE` pairs separated by commas. The twin gives the value only when the section does not set the key. An empty value counts as not set, so the key takes its twin, then its default. The twin takes the same values as the key.

| Key | Twin | Default | Value |
| --- | --- | --- | --- |
| `path` | `ZOE_PATH` | none: the section or the twin must give it | The directory of the store. The storage creates it when it is missing. A leading `~` is the home directory. |
| `blob-dir` | `ZOE_BLOB_DIR` | `blobs` in `path` | The directory of the blob files. A leading `~` is the home directory. The store does not record it, so each open must give the same one. |
| `backend` | `ZOE_BACKEND` | `redb` | The backend of the store. `redb` is the only backend. Another value raises `ValueError`. |
| `lock-timeout` | `ZOE_LOCK_TIMEOUT` | the default of the store, 10 seconds | The longest time that a vote waits for the lock of an object, in whole seconds, 0 or more, such as `10`, `10s` or `2m`. A longer wait raises `ConflictError`. |
| `early-locks` | `ZOE_EARLY_LOCKS` | `false` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, an object of a class of `early-lock-classes` takes its early write-intent lock when its state loads (ADR 0044, part B). A second writer then waits for the lock, and reads the commit of the first one. |
| `early-lock-classes` | `ZOE_EARLY_LOCK_CLASSES` | none: no lock | The hot classes of `early-locks`, each as `module.name`, separated by spaces or commas. A hot class must be a leaf record, whose state refers to no other persistent object. A class of tier 1 raises an error. |
| `read-only` | `ZOE_READ_ONLY` | `false` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, the storage refuses writes and does not create a missing directory. |
| `pack-gc` | `ZOE_PACK_GC` | `true` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If false, a pack removes old revisions only, as the `pack-gc` of FileStorage does. The store does not record it. |
| `compression` | `ZOE_COMPRESSION` | `none` | The compression of the records that the store writes: `none`, `zstd` or `zstd:LEVEL`. `zstd` is zstd at level 3. A store reads every compression, so the key can change between opens. Another value raises `ValueError`. |
| `structural-merges` | `ZOE_STRUCTURAL_MERGES` | `on` | The mode of the structural merges (tier 1) of each class that `structural-merge-class` does not name: `on`, `shadow` or `off` (ADR 0042). A shadow mode needs `resolution-log`. |
| `structural-merge-class` | `ZOE_STRUCTURAL_MERGE_CLASSES` | none | The mode of one class of tier 1, as `CLASS MODE`, such as `BTrees.IOBTree.IOBucket shadow`. The key comes once for each class. The twin takes `CLASS=MODE` pairs separated by commas. |
| `posting-dicts` | `ZOE_POSTING_DICTS` | `off` | The mode of the posting-dict rule and the first-key rule of ADR 0042: `on`, `shadow` or `off`. A shadow mode needs `resolution-log`. |
| `resolution-log` | `ZOE_RESOLUTION_LOG` | none: no line | A file of JSON lines, one for each result of tier 1. The storage opens it in append mode and never syncs it. A leading `~` is the home directory. |

The keys `staging-memory`, `max-record-size`, `max-transaction-size` and `max-blob-size` are options of the core only. No key of `<zoe>` sets them.

## `<zoeclient>`

A client of a ZOE server: `zoe.zeo.ClientStorage`, a subclass of `ZEO.ClientStorage` (see [Python API](python-api.md)). It extends `<zeoclient>` of ZODB, so it takes every key of `<zeoclient>` with the same values. It declares the ZOE extensions to the server (see [Protocol](protocol.md)), and works as a `<zeoclient>` against a stock ZEO server. Only `prefetch-references` has an environment twin, `ZOE_PREFETCH_REFERENCES`; the other keys have none, as `<zeoclient>` has none.

The keys of `<zeoclient>`: `server` (one for each address), `storage`, `cache-size`, `name`, `client`, `var`, `min-disconnect-poll`, `max-disconnect-poll`, `wait`, `read-only`, `read-only-fallback`, `drop-cache-rather-verify`, `username`, `password`, `realm`, `blob-dir`, `shared-blob-dir`, `blob-cache-size`, `blob-cache-size-check` and `client-label`. The keys that `<zoeclient>` adds:

| Key | Default | Value |
| --- | --- | --- |
| `wait-timeout` | `30` | The longest wait for a connection to the server, in seconds, at the open and after a disconnection. A longer wait raises `ClientDisconnected`. |
| `native` | not set | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, the client runs on the native client, built on the Rust crate `zoe-client`. Against a stock ZEO server, it needs `msgpack true` on the server. If false, the asyncio client of ZEO runs inside. When the key is not set, the client is native, but a stock ZEO server that speaks only the pickle protocol `Z5` at the open, with `wait`, gives the asyncio client of ZEO and one warning. |
| `early-locks` | `false` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, an object of a class of `early-lock-classes` takes its early write-intent lock on the server when its state loads (ADR 0044, part B). It needs the native client: with `native false`, the open raises an error. A server that does not accept the extension `lock_intent` gives no early lock and one warning. |
| `early-lock-classes` | none | The hot classes of `early-locks`, each as `module.name`, separated by spaces or commas, as in `<zoe>`. |
| `prefetch-references` | `ZOE_PREFETCH_REFERENCES`, else `true` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, a load that the server serves also prefetches into the client cache the objects of the same database that its record refers to, one level deep: at most 256 objects, for a record of at most 16 KiB. The record goes to the application first. Only the native client reads it. The twin gives the value when the key is not set; an empty twin counts as not set. |

### `<ssl>`

The TLS of a `<zoeclient>` section, with the keys of the `<ssl>` section of ZEO's `<clientstorage>`. The native client reads the files itself. ZEO's client gets the context that ZEO makes from them.

ZEO's component also declares a section type `ssl`, so ZConfig refuses a configuration that imports `zoe` and `ZEO` together. In such a configuration, use `<zoeclient>` with `native false` and this `<ssl>` section in place of `<clientstorage>`: it opens ZEO's client with the same TLS.

| Key | Default | Value |
| --- | --- | --- |
| `certificate` | necessary | The PEM file of the certificate of the client. It holds the private key too when `key` is not set. |
| `key` | none | The PEM file of the private key of the certificate. |
| `password-function` | none | The dotted name of a function that returns the password of the key. Only ZEO's client reads an encrypted key: the native client refuses this key. |
| `authenticate` | the CA certificates of the system | The PEM file or the directory of the CA certificates that the certificate of the server must chain to. In a directory, the native client reads every file. Without this key, keep `check-hostname` true. |
| `check-hostname` | `true` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, the certificate of the server must be valid for the name of the server. |
| `server-hostname` | the host of the address | The name of the server for TLS. |

## `<zoe-server>`

The settings of `zoe serve`. `python -m zoe.serve -C FILE` reads the one `<zoe-server>` section of a ZConfig file, writes the settings as a [configuration file](config-file.md) that has no name on disk, and replaces itself with `zoe serve --config` on that file. So no Python stays in the process of the server, and a supervisor stops it with SIGTERM.

```
%import zoe
<zoe-server>
  listen 0.0.0.0:8100
  lock-timeout 10s
  <storage main>
    path /var/zoe/main
  </storage>
</zoe-server>
```

The keys other than `binary` are the options of `zoe serve` without `--`, with the syntax of the option (see [The zoe command](cli.md#zoe-serve)). Each option has its environment twin. The section wins over the twins: `zoe.serve` removes from the environment of the server the twin of each key that the section sets. An empty value counts as not set. A leading `~` in a key that names a file or a directory is the home directory.

An unknown key, a bad boolean, a storage without a path and a storage name that comes twice exit with 2 before the server starts. A missing binary exits with 1.

| Key | Option of `zoe serve` | Value |
| --- | --- | --- |
| `binary` | none | The `zoe` binary to run. The default is `ZOE_BINARY`, else the `zoe` binary on the PATH that is not a script. The wheel has no server. |
| `path` | `--path` | The directory of the store of the storage name `1`. |
| `blob-dir` | `--blob-dir` | The blob directory of the store of `path`. |
| `listen` | `--listen` | The address of the ZEO port, `HOST:PORT` or the path of a unix domain socket. The default is `127.0.0.1:8100`. |
| `admin-listen` | `--admin-listen` | The address of the admin port. Without it, the server opens no admin port. |
| `invalidation-queue-size` | `--invalidation-queue-size` | The most transactions that `getInvalidations` covers. |
| `invalidation-age` | `--invalidation-age` | The age under which `getInvalidations` covers every transaction, in whole seconds. 0, the default, turns it off. |
| `client-queue-size` | `--client-queue-size` | The most messages that wait for one client. |
| `cache-size` | `--cache-size` | The page cache of each store, such as `512MB`. The default is 1 GiB. |
| `compression` | `--compression` | `none`, `zstd` or `zstd:LEVEL`. The default is `none`. |
| `lock-timeout` | `--lock-timeout` | The longest lock wait of a vote, in whole seconds. |
| `max-frame-size` | `--max-frame-size` | The longest frame that a client can send, in bytes. |
| `shutdown-timeout` | `--shutdown-timeout` | The longest time of a stop, in whole seconds. |
| `idle-timeout` | `--idle-timeout` | The longest idle time of a connection, in whole seconds. 0 closes no idle connection. |
| `transaction-timeout` | `--transaction-timeout` | The longest time of a transaction after its vote, in whole seconds. 0 turns it off. |
| `blocking-threads` | `--blocking-threads` | The threads of the read pool. The default is the number of cores. |
| `lock-warn` | `--lock-warn` | Log each lock held longer than this, in whole seconds. |
| `transaction-warn` | `--transaction-warn` | Log each transaction open longer than this, in whole seconds. |
| `log-format` | `--log-format` | `text` or `json`. |
| `read-only` | `--read-only` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, every store serves reads only. The default is false. |
| `pack-gc` | `--pack-gc` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If false, a pack of each store removes old revisions only. The default is true. |
| `shared-blob-dir` | `--shared-blob-dir` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, the server accepts `storeBlobShared`. The default is false. |
| `structural-merges` | `--structural-merges` | `on`, the default, `shadow` or `off`. |
| `structural-merge-class` | `--structural-merge-class` | The mode of one class, as `CLASS MODE`. The key comes once for each class. It becomes the table `[structural-merge-classes]`. |
| `posting-dicts` | `--posting-dicts` | `on`, `shadow` or `off`, the default. |
| `resolution-log` | `--resolution-log` | A file of JSON lines, one for each result of tier 1. |
| `tls-cert` | `--tls-cert` | The certificate chain of the server, as PEM. |
| `tls-key` | `--tls-key` | The private key of `tls-cert`, as PEM. |
| `tls-client-ca` | `--tls-client-ca` | The CA certificates of the clients, as PEM. |
| `tls-read-only-client-ca` | `--tls-read-only-client-ca` | The CA certificates of the read-only clients, as PEM. |

### `<storage NAME>`

A store that `zoe serve` serves under the storage name `NAME`, the name of the section: a `[storages.NAME]` table of the configuration file. ZConfig gives the name in lower case, as for `runzeo`, and refuses two sections with the same name. The section comes once for each store.

| Key | Default | Value |
| --- | --- | --- |
| `path` | necessary | The directory of the store. The server creates it when it is missing, except for a read-only store. A leading `~` is the home directory. |
| `blob-dir` | `blobs` in `path` | The directory of the blob files. A leading `~` is the home directory. |
| `read-only` | `false` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If true, the server serves this store for reads only. |
| `pack-gc` | `true` | A boolean: `true`, `false`, `yes`, `no`, `on` or `off`. If false, a pack of this store removes old revisions only. |

## The `zoe://` URI

zodburi opens a `zoe://` URI with the resolver `zoe.config:resolve_uri`, the entry point `zoe` of the group `zodburi.resolvers` of the wheel. Scripts, `zodbverify` and `zodbconvert` take such a URI. The extra `zoedb[uri]` installs zodburi 3 or later.

```
zoe:///var/zoe?lock-timeout=10s&compression=zstd
```

- The path of the URI is the key `path`, and it must be absolute. A URI with a host or a fragment raises `ValueError`.
- The query takes the other keys of `<zoe>`, with the same values. Each key comes at most once, except `structural-merge-class`, which comes once for each class, as `structural-merge-class=CLASS=MODE`.
- The URI does not read the environment twins.
- The other keys of the query go to zodburi, which takes the keys of the database, such as `database_name`, and refuses the rest.
