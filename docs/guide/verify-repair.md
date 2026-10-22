# Verify and repair

`zoe verify` checks a store and changes nothing. Run it after a conversion, after a restore, and when you suspect damage.

## Verify a store

`zoe verify` opens the store itself, and a store has one lock. Stop the Zope instance or the server first.

```sh
zoe verify --path var/zoe
```

```text
Verified the store var/zoe
  transactions     5
  sequences        0
  ephemeral keys   0
  durable keys     0
  expired keys     0
  records          68
  objects          45
  blobs            1
  object count     45
Findings: 0
  checksums        0
  blob files       0
  references       0
  pickles          0
  unknown classes  0
  count mismatch   0
Classes: 2
   1  ZODB.blob.Blob
  44  persistent.mapping.PersistentMapping
```

The command checks:

- the checksum of each record;
- the blob file of each blob record: its size and its hash;
- the references: each object that a record refers to has a record;
- each pickle: its class decodes;
- the object count of the store.

Each problem is a finding. A finding fails the command with exit code 1. The report goes to stdout.

| Option | Use it to |
| --- | --- |
| `--blob-dir DIR` | check a store whose blobs are not in `blobs` in the store directory |
| `--classes FILE` | also report each class of the store that your application does not provide. The file names one class on each line, as `module.name`. |
| `--format json` | get one JSON document for a program, with oids and tids as integers |
| `--backup DIR` | compare the dumps of a backup directory with the store (see [Back up and restore](backup-restore.md#check-a-backup)) |

## A store that was not closed cleanly

A process that ends without a close leaves the database file open: after SIGKILL, a crash or a loss of power. Every commit that a client saw complete is in the file. But the file needs a repair before a read-only open.

The repair happens on its own:

- A writer, such as the Zope instance or `zoe serve`, repairs the file at its next open. The open then reads every page of the file, and takes longer on a large store.
- A tool that only reads, such as `zoe verify`, `zoe backup`, `zoe mcp` or Loupe, repairs the file first, then opens it read-only. It writes one line that says so:

  ```text
  zoe verify: var/zoe: the store was not closed cleanly: repaired the store
  ```

The repair writes only the database file `zoe.redb`. It keeps every durable commit and changes no data. On a store that was closed cleanly, a tool changes no byte.

The repair needs write access to `zoe.redb`. On a read-only file system, or for a user that cannot write the file, it fails, and the tool exits with 1:

```text
zoe verify: var/zoe: the store was not closed cleanly, and its repair failed: Permission denied (os error 13)
```

Copy the store to a writable place, or open it once with a writer.

## A finding

A finding means damage or a missing part:

| Finding | What to do |
| --- | --- |
| a record or a blob that fails its check | Restore the store from a backup. Check the backup first with `zoe verify --backup`. |
| a blob file that is missing | Check `--blob-dir`: the store does not record its blob directory. Then restore the blob directory from a backup. |
| a reference to an object without a record | An object is gone that another object still refers to. Find the object that refers to it with Loupe, then restore the store from a backup. |
| a class that the application does not provide | Install the add-on that provides the class, or remove its objects in the application. |

No tool of ZOE edits records yet. Loupe and `zoe mcp` read a store and show each object and its history (see [Loupe](loupe.md) and [MCP](mcp.md)).
