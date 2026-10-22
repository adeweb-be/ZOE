# Troubleshooting

This page lists the messages that a user of ZOE can see, with their cause and their fix. A message of the `zoe` command starts with `zoe <subcommand>:` and goes to stderr. A usage error starts with `error:` and exits with 2. A failure exits with 1.

## The store

### `the store or its blob directory is locked by another open store`

```text
zoe verify: var/zoe: the store or its blob directory is locked by another open store
```

**Cause.** Another process holds the store: a Zope instance in-process, `zoe serve`, `zoe mcp`, Loupe, or another command. A store has one lock, also for a read-only open. In-process, a second instance on the same store gets this message as a `StorageError`.

**Fix.** Stop the process that holds the store, or run the command on a copy of the store, such as a restore of a backup. For several Zope instances on one store, run a server (see [Run a server](server.md)).

### `the store was not closed cleanly: repaired the store`

```text
zoe verify: var/zoe: the store was not closed cleanly: repaired the store
```

**Cause.** A process ended without a close of the store: SIGKILL, a crash or a loss of power. The tool repaired the database file before it read the store. `zoe serve` writes the same words in a WARN log line, and Loupe in its window.

**Fix.** Nothing. Every commit that a client saw complete is in the store. To avoid the repair, stop the instance and the server with SIGTERM, and give them the time of their stop (see [Verify and repair](verify-repair.md#a-store-that-was-not-closed-cleanly)).

### `the store was not closed cleanly, and its repair failed`

```text
zoe verify: var/zoe: the store was not closed cleanly, and its repair failed: Permission denied (os error 13)
```

**Cause.** The repair writes the database file `zoe.redb`, and it cannot: the file system is read-only, or the user cannot write the file.

**Fix.** Run the tool as the user that owns the store, or copy the store to a writable place and run the tool there.

### `the store has blobs, but the blob directory ... has no layout file`

```text
zoe verify: var/zoe: the store has blobs, but the blob directory /srv/empty-blobs has no layout file: give the blob directory of the store with --blob-dir (ZOE_BLOB_DIR)
```

In-process, the open raises a `StorageError` that ends with `open the store with its blob directory`.

**Cause.** The store does not record its blob directory. The open got a directory that is not the blob directory of the store: a wrong or missing `blob-dir` or `--blob-dir`, or a file system that is not mounted.

**Fix.** Give the blob directory of the store: the key `blob-dir` of `<zoe>`, or the option `--blob-dir` of `zoe` and `zoe serve`. Without the option, it is `blobs` in the store directory.

### `the blob directory ... has no layout file and holds other files`

**Cause.** The blob directory has no layout file, and it holds files of another program. The open refuses it, so it removes none of these files.

**Fix.** Give an empty or a missing directory.

### `unknown store format`

**Cause.** The directory holds a file `zoe.redb` that this build does not read: a store of a newer build of ZOE, or a file that is not a store.

**Fix.** Open the store with the build that wrote it, or with a newer one. A store of the old `ZOE format 1` needs no fix: the first writer open upgrades it.

### `a finish failed: reopen the store to commit again`

**Cause.** A commit failed at its write to the disk, for example on a full disk. The outcome of that commit is unknown, so the store refuses every later commit. In-process, the error is a `StorageSystemError`.

**Fix.** Free space on the disk, then restart the instance or the server. The open of the store gives the last durable commit.

## The command line

### `error: unexpected argument 'false' found`

```text
error: unexpected argument 'false' found

Usage: zoe pack [OPTIONS] --path <STORE> --days <N>
```

**Cause.** A boolean option takes its value only with `=`. The command read `false` as a second argument.

**Fix.** Write `--pack-gc=false`. The same holds for `--read-only`, `--shared-blob-dir`, `--dry-run` and `--force`. An option alone means `true`.

### `this build has no server`

```text
zoe serve: this build has no server; run the `zoe` binary
```

**Cause.** You ran `zoe serve` or `zoe mcp` through the console script of the wheel, which has no server.

**Fix.** Run the static `zoe` binary (see [Install](install.md#the-binary-and-the-image)). `python -m zoe.serve` also runs the binary.

### `zoe convert: ... not a FileStorage file`

```text
zoe convert: notes.txt: not a FileStorage file: the magic is not "FS30"
```

**Cause.** The file of `--from-data-fs` is not a `Data.fs`.

**Fix.** Give the `Data.fs` itself, not its index, its lock file or a directory. The command created nothing.

### `zoe convert: ... Data.fs.lock: No such file or directory`

```text
zoe convert: var/export/Data.fs.lock: No such file or directory (os error 2)
```

**Cause.** The directory of the `Data.fs` of `--to-data-fs` does not exist.

**Fix.** Create the directory, then run the command again.

### `zoe convert: the conversion does not verify`

**Cause.** The report lists a problem: the target differs from the source. A conversion from a `Data.fs` without `--data-fs-blob-dir` counts each blob record as a problem.

**Fix.** Read the problems in the report. Give `--data-fs-blob-dir` when the `Data.fs` has blobs, then run the command again: it resumes after the last transaction of the store.

### `zoe convert: ... the store holds ... of the classes of zoe.plone`

```text
zoe convert: var/zoe: the store holds 1 record of the classes of zoe.plone. A client without zoedb loads them as broken objects, so a site of the copy cannot read the properties of its members and groups, or its workflow histories:
  zoe.plone.history.WorkflowHistory: 1 record (0x000000000000002d)
Import the profile zoe.plone:uninstall on each site of the store first: it gives the stock values back. Then pack the store with zoe pack, with its GC, at a time after the uninstall: the pack removes the records that no object references, such as those of a deleted member or of deleted content. Then convert. Or give --force (ZOE_FORCE) to convert anyway.
```

**Cause.** `--to-data-fs` found records of the classes of `zoe.plone`, which a client without `zoedb` loads as broken objects. A moved registry gives a message of the same kind, which names `move_back`.

**Fix.** Follow the steps of [Uninstall](plone.md#uninstall): move each registry back, import the profile `zoe.plone:uninstall`, then pack the store with garbage collection. Or give `--force` when the copy does not need these data.

### `zoe restore`: the target holds a store

```text
zoe restore: var/zoe-restored: holds a store; a restore never writes into a store
```

**Cause.** `zoe restore` builds a new store. It refuses a directory that holds a store.

**Fix.** Give a new or an empty directory. To restore in place, move the old store away first.

### `zoe backup`: the store was packed after the last tid of the dumps

```text
zoe backup: backups/zoe: the store was packed at tid 0x040c80440de1d2aa, after the last tid 0x040c80440d4f6f11 of the dumps; write a new full dump into a new directory
```

**Cause.** A pack ran past the last backup. It removed history that the next dump of the chain must hold, so the command writes nothing.

**Fix.** Write a new full dump into a new backup directory: `zoe backup --path var/zoe --to backups/zoe-2`. Keep the old directory as long as you need its history. To keep one chain, pack only as of a time at or before the last backup (see [Back up and restore](backup-restore.md#a-pack-and-the-chain-of-dumps)).

## `zoe serve`

### `no store to serve`

```text
error: no store to serve: give --path, --storage or a [storages.NAME] table of --config
```

**Cause.** The command got no store. It exits with 2.

**Fix.** Give `--path`, a `--storage NAME=PATH`, or a `[storages.NAME]` table in the configuration file.

### The port is in use

```text
zoe serve: cannot listen on 127.0.0.1:8100: Address already in use (os error 98)
```

**Cause.** Another process listens on the address of `--listen` or `--admin-listen`. The server creates no store and exits with 1.

**Fix.** Stop the other process, or give another port. On a unix domain socket, the server removes a socket file on which no process accepts connections, and fails when a process accepts there.

### The configuration file

A file that does not parse, an unknown key, a value of the wrong type and a `[storages.NAME]` table without `path` exit with 2, before the server creates anything. The message names the file and the key, such as `the configuration file zoe.toml: no-such-key is not a key of the file`. [The configuration file of zoe serve](../reference/config-file.md#errors) lists the errors.

### `/readyz` answers not ready

`/readyz` answers 503:

| Body | Cause | Fix |
| --- | --- | --- |
| `starting` | the stores still open. An open after a crash repairs the database file and takes longer. | Wait. Give the startup probe time. |
| `stopping` | the server got SIGTERM or SIGINT | Nothing: the readiness probe keeps the clients away during the stop. |
| `NAME: not ready: the lock table held entries at the open` | the lock table of the store held entries just after the open. The lock table lives in memory, and a new open starts with an empty table, so this is a fault of ZOE. | Restart the server. Report the fault with the log of the start. |

## The clients

### A stock ZEO client does not connect

**Cause.** `zoe serve` speaks only the msgpack protocol of ZEO 6, and the client has no `msgpack`. The log of the instance shows an error for each try, then the client raises `ClientDisconnected`:

```text
ERROR ZEO.asyncio.smp: Processing message `b'M5'` failed
ModuleNotFoundError: No module named 'msgpack'
```

**Fix.** Install `msgpack` 1 or later in the Python environment of the instance. The extra `zoedb[plone]` installs it.

### `ReadOnlyError` at the start

**Cause.** The client asks to write, and the store serves reads only: `--read-only`, `read-only = true` for the store, or a client certificate of `--tls-read-only-client-ca`.

**Fix.** Serve the store for writes, or give the client `read-only true`, or `read-only-fallback true` to connect read-only when it cannot write (see [Connect clients](clients.md#the-read-only-fallback)).

### `ValueError` at the start

**Cause.** The key `storage` of the client names a store that the server does not serve. The server answers the `register` of the client with `ValueError`, and the client does not connect.

**Fix.** Give a storage name of the server. The store of `--path` has the name `1`. Each `--storage NAME=PATH` and each `[storages.NAME]` table gives its name.

### `ConflictError` after a wait

**Cause.** A commit waited longer than the lock timeout for the lock of an object, 10 seconds by default. Another transaction held that lock: a long commit, or a client that stopped during its commit.

**Fix.** Zope retries the request. When it happens often, look for the slow commits with `--lock-warn` and `--transaction-warn` (see [Monitoring](monitoring.md#logs)). `--transaction-timeout` closes a client that holds its locks too long after its vote.

### ZConfig refuses `%import zoe` with `%import ZEO`

```text
ZConfig.SchemaError: type name cannot be redefined: 'ssl'
```

**Cause.** The components of ZEO and of `zoedb` both declare a section `<ssl>`, so one file cannot import both.

**Fix.** Use `<zoeclient>` with `native false` and its `<ssl>` section in place of `<clientstorage>` of ZEO (see [Connect clients](clients.md#tls)).

## In-process

### `<zoe> needs path`

```text
ValueError: <zoe> needs path, or the ZOE_PATH variable
```

**Cause.** The `<zoe>` section has no `path`, and the variable `ZOE_PATH` is not set.

**Fix.** Give `path` in the section, or set `ZOE_PATH`.

### `zoe.WrongThread`

**Cause.** A thread called a method of a transaction that another thread began, such as `store` or `tpc_vote`. The thread that begins a transaction drives it. Application code that shares a connection between threads gets this error.

**Fix.** Give each thread its own connection.

### `RuntimeWarning: The global interpreter lock (GIL) has been enabled`

**Cause.** On the free-threaded CPython 3.14t, a C extension that does not declare that it runs without the GIL turns the GIL back on.

**Fix.** See [Known issues](known-issues.md#the-free-threaded-cpython-314t).

## Plone

### `no backend ... of zoe.plone`

```text
ValueError: no backend 'search' of zoe.plone: use locking, indexing or registry
```

**Cause.** `ZOE_PLONE_BACKENDS` names a backend that does not exist. The instance does not start.

**Fix.** Use the names `locking`, `indexing` and `registry`, separated by commas.

### `Unsupported` on each request, from the registry

**Cause.** The registry of the site is a copy whose records stayed in the namespaces of another store: a copy through `zodbconvert`, `zoe convert --force`, a zexp or a copy of the site.

**Fix.** On the source store, move the registry back with `zoe.plone.registry.move_back`, then copy again. When the source is gone, `zoe.plone.registry.reset` gives the copy empty records, and an import of the profiles of the site gives the default values back.
