# Run a server

`zoe serve` serves ZOE stores to ZEO clients over the ZEO 6 protocol. It replaces `runzeo` of ZEO. The clients stay as they are: a stock `ZEO.ClientStorage` connects. The server is the static `zoe` binary, with no Python inside (see [Install](install.md#the-binary-and-the-image)).

## Quickstart

Start a server on one store:

```sh test:serve
zoe serve --path var/zoe --listen 127.0.0.1:8100 --admin-listen 127.0.0.1:8101
```

The server creates the store when it does not exist. When it is ready, it writes two lines on stdout:

```text
admin 127.0.0.1:8101
listening 127.0.0.1:8100
```

The logs go to stderr. The store of `--path` has the storage name `1`, the default of ZEO.

Ask the admin port if the server is ready:

```sh test:readyz
curl http://127.0.0.1:8101/readyz
```

It answers `1: ready`, with the status 200. Connect a stock ZEO client, which needs the package `msgpack`, then commit and read an object:

```python test:client
import ZEO

db = ZEO.DB(("127.0.0.1", 8100))
with db.transaction() as connection:
    connection.root()["greeting"] = "hello"
with db.transaction() as connection:
    print(connection.root()["greeting"])
db.close()
```

Stop the server with Ctrl-C or SIGTERM. It completes the commits that it received, closes the store cleanly and exits with 0.

[Connect clients](clients.md) gives the `zope.conf` of a Zope or Plone instance.

## Several stores

One server serves several stores, each under its own storage name. `--storage NAME=PATH` adds a store:

```sh
zoe serve --listen 127.0.0.1:8100 --storage main=var/main --storage catalog=var/catalog
```

A client selects a store by its storage name: the key `storage` of its section. A Plone site with mount points uses one client section for each mount point, all on one server.

## A configuration file

`zoe serve --config FILE` reads the options from a TOML file. The keys are the options without `--`:

```toml test:zoe.toml
path = "var/zoe"
listen = "127.0.0.1:8100"
admin-listen = "127.0.0.1:8101"
shutdown-timeout = "30s"
```

```sh test:serve-config
zoe serve --config zoe.toml
```

Each `[storages.NAME]` table adds a store under the storage name `NAME`, with its own `path`, `blob-dir`, `read-only` and `pack-gc`.

For each option, the command line wins over the environment twin, `ZOE_*`, and the twin wins over the file. [The configuration file of zoe serve](../reference/config-file.md) gives the rules and a file with named stores.

## A `<zoe-server>` section, for buildout users

`python -m zoe.serve -C FILE` reads a `<zoe-server>` section of a ZConfig file, and starts `zoe serve` with its settings. A buildout can then write the settings of the server with the other files of the instance:

```xml test:zoe-server.conf
%import zoe
<zoe-server>
  path var/zoe
  listen 127.0.0.1:8100
  admin-listen 127.0.0.1:8101
</zoe-server>
```

```sh test:serve-zconfig
python -m zoe.serve -C zoe-server.conf
```

The Python of the command needs the `zoedb` wheel. The wheel has no server, so `zoe.serve` runs the `zoe` binary: the key `binary` of the section, else the variable `ZOE_BINARY`, else the `zoe` binary on the `PATH`. The process of the binary replaces the Python process, so a supervisor stops the server with SIGTERM as before. A `<storage NAME>` subsection adds a named store. [zope.conf sections](../reference/zope-conf.md#zoe-server) lists the keys.

## Common options

| Option | Use it to |
| --- | --- |
| `--blob-dir DIR` | keep the blobs of the store of `--path` outside the store directory. Give the same directory at each start. |
| `--lock-timeout SECONDS` | change how long a commit waits for the lock of an object, 10 seconds by default |
| `--read-only[=BOOL]` | serve each store for reads only |
| `--pack-gc=false` | pack without garbage collection, for a site with mount points (see [Pack](pack.md)) |
| `--log-format json` | write one JSON object for each log line |
| `--shutdown-timeout SECONDS` | change the longest time of a stop, 30 seconds by default |

A boolean option alone means `true`. To give `false`, write it with `=`: `--pack-gc=false`. `--pack-gc false` is a usage error, and the command exits with 2.

[The zoe command](../reference/cli.md#zoe-serve) lists every option of `zoe serve`, its twin and its default.

## A unix domain socket

A listen address with a `/` is the path of a unix domain socket:

```sh
zoe serve --path var/zoe --listen ./zeo.sock
```

A client gives the same path as its server address. The path must be shorter than 108 bytes. At its start, the server removes a socket file on which no process accepts connections. At its stop, it removes its socket file.

## TLS

`--tls-cert FILE` and `--tls-key FILE` make the ZEO port speak TLS. Both files are PEM.

```sh
zoe serve --path var/zoe --listen 10.0.0.5:8100 \
  --tls-cert server.pem --tls-key server-key.pem \
  --tls-client-ca clients-ca.pem
```

- `--tls-client-ca FILE` asks each client for a certificate that chains to one of these CA certificates.
- `--tls-read-only-client-ca FILE` adds a second CA. The holder of a certificate of this CA connects read-only only. Use it for tools that only read.
- The admin port stays plain HTTP.
- The server reads the files at its start only. To change a certificate, restart the server.

TLS needs a `HOST:PORT` address, not a socket. [Connect clients](clients.md#tls) gives the side of the clients.

## Stop

SIGTERM and SIGINT stop the server. At a stop, the server:

1. reads no new call;
2. completes each commit whose `tpc_finish` it received;
3. aborts the other transactions;
4. closes the connections and the stores.

The stop takes at most `--shutdown-timeout`, 30 seconds by default, and the command exits with 0. Give your supervisor a longer wait before it sends SIGKILL: the units of [Deploy on a host](deploy-host.md) wait 45 seconds.

A SIGKILL loses no commit that a client saw complete: `tpc_finish` replies only after the commit is durable. The next start repairs the database file, and takes longer on a large store.
