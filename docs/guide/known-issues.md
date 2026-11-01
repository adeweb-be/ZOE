# Known issues

This page lists the limits of ZOE today that a user meets. The status table of the [technical specification](../spec/technical-specification.md#status) lists every part that is not built.

## Nothing is published

No release is on PyPI or on `ghcr.io`. Build the wheel, the binary and the image from the source (see [Install](install.md)). The version of each part is `0.0.0`.

## The free-threaded CPython 3.14t

The extension module of `zoedb` runs without the GIL. Three packages that ZODB needs do not declare this yet in their releases: `persistent`, `zodbpickle` and `BTrees`. With their stock releases on CPython 3.14t:

- the import of `zodbpickle` or of `BTrees` turns the GIL back on, with a `RuntimeWarning`. The code still runs, with the GIL;
- `persistent` can lose the state of an object when two threads load and invalidate it at the same time. The process can crash.

The repository uses forks of the three packages that fix these issues, at fixed commits. Its uv workspace installs them: see the sources in [`python/pyproject.toml`](https://github.com/adeweb-be/ZOE/blob/main/python/pyproject.toml). Install the same forks in an environment on 3.14t. The findings give the details:

- [persistent loses the state of an object on CPython 3.14t](../findings/persistent-inline-values.md)
- [zodbpickle turns the GIL on at `import ZODB` on CPython 3.14t](../findings/zodbpickle-free-threading.md)
- [BTrees turns the GIL on in a ZODB application on CPython 3.14t](../findings/btrees-free-threading.md)

In a Plone process, other C extensions, such as `ExtensionClass`, also turn the GIL back on. Plone 6.1 supports CPython up to 3.13, so a Plone site runs on 3.12 or 3.13.

## macOS

- The static binary, the server image and Loupe build on Linux only. On macOS, the wheel builds and the tests run.
- On macOS, a sync of the store is `fsync(2)`, as FileStorage does. A commit survives a crash of the process and of the system. A loss of power can lose the commits that the cache of the drive held. On Linux, a sync is `fsync` or `fdatasync`.

## One process holds a store

A store has one lock. A Zope instance in-process, a server, `zoe pack`, `zoe verify`, `zoe backup`, `zoe mcp` and Loupe each take it. So:

- the tools of the command line and Loupe run only while the instance or the server is stopped, or on a copy of the store;
- `zoe backup` has no online mode yet. Stop the server for a backup;
- `read-only true` in `<zoe>` does not let a second process open the store.

## A slow first open after a crash

After a process ends without a close, the next open repairs the database file. The repair reads every page of the file, shows no progress and cannot stop. On a large store, it takes long. Give the startup probe of a server time.

## The KV namespaces outside ZOE

A `Data.fs` holds no KV namespace and no sequence. `zoe convert --to-data-fs`, `zodbconvert`, and a copy through the ZODB iterator leave them out: the queue of queued indexing, the locks of edit forms, and the records of a moved registry. See [Plone](plone.md#uninstall) before you leave ZOE.

## Not built yet

Some parts of the design that a user can look for:

- a backup through the server, and a continuous backup;
- a Helm chart, and servers with replicas;
- the change feed, `zoe admin` and the maintenance mode of the server;
- Loupe on a server, and an editor of records;
- `zoe mcp` against a server;
- the reload of a TLS certificate without a restart.
