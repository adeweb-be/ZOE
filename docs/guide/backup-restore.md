# Back up and restore

`zoe backup` writes the store into dump files in a backup directory. `zoe restore` builds a new store from them. A dump holds the transactions with their records and blobs, the sequences, and the durable KV namespaces. So a restore gives the same tids, oids, pickles and blob files, and the data of `zoe.plone`.

## When to back up

`zoe backup` opens the store itself, and a store has one lock. So it runs only while no other process holds the store:

- in-process: stop the Zope instance;
- with a server: stop `zoe serve`.

An online backup through the server is not built yet. A store that a process holds gives this error:

```text
zoe backup: var/zoe: the store or its blob directory is locked by another open store
```

## Back up

```sh
zoe backup --path var/zoe --to backups/zoe
```

The first backup writes the full dump. Each later backup writes an incremental dump: the transactions after the last dump. With nothing new, the command writes nothing and exits with 0:

```text
Nothing new: the dumps in backups/zoe end at tid 0x040c7f8a22d772dd, the last tid of the store var/zoe
```

The output of the first backup:

```text
Backed up the store var/zoe into backups/zoe
  dump          backups/zoe/0000000000000000-040c7f8a22d772dd.zoedump (full)
  after tid     0x0000000000000000
  last tid      0x040c7f8a22d772dd
  transactions  5
  records       68
  blobs         1 (4096 bytes)
  sequences     0
  durable rows  0
```

The backup directory holds one file for each dump, named after its range of tids: `<after>-<last>.zoedump`. The command never changes a dump that it wrote. Copy the whole directory to your backup media. Keep the dumps of a chain together: a restore needs each dump from the full dump on.

### A pack and the chain of dumps

A pack keeps the chain when its pack time is at or before the last backup. For example, a daily backup and a pack with `--days 7` keep the chain.

A pack past the last backup breaks the chain: the pack removed history that the next dump must hold. The next backup then refuses, writes nothing and exits with 1:

```text
zoe backup: backups/zoe: the store was packed at tid 0x040c80440de1d2aa, after the last tid 0x040c80440d4f6f11 of the dumps; write a new full dump into a new directory
```

Then start a new chain: write a new full dump into a new backup directory, such as `zoe backup --path var/zoe --to backups/zoe-2`. Keep the old directory as long as you need the history that it holds.

## Check a backup

`zoe verify --backup` restores the dumps into a scratch copy and compares the copy with the store:

```sh
zoe verify --path var/zoe --backup backups/zoe
```

```text
Compared the dumps in backups/zoe with the store var/zoe
  dumps            1
  last tid         0x040c7f8a22d772dd
  transactions     5
  records          68
  blobs            1
  sequences        0
  durable rows     0
Differences: 0
```

The scratch copy goes to the temporary directory, or to `--scratch DIR`. It needs room for the whole store. The command removes it at the end. A difference fails the command with 1.

## Restore

```sh
zoe restore --from backups/zoe --path var/zoe-restored
```

```text
Restored the store var/zoe-restored from backups/zoe
  dumps         1
  last tid      0x040c7f8a22d772dd (2026-10-22 04:26:08.166 UTC)
  transactions  5
  records       68
  blobs         1
  sequences     0
  durable rows  0
```

- The target must not exist, or must be an empty directory. The command refuses a directory that holds a store, with `zoe restore: var/zoe-restored: holds a store; a restore never writes into a store`. To restore in place, move the damaged store away first.
- The command checks every dump before it writes anything. A damaged dump fails the restore, and the target stays empty.
- The blobs of the new store go to `blobs` in the store directory. Point `blob-dir` or `--blob-dir` there, or move the directory.

Then check the new store with `zoe verify` (see [Verify and repair](verify-repair.md)), and start the instance or the server on it.

## A `Data.fs` from a backup

The dumps are a format of ZOE. To give a backup to FileStorage, restore it, then convert the store (see [Convert](convert.md#from-a-store-to-a-datafs)).
