# Connect clients

A Zope or Plone instance connects to `zoe serve` with one of two client sections in `zope.conf`:

| Section | Client | Needs | Gives |
| --- | --- | --- | --- |
| `<zeoclient>` | the stock `ZEO.ClientStorage` | the package `msgpack` | the ZEO protocol. No ZOE extension. |
| `<zoeclient>` | `zoe.zeo.ClientStorage`, a subclass of `ZEO.ClientStorage` | the `zoedb` wheel | the ZEO protocol, and the ZOE extensions: the sequences, the KV namespaces and the early locks. By default, its client runs in Rust. |

Both connect to the same server, and you can mix them. Start with the stock client when you do not want ZOE in the instance. Use `<zoeclient>` for the features of `zoe.plone` that need the extensions (see [Plone](plone.md)).

## The stock ZEO client

`zoe serve` speaks only the msgpack protocol of ZEO 6. So the stock client needs the package `msgpack`, version 1 or later, in the Python environment of the instance. The extra `zoedb[plone]` installs it. Without `msgpack`, the client does not connect.

```xml
<zodb_db main>
  cache-size 30000
  <zeoclient>
    server 127.0.0.1:8100
    storage 1
    name zeostorage
    var var
    blob-dir var/blobcache
    shared-blob-dir off
  </zeoclient>
  mount-point /
</zodb_db>
```

- `server` is the address of the ZEO port of the server, `HOST:PORT` or the path of a unix domain socket.
- `storage` is the storage name. The store of `--path` has the name `1`, the default of ZEO.
- `blob-dir` with `shared-blob-dir off` is a cache of the blobs on the client. The server sends each blob that the client reads.

With plone.recipe.zope2instance:

```ini
[instance]
recipe = plone.recipe.zope2instance
zeo-client = on
zeo-address = 127.0.0.1:8100
blob-storage = ${buildout:directory}/var/blobcache
shared-blob = off
eggs +=
    msgpack
```

## `<zoeclient>`

`<zoeclient>` takes every key of `<zeoclient>`, with the same values, and a few keys of its own. It needs `%import zoe`:

```xml
%import zoe
<zodb_db main>
  cache-size 30000
  <zoeclient>
    server 127.0.0.1:8100
    storage 1
    name zoeclient
    blob-dir var/blobcache
    shared-blob-dir off
  </zoeclient>
  mount-point /
</zodb_db>
```

- By default, the client runs in Rust: the native client. `native false` runs the asyncio client of ZEO inside. That client declares the extensions too, but it takes no early locks.
- `wait-timeout` is the longest wait for a server, at the open and after a disconnection, 30 seconds by default.
- When the native client loads an object from the server, it also loads the objects that the object refers to, at most 256, in the background. So a page that reads the children of a folder waits for the server about once for the folder, not once for each child. `prefetch-references off`, or `ZOE_PREFETCH_REFERENCES=off` in the environment, turns it off.
- Against a stock ZEO server, the client works too, without the extensions. The native client needs `msgpack true` on that server. When `native` is not set and the server speaks only the pickle protocol of ZEO, the client takes the asyncio client of ZEO and logs a warning.

plone.recipe.zope2instance cannot write a `<zoeclient>` section. Render `zope.conf` from a template, as for the `<zoe>` section (see [Run in-process](in-process.md#in-a-buildout)). The `zoeclient` scenario of the repository is a whole example: [`plone/zoeclient.cfg`](https://github.com/adeweb-be/ZOE/blob/main/plone/zoeclient.cfg) and [`plone/templates/zoeclient.conf.in`](https://github.com/adeweb-be/ZOE/blob/main/plone/templates/zoeclient.conf.in).

[zope.conf sections](../reference/zope-conf.md#zoeclient) lists the keys of `<zoeclient>`.

## Mount points

A Plone site with mount points has one database for each mount point. Serve each database as a named store of one server, and give each database its own client section with its storage name:

```xml
<zodb_db content>
  cache-size 30000
  <zeoclient>
    server 127.0.0.1:8100
    storage content
    name content
    blob-dir var/blobcache-content
    shared-blob-dir off
  </zeoclient>
  mount-point /content
  container-class OFS.Folder.Folder
</zodb_db>
```

The server then serves the storage names of both databases, for example `zoe serve --storage main=/var/lib/zoe/main --storage content=/var/lib/zoe/content`. Plone 6.1 does not ship `Products.ZODBMountPoint`. Add it to the eggs of the instance.

A pack of one database cannot see the references from the other databases. So pack each store of a site with mount points without garbage collection (see [Pack](pack.md)).

## TLS

When the server speaks TLS (see [Run a server](server.md#tls)), give the client an `<ssl>` section.

With `<zoeclient>`:

```xml
%import zoe
<zodb_db main>
  <zoeclient>
    server zoe.example.org:8100
    storage 1
    blob-dir var/blobcache
    <ssl>
      certificate client.pem
      key client-key.pem
      authenticate server-ca.pem
    </ssl>
  </zoeclient>
  mount-point /
</zodb_db>
```

- `certificate` and `key` are the certificate of the client and its private key, as PEM. The server checks them when it has `--tls-client-ca` or `--tls-read-only-client-ca`.
- `authenticate` is the CA certificate that the certificate of the server must chain to.
- `check-hostname` is true by default: the certificate of the server must be valid for the host of `server`. `server-hostname` gives another name.
- The native client does not read an encrypted key: it refuses `password-function`. Use `native false` for an encrypted key.

The stock client takes TLS in the `<clientstorage>` section of ZEO, with the same `<ssl>` section. Write `%import ZEO` in place of `%import zoe`: ZConfig refuses a file that imports both, because both declare a section `<ssl>`.

## The read-only fallback

A client that asks to write gets `ReadOnlyError` from a store that serves reads only: a server with `--read-only`, a store with `read-only = true`, or a certificate of `--tls-read-only-client-ca`. With `read-only-fallback true`, the client then connects read-only, and the instance serves the pages that only read:

```xml
<zeoclient>
  server 127.0.0.1:8100
  storage 1
  read-only-fallback true
</zeoclient>
```

`read-only true` connects read-only from the start. Both keys work in `<zeoclient>` and in `<zoeclient>`.

## Pack from a client

A client packs the store through the server, while the other clients work. The `Pack` button of the Zope management interface does it, and so does the `zeopack` script of ZEO. See [Pack](pack.md).
