# The `zoe` command

`zoe` is one binary with seven subcommands. This page gives each subcommand, each of its options, the environment twin of each option, its default and the exit codes. ADR 0028 records the decisions of the command.

## Entry points

| Entry point | What it runs |
| --- | --- |
| the `zoe` binary | every subcommand |
| the console script `zoe` of the `zoedb` wheel | the same code as the binary, without `serve` and `mcp` |
| `python -m zoe` | the console script |
| `python -m zoe.serve -C FILE` | `zoe serve`, with the `<zoe-server>` section of a ZConfig file (see [zope.conf sections](zope-conf.md#zoe-server)) |

The extension module of the wheel has no server and no MCP server. So `zoe serve` and `zoe mcp` through the console script fail with exit code 1, write a message that names the `zoe` binary, and create nothing.

`zoe --help` and `zoe <subcommand> --help` give the help on stdout. `zoe --version` gives the version. Both exit with 0. `zoe` with no argument writes the help to stderr and exits with 2.

## Rules of all subcommands

- Each option has an environment twin, `ZOE_` and the name of the option in upper case, with `_` for `-`. One option breaks this rule: the twin of `--structural-merge-class` is `ZOE_STRUCTURAL_MERGE_CLASSES`, which takes `CLASS=MODE` pairs separated by commas. The help of each option names its twin. An option on the command line wins over its twin. [Environment variables](environment.md) lists the twins.
- `zoe serve` also reads a [configuration file](config-file.md). The command line wins over the twin, and the twin wins over the file.
- The command parses all its arguments before it opens a file. So a usage error changes nothing on disk.
- The report of a subcommand goes to stdout. A failure goes to stderr, as `zoe <subcommand>: <message>`. A usage error goes to stderr, as `error: <message>` and the usage.
- A subcommand that only reads a store opens it read-only. On a store that was closed cleanly, it changes no byte. On a store that a writer left open, it repairs the store first, and writes one line to stderr that says `repaired the store` (ADR 0028).

## Exit codes

| Code | Meaning |
| --- | --- |
| 0 | success; also `--help`, `--version`, and the end of `zoe serve` on SIGTERM or SIGINT |
| 1 | a failure: the work failed, a conversion does not verify, `zoe verify` found a problem, or the report cannot go out |
| 2 | a usage error: no argument, a bad option, a bad value, a missing option, or for `zoe serve` a configuration file that does not read or parse |

## Values

| Value | Syntax |
| --- | --- |
| `BOOL` | `true` or `false`, in any case. `yes`, `no`, `on`, `off`, `y`, `n`, `t`, `f`, `1` and `0` also work. The ZConfig sections take fewer words (see [zope.conf sections](zope-conf.md)). The option alone means `true`. A value needs `=`: `--read-only=false`. `--read-only false` is a usage error. The twin takes the same values. |
| `SECONDS` | a whole number of seconds, 0 or more, with the suffix `s`, `m`, `h` or `d` in any case, or no suffix for seconds: `30`, `30s`, `5m`, `1h`, `1d`. It is the syntax of ZConfig's `time-interval`. |
| `BYTES` of `--cache-size` | a whole number, with the suffix `KB`, `MB` or `GB` in any case, or no suffix for bytes. Each suffix is a power of 1024, as in ZConfig's `byte-size`. |
| `MODE` | `on`, `shadow` or `off`, in any case. |
| `CODEC` | `none`, `zstd` or `zstd:LEVEL`, in any case. `zstd` is zstd at level 3. |

## `zoe convert`

Convert a `Data.fs` into a store, or a store into a `Data.fs`. Give `--from-data-fs` or `--to-data-fs`, not both. Each conversion resumes after the last transaction of its target, verifies the target, and writes a report. A report that lists a problem fails the command with 1.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the store. With `--from-data-fs`, the command creates the store when it does not exist. With `--to-data-fs`, the store must exist. |
| `--blob-dir DIR` | `ZOE_BLOB_DIR` | `blobs` in the store directory | The blob directory of the store, as the key `blob-dir` of `<zoe>`. The store does not record it. With `--from-data-fs`, the blobs of the store go there. With `--to-data-fs`, a store with blobs whose blob directory has no layout file fails the command before it writes anything. |
| `--from-data-fs FILE` | `ZOE_FROM_DATA_FS` | none | Copy the transactions of this `Data.fs` into the store. |
| `--to-data-fs FILE` | `ZOE_TO_DATA_FS` | none | Copy the transactions of the store into this `Data.fs`. It needs `--data-fs-blob-dir`. |
| `--data-fs-blob-dir DIR` | `ZOE_DATA_FS_BLOB_DIR` | none | The blob directory of the `Data.fs`. A conversion from a `Data.fs` without it copies no blob, and its report counts the blob records as a problem. |
| `--force[=BOOL]` | `ZOE_FORCE` | `false` | With `--to-data-fs`, convert a store that holds a moved registry of `zoe.plone` (ADR 0043, part B) or records of the classes of `zoe.plone`, and write a warning that names each registry and each class. Without it, the command refuses such a store and writes nothing: a `Data.fs` holds no namespace, and a client without `zoedb` loads these records as broken objects. `--force=false` refuses also when the twin forces. It has no effect with `--from-data-fs`. |

## `zoe pack`

Pack a store as of a number of days ago, or report what that pack would remove.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the store. It must hold a store. |
| `--days N` | `ZOE_DAYS` | necessary | Pack as of N days ago: the time of the clock minus N times 86,400 seconds, as ZODB's `DB.pack(days=N)` computes it. The pack keeps every revision after that time. Of the older revisions, it keeps the newest one of each object that a reader at that time can reach, and removes the others. |
| `--pack-gc[=BOOL]` | `ZOE_PACK_GC` | `true` | Collect garbage. With `false`, the pack removes old revisions only, as the `pack-gc` of FileStorage does: of the revisions at or before the pack time, it keeps the newest one of each object, also when no reader can reach the object. An object whose newest revision there is a deletion goes. |
| `--dry-run[=BOOL]` | `ZOE_DRY_RUN` | `false` | Report what the pack would remove, by class, and change nothing. A dry run opens the store read-only. `--dry-run=false` packs, also when the twin asks for a dry run. |

## `zoe serve`

Serve stores with the ZEO 6 msgpack protocol until SIGTERM or SIGINT, each store under its own storage name. A stock `ZEO.ClientStorage` connects. The server announces and takes only the protocol `M5`.

- The store of `--path` has the storage name `1`, the default of ZEO. `--storage NAME=PATH` adds a store, and so does each `[storages.NAME]` table of the [configuration file](config-file.md). The order is: `--path`, then `--storage`, then the tables of the file in the order of their names. One store at least is necessary. A storage name that comes twice is a usage error.
- The server creates a store that does not exist, except a read-only store.
- stdout carries only two lines: `admin <address>` when the server has an admin port, then `listening <address>` once the server accepts connections. So a port of 0 gives a free port. The logs go to stderr (see [Admin port, metrics and logs](metrics.md)).
- SIGTERM or SIGINT stops the server within `--shutdown-timeout`, and the command exits with 0. The command handles both signals before it creates anything on disk.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | none | The directory of the store of the storage name `1`. |
| `--blob-dir DIR` | `ZOE_BLOB_DIR` | `blobs` in the store directory | The blob directory of the store of `--path`. It needs `--path`. The key `blob-dir` of a `[storages.NAME]` table sets the blob directory of that store. The store does not record its blob directory, so give the same one at each start. |
| `--storage NAME=PATH` | `ZOE_STORAGE` | none | A store under the storage name `NAME`, which a client gives to `register`. Repeat the option for more stores. The twin gives one store. |
| `--config FILE` | `ZOE_CONFIG` | none | The [configuration file](config-file.md), a TOML file of options and stores. A file that does not read or parse, or an unknown key, is a usage error. |
| `--listen ADDRESS` | `ZOE_LISTEN` | `127.0.0.1:8100` | The address of the ZEO port: `HOST:PORT` with an IP address, or the path of a unix domain socket. A value with a `/` is a path, such as `./zeo.sock`. A port of 0 takes a free port. The server binds a relative path as it is, so the path must be shorter than 108 bytes. At its start, the server removes a socket file on which no process accepts connections, and fails when a process accepts there. At its stop, it removes its socket file. TLS needs `HOST:PORT`. |
| `--admin-listen HOST:PORT` | `ZOE_ADMIN_LISTEN` | none: no admin port | The address of the admin port, an HTTP port with `/healthz`, `/readyz` and `/metrics` (see [Admin port, metrics and logs](metrics.md)). A port of 0 takes a free port. |
| `--invalidation-queue-size TRANSACTIONS` | `ZOE_INVALIDATION_QUEUE_SIZE` | `100000` | The most transactions that `getInvalidations` covers. A client that reconnects further behind gets nil and verifies its cache. |
| `--invalidation-age SECONDS` | `ZOE_INVALIDATION_AGE` | `0`, off | Past `--invalidation-queue-size`, `getInvalidations` also covers every transaction after the tid of a client when the time of that tid is less than this before the clock of the server, as the key `invalidation-age` of ZEO's `<zeo>` section. |
| `--client-queue-size MESSAGES` | `ZOE_CLIENT_QUEUE_SIZE` | `1000`, at least 1 | The most messages that wait in the hub queue of one connection. Past it, the server closes the connection, and the client reconnects and verifies its cache. |
| `--cache-size BYTES` | `ZOE_CACHE_SIZE` | the default of the store, 1 GiB | The most bytes that the page cache of each store holds, such as `512MB` or `1GB`. |
| `--lock-timeout SECONDS` | `ZOE_LOCK_TIMEOUT` | the default of the store, 10 seconds | The longest time that a vote waits for the lock of an object. A longer wait fails the vote with `ConflictError`. |
| `--lock-warn SECONDS` | `ZOE_LOCK_WARN` | none: no line | Log each lock held longer than this, when the lock is released. 0 logs no lock. |
| `--transaction-warn SECONDS` | `ZOE_TRANSACTION_WARN` | none: no line | Log each transaction open longer than this, when it ends. 0 logs no transaction. |
| `--log-format FORMAT` | `ZOE_LOG_FORMAT` | `text` | The format of the log lines on stderr: `text`, or `json` for one JSON object on each line. |
| `--max-frame-size BYTES` | `ZOE_MAX_FRAME_SIZE` | `268435456`, 256 MiB | The longest frame that a client can send: a whole number of bytes, 1 or more, with no suffix. A longer frame closes its connection. A `storea` or `restorea` frame holds a whole record, so a copy through the server needs a limit above the largest record of its source. |
| `--shutdown-timeout SECONDS` | `ZOE_SHUTDOWN_TIMEOUT` | `30` | The longest time that a stop on SIGTERM or SIGINT takes. The server commits each `tpc_finish` that it received, aborts the other transactions, closes the connections and closes the stores. A call that still runs at the end keeps the store of its session open, and that store does not close cleanly. |
| `--idle-timeout SECONDS` | `ZOE_IDLE_TIMEOUT` | `300` | The longest time that a connection sends no frame while no call of it runs. A heartbeat is a frame, and a stock client sends one every 60 seconds. The server closes an idle connection: its transaction aborts and its locks go. 0 closes no idle connection. |
| `--transaction-timeout SECONDS` | `ZOE_TRANSACTION_TIMEOUT` | `0`, off | The longest time that a transaction stays open after its vote, as the key `transaction-timeout` of ZEO. Past it, the server closes the connection, and the transaction aborts. |
| `--blocking-threads THREADS` | `ZOE_BLOCKING_THREADS` | the number of cores | The threads that run the calls that read, such as `loadBefore`, for all the connections together, 1 or more. A transaction, a download and a pack each run on a thread outside this pool. |
| `--compression CODEC` | `ZOE_COMPRESSION` | the default of the store, `none` | The compression of the records that the stores write. A store reads every compression, whatever this option. |
| `--read-only[=BOOL]` | `ZOE_READ_ONLY` | `false` | Serve every store for reads only. The store must exist. The server holds the lock of each store, so no writer opens it. A client that registers to write gets `ReadOnlyError`. The key `read-only` of a `[storages.NAME]` table makes one store read-only. |
| `--pack-gc[=BOOL]` | `ZOE_PACK_GC` | `true` | Pack each store with garbage collection. With `false`, a pack removes old revisions only. The key `pack-gc = false` of a `[storages.NAME]` table sets it for one store. The store records nothing, so give it at each start. |
| `--structural-merges MODE` | `ZOE_STRUCTURAL_MERGES` | `on` | The mode of the structural merges (tier 1) of each class that `--structural-merge-class` does not name (ADR 0042). A shadow mode needs `--resolution-log`. |
| `--structural-merge-class CLASS=MODE` | `ZOE_STRUCTURAL_MERGE_CLASSES` | none | The mode of one class of tier 1, such as `BTrees.IOBTree.IOBucket=shadow`. Repeat the option for more classes. The twin takes pairs separated by commas. The configuration file takes a table `[structural-merge-classes]`. |
| `--posting-dicts MODE` | `ZOE_POSTING_DICTS` | `off` | The mode of the posting-dict rule and the first-key rule of ADR 0042. A shadow mode needs `--resolution-log`. |
| `--resolution-log FILE` | `ZOE_RESOLUTION_LOG` | none: no line | A file of JSON lines, one for each result of tier 1. The server opens it in append mode at its start, and never syncs it. |
| `--shared-blob-dir[=BOOL]` | `ZOE_SHARED_BLOB_DIR` | `false` | Accept `storeBlobShared`: the server and its clients share the blob directory of each store, as with the option `shared-blob-dir` of a ZEO client. Run the clients as the user of the server. Without it, `storeBlobShared` fails the vote. |
| `--tls-cert FILE` | `ZOE_TLS_CERT` | none: no TLS | The certificate chain of the server, as PEM, leaf first. It needs `--tls-key`. With both, the ZEO port speaks TLS. The admin port stays plain HTTP. The server reads the file at its start only. |
| `--tls-key FILE` | `ZOE_TLS_KEY` | none | The private key of `--tls-cert`, as PEM. It needs `--tls-cert`. |
| `--tls-client-ca FILE` | `ZOE_TLS_CLIENT_CA` | none | The CA certificates of the clients, as PEM. The server then requires a client certificate that chains to one of them, as the key `authenticate` of ZEO's `<ssl>` section does. It needs `--tls-cert`. |
| `--tls-read-only-client-ca FILE` | `ZOE_TLS_READ_ONLY_CLIENT_CA` | none | The CA certificates of the read-only clients, as PEM. The holder of a certificate that chains to one of them registers read-only only. With this option, a client certificate is necessary. It needs `--tls-cert`. |

## `zoe mcp`

Serve the read-only ops tools of a store to an agent, over MCP on stdin and stdout. stdout carries only MCP messages. The command ends with 0 when the client closes stdin. [MCP tools](mcp.md) lists the tools and the prompts.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the store. The command opens it for reads only, and holds the lock of the store. |
| `--blob-dir DIR` | `ZOE_BLOB_DIR` | `blobs` in the store directory | The blob directory of the store. The tools `verify` and `space` read the blob files there. For a store with blobs whose blob directory has no layout file, `verify` is a tool error. |

## `zoe verify`

Check the checksums, the blob files, the references and the classes of a store, or compare the dumps of a backup directory with it. The command changes nothing, and exits with 1 on a finding.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the store. |
| `--blob-dir DIR` | `ZOE_BLOB_DIR` | `blobs` in the store directory | The blob directory of the store. A store with blobs whose blob directory has no layout file fails the command before the check. |
| `--format FORMAT` | `ZOE_FORMAT` | `text` | The format of the report: `text`, lines for a person, or `json`, one JSON document for a program, with oids and tids as integers. |
| `--classes FILE` | `ZOE_CLASSES` | none | A file of the classes that the application provides, one `module.name` on each line. A class of the store that the file does not name is a finding. It does not go with `--backup`. |
| `--backup DIR` | `ZOE_BACKUP` | none | Compare the dumps of this backup directory with the store, in place of the check of the store. The command restores the dumps into a copy, opens the store read-only, compares the copy with the store up to the last tid of the dumps, and removes the copy. A difference is a finding. |
| `--scratch SCRATCH` | `ZOE_SCRATCH` | the temporary directory, `TMPDIR` | The directory of the copy of `--backup`, on a filesystem with room for the whole store. It needs `--backup`. The command removes the copy at the end, also when the check fails. |

## `zoe backup`

Write a dump of a store into a backup directory: the full dump the first time, then the transactions after the last dump. With nothing new, the command writes nothing. A running `zoe serve` holds the lock of its stores, so back up a store while no server serves it.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the store. The command opens it read-only. |
| `--to DIR` | `ZOE_TO` | necessary | The backup directory. The command creates it when it is missing. It never changes a dump. |

## `zoe restore`

Rebuild a store from the dumps of a backup directory. The command checks every dump before it writes anything. A restore that fails leaves nothing at the target.

| Option | Twin | Default | Value |
| --- | --- | --- | --- |
| `--from DIR` | `ZOE_FROM` | necessary | The backup directory. |
| `--path STORE` | `ZOE_PATH` | necessary | The directory of the new store. It must not exist, or be an empty directory. The command refuses a directory that holds a store. |
