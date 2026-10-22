# Pack

A pack removes old revisions of the objects, and the objects that no object refers to any more. ZOE packs as ZODB packs a FileStorage: a pack as of a time keeps each revision after that time and, of the older revisions, the newest one of each object that a reader at that time can reach. A pack also removes the blob files of the records that it removes.

After a pack, you cannot undo a transaction at or before the pack time.

## Pack a store that runs

A pack runs online: the clients read and commit while it runs.

- In-process, and from a client of `zoe serve`: the pack form of the Zope management interface, in the Control Panel, under the database. It packs as of a number of days ago.
- Through `zoe serve`, from the command line: the `zeopack` script of ZEO packs a store of the server.

```sh
zeopack -d 7 127.0.0.1:8100
```

For a named store, add its storage name: `127.0.0.1:8100:catalog`. `zeopack` connects read-only. The server lets a read-only connection pack, as ZEO does. A read-only store, and a client of `--tls-read-only-client-ca`, cannot pack.

## Pack a stopped store

`zoe pack` opens the store itself. It runs only while no other process holds the store: stop the Zope instance or the server first.

See first what the pack would remove, and change nothing:

```sh
zoe pack --path var/zoe --days 0 --dry-run
```

```text
A pack of the store var/zoe as of tid 0x040c7f8a35ed8685 would remove:
  records  44 (4661 bytes)
  objects  21
  blobs    0 (0 bytes)
  by class:
    records  bytes  class
         44   4661  persistent.mapping.PersistentMapping
```

Then pack:

```sh
zoe pack --path var/zoe --days 0
```

```text
Packed the store var/zoe as of tid 0x040c7f8a35f72679. It removed:
  records  44 (4661 bytes)
  objects  21
  blobs    0 (0 bytes)
  by class:
    records  bytes  class
         44   4661  persistent.mapping.PersistentMapping
```

`--days N` packs as of N days ago, as `DB.pack(days=N)` of ZODB does. `--days 0` packs as of now, as in the example. Keep some days of history, such as `--days 7`, to undo recent transactions.

## Without garbage collection

A pack with garbage collection removes the objects that no object of the store refers to. A Plone site with mount points has references from one database to another, which the pack of one store does not see. So pack each store of such a site without garbage collection. Such a pack removes old revisions only, as the `pack-gc false` of FileStorage does.

The store does not record this choice. Give it to the process that packs:

| Where | Setting |
| --- | --- |
| `zoe pack` | `--pack-gc=false` |
| `<zoe>` | `pack-gc false` |
| `zoe serve` | `--pack-gc=false` for every store, or `pack-gc = false` in the table of one store of the configuration file |

Write the boolean option of `zoe` with `=`: `--pack-gc false` is a usage error.

## A stopped pack

A stop of the server stops a pack between two batches. A later pack at the same time removes what the first one left. A record that fails its checksum stops the pack, which then removes nothing more. Check the store, and restore it from a backup when the check finds damage (see [Verify and repair](verify-repair.md)).
