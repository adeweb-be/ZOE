# 0027. The blob directory

Status: proposed. It was decided while the human was away, and the human confirms it. It amends the layout of ADR 0023: `blobs/` is the default place of the blob directory.

## Context

ADR 0023 puts the blob files in `blobs/` in the store directory. The configuration of the specification also has `blob-dir`, and the acceptance test of `IBlobStorage` opens `zoe.Storage(path, blob_dir=...)` with a blob directory outside the store directory. ZODB's own blob tests do the same: their storages keep the blobs in a sibling directory.

The first build made another `blob_dir` a symbolic link `<path>/blobs` to it, and left the core unchanged. The review found data loss with it. A copy of the store directory, made with `cp -a`, rsync or tar, kept the link, so the copy shared the blob directory of the original. The open of the copy ran the blob recovery against the older database of the copy. A pending blob of the original vanished, and with an intent file left behind, a committed blob file of the original was deleted. A read-only `zoe.Storage` triggered it too, because it opens the store as a writer. The link also stopped the cleanup of ZODB's blob tests: the `rmtree` of `zope.testing` cannot remove a symbolic link to a directory.

## Decision

**The path is an option of the core.** `Options::blob_dir` gives the blob directory, and `None` keeps `blobs` in the store directory. `zoe.Storage(path, blob_dir=...)` passes its argument there, and the `blob-dir` key of the configuration maps to it.

**The names of the blob store do not change.** The crate-private `Relocated` maps `blobs` and the names in `blobs/` onto the blob directory, and the store directory `""`, the parent of `blobs`, onto the directory that holds the blob directory. So the sync of the parent of `blobs`, which the first blob file of a process makes, syncs the directory that holds the blob directory. Every other name stays in the store directory, which keeps the lock of the store. Every operation goes through a `Vfs`, so the crash simulation covers a blob directory outside the store directory as it covers `blobs/`. No `Vfs` or `Backend` signature changes, and the on-disk format does not change.

**The open checks the path first.** `Store::open_with` refuses, before it creates anything, a path that does not end in a UTF-8 name and an existing entry that is not a directory. Then it creates the directory that holds the blob directory, and then the store directory, each with a sync of its parent. So a blob directory that fails leaves no lock file and no database in the store directory. The blob directory itself comes with the first blob, as `blobs/` does.

**The store does not record its blob directory.** Each open must give the same one, as with FileStorage. A writer open refuses with `MissingBlobDir` a store that has blob records while its blob directory has no layout file. The first blob of a store makes the layout file durable before any blob record, so this blob directory is not the one of the store: the option is missing or wrong, or the directory is not mounted. Without the check, the writer would start a second blob directory, and its recovery would never see the intent files of the first one. `Backend::has_blobs` tells if the store has a blob record. A store without blob records takes any blob directory. zoe-py maps `MissingBlobDir` to `StorageError`.

**The blob directory takes one open store.** A writer takes an exclusive `flock` on a descriptor of the blob directory itself, through `Vfs::lock_dir`: at its open when the directory exists, and in the first blob right after it creates the directory. It takes the lock before it reads or removes anything there, and keeps it until the store closes. Another open store that holds the lock fails the open, or the first blob, with `Locked`, and removes nothing. The lock adds no file to the directory, so the format does not change. A filesystem that cannot lock a directory, as some network filesystems, fails the open with its error: the store never runs without the lock.

**A blob directory without a layout file is new or foreign.** The first blob writes `tmp/layout.tmp`, syncs it and `tmp/`, renames it to `.layout` and syncs the blob directory, before any temporary file or intent file exists. So a writer open runs the recovery only when the layout file exists. A blob directory without it may hold nothing, an empty `tmp/`, or a `tmp/` with only `layout.tmp`, which a stopped first blob leaves. Anything else is a directory of another program: the open and the first blob refuse it with `NotBlobDir`, and never remove its files or add a layout file to it.

## Alternatives

- **The symbolic link.** It needs no core change, but a copy of the store directory shares the blob directory of the original, which loses data, and tools that walk the store directory see the link.
- **The blob directory in `meta`.** The store would find its blob directory alone. But it is a data-model change, a red zone, and a copied store would still point at the blob directory of the original.
- **A lock file in the blob directory.** It would stop two open stores from sharing one blob directory, as the lock on the directory does. But it adds a file to the layout of the blob directory, which is a format change.
- **A store identity in the blob directory.** It would also stop a store from using the blob directory of another store after that store closed. It is a format change, and it is not built.
- **Only `<path>/blobs`.** The acceptance test and the configuration need another directory.

## Consequences

- The blob files, their temporary directory, the intent files and every rename stay in the blob directory, on its device. A commit still renames each blob file.
- A copy of a store directory with the default blob directory copies its blobs. A store with another blob directory needs a copy of that directory too, and the copy must open with its own copy, as with FileStorage.
- A forgotten or wrong `blob_dir`, or a blob directory that is not mounted, fails the open of a store with blobs. It never deletes a blob file.
- Two open stores never share a blob directory: the second one gets `Locked`.
- One sequential case remains. A store that crashed leaves an intent file in its blob directory. If another store opens later with that blob directory by mistake, a copy of the store directory with the same `blob_dir` for example, its recovery reads the intent file against its own database. It can remove a blob file that the crashed store committed. Only a store identity, written in the store and in the blob directory, would tell the two stores apart. That is a format change, left for later. Until then, each store needs its own blob directory, and a copy of a store needs a copy of its blob directory.
- A directory of another program given as `blob_dir` keeps its files: the store refuses it.
- ZODB's blob tests can remove their directories again.
