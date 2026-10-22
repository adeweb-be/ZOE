# The configuration file of `zoe serve`

`zoe serve --config FILE`, or the twin `ZOE_CONFIG`, reads a TOML file of options and stores. `python -m zoe.serve` writes such a file from a [`<zoe-server>` section](zope-conf.md#zoe-server).

## Precedence

For each option, the first source that gives it wins:

1. the command line;
2. the environment twin, `ZOE_*`;
3. the configuration file;
4. the default of the option.

The stores add up. The server serves the store of `--path` under the storage name `1`, then the stores of `--storage`, then the stores of the `[storages.NAME]` tables of the file, in the order of their names. A storage name that comes twice is a usage error.

## Keys

- The top-level keys are the long options of `zoe serve` without `--`, such as `listen` and `admin-listen` (see [The zoe command](cli.md#zoe-serve)). Three options are not keys: `config`, `storage` and `structural-merge-class`. The `[storages.NAME]` tables take the place of `--storage`, and the table `[structural-merge-classes]` the place of `--structural-merge-class`.
- A key takes a string, an integer or a boolean, with the syntax of its option: `listen = "127.0.0.1:8100"`, `client-queue-size = 1000`, `read-only = true`, `shutdown-timeout = "30s"`.
- Each `[storages.NAME]` table adds a store under the storage name `NAME`.
- The table `[structural-merge-classes]` gives the mode of classes of tier 1, as `CLASS = MODE`. When the command line or the twin gives `--structural-merge-class`, the server ignores this table.

| Key of `[storages.NAME]` | Default | Value |
| --- | --- | --- |
| `path` | necessary | The directory of the store, a string that is not empty. The server creates the store when it does not exist, except a read-only store. |
| `blob-dir` | `blobs` in `path` | The blob directory of the store, a string that is not empty. The store does not record it, so give the same one at each start. |
| `read-only` | `false` | `true` or `false`. If true, the server serves this store for reads only. |
| `pack-gc` | `true` | `true` or `false`. If false, a pack of this store removes old revisions only. A store packs with garbage collection only when this key and the option `pack-gc` are both true. |

A relative path is relative to the working directory of the server.

## Errors

These are usage errors. The command exits with 2 before it opens or creates anything:

- a file that does not read or does not parse;
- an unknown key, at the top level or in a table;
- a value of the wrong type, or a value that its option refuses;
- a `[storages.NAME]` table without `path`, or with an empty name;
- no store at all: no `--path`, no `--storage` and no table;
- a storage name that comes twice.

## Example

`deploy/zoe.toml` is the example of a host, for the systemd unit and the supervisor program. Install it as `/etc/zoe/zoe.toml`:

```toml
# The store of the storage name 1, the default of ZEO. Its blobs go to
# /var/lib/zoe/blobs. Add stores with [storages.NAME] tables.
path = "/var/lib/zoe"

# The ZEO port. Put it on the private address of the host that the ZEO
# clients reach.
listen = "127.0.0.1:8100"

# The admin port: /healthz, /readyz and /metrics. Put it on a private
# address, never on a public one.
admin-listen = "127.0.0.1:8101"

# The stop on SIGTERM ends at this timeout. TimeoutStopSec of the systemd
# unit and stopwaitsecs of the supervisor program are above it.
shutdown-timeout = "30s"

# One JSON object for each log line on stderr.
log-format = "json"
```

A file with named stores and a mode for one class:

```toml
listen = "0.0.0.0:8100"
lock-timeout = "10s"
resolution-log = "/var/log/zoe/merges.log"

[structural-merge-classes]
"BTrees.OOBTree.OOBucket" = "shadow"

[storages.main]
path = "/var/zoe/main"

[storages.catalog]
path = "/var/zoe/catalog"
blob-dir = "/var/zoe/catalog-blobs"
read-only = false
```
