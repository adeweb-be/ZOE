# Run in-process

In-process, a Zope instance opens the store in its own process, as it opens a `Data.fs` with `<filestorage>`. No server runs. The storage is `zoe.Storage`, from the `zoedb` wheel.

One process opens a store at a time: the store holds a lock. For two or more Zope instances on one store, run a server (see [Run a server](server.md)). The store has one format for both modes, so you can change the mode later without a conversion (see [Convert](convert.md#switch-between-in-process-and-server)).

## Quickstart

1. Install the `zoedb` wheel into the Python environment of the instance (see [Install](install.md)).
2. In `zope.conf`, replace the section `<zodb_db main>` with this one. Its storage is a `<zoe>` section in place of `<filestorage>`:

   ```xml test:zope-conf
   %import zoe
   <zodb_db main>
     cache-size 30000
     <zoe>
       path var/zoe
       blob-dir var/zoe/blobs
     </zoe>
     mount-point /
   </zodb_db>
   ```

3. Start the instance. The first open creates the store.

What each line does:

- `%import zoe` loads the section types of the wheel. It comes before the first section that uses them.
- `path` is the directory of the store. The storage creates it when it is missing.
- `blob-dir` is the directory of the blob files. Without it, the blobs go to `blobs` in `path`, as here. The store does not record its blob directory, so give the same one at each start. Never give it the blob directory of a `Data.fs`, such as `var/blobstorage`: the store writes and removes blob files there.
- A relative path is relative to the working directory of the process. Use absolute paths in production.

To keep the content of an existing site, convert its `Data.fs` first (see [Convert](convert.md)).

## The store directory

The store directory holds the database file `zoe.redb`, the file `lock` and, by default, the blob directory `blobs`. Do not copy the files of a store that a process holds open. To copy a store, use `zoe backup` (see [Back up and restore](backup-restore.md)).

## Settings

Each key of `<zoe>` has an environment twin: `ZOE_` and the key in upper case, with `_` for `-`, such as `ZOE_LOCK_TIMEOUT` for `lock-timeout`. The key wins over its twin. So one `zope.conf` can serve several hosts, each with its own `ZOE_PATH`.

Some keys that you can need:

| Key | Use it to |
| --- | --- |
| `lock-timeout` | change how long a commit waits for the lock of an object, 10 seconds by default |
| `pack-gc` | pack without garbage collection, for a site with mount points (see [Pack](pack.md)) |
| `compression` | compress the records with zstd |
| `read-only` | refuse every write |

A boolean key takes `true`, `false`, `yes`, `no`, `on` or `off`. [zope.conf sections](../reference/zope-conf.md#zoe) lists every key, its twin and its default.

`read-only true` does not let a second process share the store. The storage still takes the lock of the store.

## In a buildout

plone.recipe.zope2instance writes a `<filestorage>`, a `<zeoclient>` or a RelStorage section, but no `<zoe>` section. So let the recipe write no `zope.conf`, and render it from a template with collective.recipe.template:

```ini
[buildout]
parts =
    instance
    zope-conf

[instance]
recipe = plone.recipe.zope2instance
zope-conf = ${buildout:parts-directory}/instance/etc/zope.conf
eggs +=
    zoedb[plone]

[zope-conf]
recipe = collective.recipe.template
input = ${buildout:directory}/templates/zope.conf.in
output = ${instance:zope-conf}
```

The template holds a whole `zope.conf`, with the `<zoe>` section of the quickstart.

Buildout finds no `zoedb` on PyPI, because nothing is published yet. Install the wheel where buildout finds it. The `zoe` scenario of the repository is a whole example: [`plone/zoe.cfg`](https://github.com/adeweb-be/ZOE/blob/main/plone/zoe.cfg) and its template [`plone/templates/zope.conf.in`](https://github.com/adeweb-be/ZOE/blob/main/plone/templates/zope.conf.in). It installs the wheel into a directory and links that directory as a develop egg. `just plone zoe check` builds it, creates a Plone site, and reads the site over HTTP.

## The `zoe://` URI

zodburi opens a store from a URI, for scripts. Install the wheel with the extra `uri`. The path of the URI is the store directory, and it must be absolute. The query takes the other keys of `<zoe>`:

```
zoe:///var/lib/zoe?blob-dir=/var/lib/zoe-blobs&lock-timeout=10s
```

For example, in a script:

```python
import zodburi
from ZODB import DB

factory, dbkw = zodburi.resolve_uri("zoe:///var/lib/zoe")
db = DB(factory(), **dbkw)
```

The URI does not read the environment twins. [The zoe:// URI](../reference/zope-conf.md#the-zoe-uri) gives its rules.

## Stop the instance

A stop by SIGTERM or SIGHUP closes the store cleanly. The storage installs a handler of both signals when the process has none. A `tpc_finish` that started before the signal completes. A transaction that has not reached `tpc_finish` aborts. The handler waits at most 5 seconds, then the process ends. SIGINT, Ctrl-C, closes the store too.

A store that a process did not close, after SIGKILL or a crash, keeps every commit that a client saw complete. The next open repairs the database file, and takes longer on a large store. See [Verify and repair](verify-repair.md#a-store-that-was-not-closed-cleanly).
