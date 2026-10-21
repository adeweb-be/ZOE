# 0023. The blob record and the blob directory

Status: accepted. The section "The blob directory is an option of the core" is built (`Options::blob_dir` in `crates/zoe-core/src/store.rs`, `Relocated` in `crates/zoe-core/src/blob/mod.rs`) and waits for the confirmation of the human.

## Context

F1 asks for `IBlobStorage`. N13 asks for a checksum on every blob, verified on read. N19 and ADR 0020 reserve a BLAKE3 hash in every blob record from the first store written. N1 asks that an acknowledged commit survive a crash at any instruction, blobs included.

A blob is a file beside the store, immutable per `(oid, tid)`, so that ZODB tools and ZEO shared blob directories can read it. ZODB's BushyLayout is the layout those tools know. The `blobs` row of the data model lists a file name, a size and a checksum, with no byte layout, and the path `blobs/<oid hex>/<tid hex>.blob` is not BushyLayout.

The blob file lives in the filesystem and its record lives in the backend. No single sync makes both durable, so the order of the steps decides what a crash can leave behind.

The configuration of the specification has `blob-dir`, and the acceptance test of `IBlobStorage` opens `zoe.Storage(path, blob_dir=...)` with a blob directory outside the store directory. ZODB's own blob tests do the same: their storages keep the blobs in a sibling directory.

## Decision

### Layout

The blob directory holds the blob files in ZODB's BushyLayout. By default it is `blobs/` in the store directory; the option of the section "The blob directory is an option of the core" puts it elsewhere. Below, `blobs/` names the blob directory, wherever it is.

- `blobs/.layout` holds `bushy`.
- The file of `(oid, tid)` is `blobs/0xAA/0xBB/…/0xHH/<tid>.blob`: one directory for each of the 8 big-endian bytes of the oid, then the tid as ZODB's `tid_repr` writes it. That is 16 hex digits for every tid that the core allocates.
- `blobs/tmp/` holds the files of uncommitted blobs and the intent files.

A committed blob file is read-only and never changes. The core creates `blobs/`, `blobs/tmp/` and the layout file with the first blob, so a store without blobs has no blob directory. The layout file is written under a temporary name and renamed into place, so a crash cannot leave a partial one. An open refuses a layout file that does not say `bushy`.

### The blob record

The `blobs` key space is a redb table:

- The key is the 16-byte big-endian `(oid, tid)`, the same key as in `records`.
- The value is a frame (ADR 0020) with previous serial 0, no flags, codec 0 and no typed-field slots. Its checksum input starts with the tag 2 of `blobs`.
- The payload is 40 bytes: the BLAKE3 hash of the blob file (32 bytes), then the size of the file in bytes as a little-endian `u64`.

The xxh3 checksum of the frame covers the key, so a damaged hash, or a row stored under another key, is an error and never a wrong hash. The row holds no file name, because the layout gives the name from `(oid, tid)`. A new store gets an empty `blobs` table when it is created, with the other tables. A store without that table gets it at its first blob commit; until then it reads as a store without blob records. The format name does not change for it.

### The hash goes in the backend transaction

`WriteTxn::put_blob` stages the blob record in the write transaction that holds the record, and `Backend::load_blob` reads it. So the hash becomes durable in the same commit as the record, never before and never after. Every backend implements these two required methods.

### Finish

`store_blob` syncs the temporary file. The hash is computed while the client writes the file, so finish does not read the file again. After finish allocates the tid and before the backend commit, it does these steps in order:

1. It writes the intent file `blobs/tmp/0x<16 hex digits of the tid>.intent`, syncs it and syncs `blobs/tmp/`. The intent is a frame under the 8-byte tid key, with the tag 4 of the intent files. Its payload is the oids of the blobs, 8 big-endian bytes each.
2. For each blob, it creates the missing oid directories one level at a time and syncs the parent of each new one. Then it makes the file read-only, renames it into its place and syncs the oid directory.
3. It commits the backend write transaction, which holds the records, the blob records, the transaction row and the meta update.
4. It removes the intent file without a sync.

### Recovery

A writer open runs the recovery only when the layout file exists. It reads only `blobs/tmp/` and the oid directories that its intent files name. First it syncs, from the top down, the parent of each existing level of each of those oid directories. Then it removes every named blob file that has no blob record, and syncs the directory of each named oid without a blob record, also when the file is already gone: an open that a crash stopped can have removed the file without that sync, and a power loss would bring it back. Last, it removes the intent files and the temporary files and syncs `blobs/tmp/`. It removes only regular files there, and leaves any other entry. An intent file whose frame does not decode was never synced, so its finish created no directory and renamed no file, and the open only removes it. A read-only open, such as the one of Loupe, does none of this: it only checks the layout file. A blob file without its blob record stays invisible to it, because every blob read starts from the blob record, and the next writer open removes the file.

### A directory that exists is not always durable

A process killed between a mkdir and the sync of its parent leaves, in the page cache, a directory that a later power loss can remove. A new process that finds the directory does not create it again, so it must sync it on its own. The first blob file of a process syncs the parents of `blobs/` and `blobs/tmp/` even when they exist. A finish creates oid directories only after its intent file is durable, and a killed finish leaves its intent file, so the syncs of the open cover every oid directory that a killed process can leave.

### Reads

`open_blob` returns a reader that hashes the bytes as it reads them. At the end of the file it compares the size and the hash with the blob record. A mismatch fails that read with `InvalidData`, and the error holds `CorruptBlob { oid, tid }`. `verify_blob` reads the whole file and returns `CorruptBlob`. A missing file is also `CorruptBlob`.

### The blob directory is an option of the core

**The path is an option of the core.** `Options::blob_dir` gives the blob directory, and `None` keeps `blobs` in the store directory. `zoe.Storage(path, blob_dir=...)` passes its argument there, and the `blob-dir` key of the configuration maps to it.

**The names of the blob store do not change.** The crate-private `Relocated` maps `blobs` and the names in `blobs/` onto the blob directory, and the store directory `""`, the parent of `blobs`, onto the directory that holds the blob directory. So the sync of the parent of `blobs`, which the first blob file of a process makes, syncs the directory that holds the blob directory. Every other name stays in the store directory, which keeps the lock of the store. Every operation goes through a `Vfs`, so the crash simulation covers a blob directory outside the store directory as it covers `blobs/`. No `Vfs` or `Backend` signature changes, and the on-disk format does not change.

**The open checks the path first.** `Store::open_with` refuses, before it creates anything, a path that does not end in a UTF-8 name and an existing entry that is not a directory. Then it creates the directory that holds the blob directory, and then the store directory, each with a sync of its parent. So a blob directory that fails leaves no lock file and no database in the store directory. The blob directory itself comes with the first blob, as `blobs/` does.

**The store does not record its blob directory.** Each open must give the same one, as with FileStorage. A writer open refuses with `MissingBlobDir` a store that has blob records while its blob directory has no layout file. The first blob of a store makes the layout file durable before any blob record, so this blob directory is not the one of the store: the option is missing or wrong, or the directory is not mounted. Without the check, the writer would start a second blob directory, and its recovery would never see the intent files of the first one. `Backend::has_blobs` tells if the store has a blob record. A store without blob records takes any blob directory. zoe-py maps `MissingBlobDir` to `StorageError`.

**The blob directory takes one open store.** A writer takes an exclusive `flock` on a descriptor of the blob directory itself, through `Vfs::lock_dir`: at its open when the directory exists, and in the first blob right after it creates the directory. It takes the lock before it reads or removes anything there, and keeps it until the store closes. Another open store that holds the lock fails the open, or the first blob, with `Locked`, and removes nothing. The lock adds no file to the directory, so the format does not change. A filesystem that cannot lock a directory, as some network filesystems, fails the open with its error: the store never runs without the lock.

**A blob directory without a layout file is new or foreign.** The first blob writes `tmp/layout.tmp`, syncs it and `tmp/`, renames it to `.layout` and syncs the blob directory, before any temporary file or intent file exists. So a writer open runs the recovery only when the layout file exists. A blob directory without it may hold nothing, an empty `tmp/`, or a `tmp/` with only `layout.tmp`, which a stopped first blob leaves. Anything else is a directory of another program: the open and the first blob refuse it with `NotBlobDir`, and never remove its files or add a layout file to it.

### Why a crash at any point is safe

- **Before the intent file is durable.** No file has been renamed yet. The open removes the temporary files. The commit is absent and was not acknowledged.
- **After the intent file, before the backend commit.** Some blob files can be in place without a blob record. The intent file names them, and the open removes them. The commit is absent and was not acknowledged.
- **After the backend commit.** Each blob file and its directory entry were synced before the commit, so every blob record has its file. If the intent file is still there, it names files that have their blob records, and the open keeps them.
- **During the open.** The removals of the orphan files are synced before the removal of their intent file. So a crash during the open leaves either the intent file, and the next open removes the orphans, or no orphan. The next open syncs the directory of each orphan also when the file is gone, because the removal of the stopped open can sit in the page cache only.
- **A killed process, a restart, then a power loss.** The killed process leaves operations that are not synced. The open and the first blob file of the next process sync the directories that it can leave, so a later commit does not depend on them.

The crash simulation checks this. `MemVfs` records each sync per directory. The acceptance test opens the store on every crash image of a blob commit. Unit tests open every image of a crash during recovery, and every image after a process is killed at a directory sync and the next process commits a blob.

### Why the open reads only `blobs/tmp/`

N7 bounds the startup time of a large store and allows no index rebuild. A walk of the whole blob tree costs time in proportion to the number of blobs, and a content site can hold hundreds of thousands of them. The intent file names exactly the files that a crash can leave without a record. So the work of an open depends only on the finishes that were in progress at the crash, not on the size of the store.

## Consequences

- After any crash, every blob record has its file, and a blob file without a record lives only until the next writer open.
- A blob commit costs these syncs on top of the backend commit: one for each blob file, one for the intent file, one for `blobs/tmp/`, one for each oid directory, and one for the parent of each new directory. The first blob file of a process adds two directory syncs.
- A read through `open_blob` verifies the blob. A reader that stops before the end, or a client that opens the path from `blob_path` itself, checks nothing. `verify_blob` and scrub cover those cases.
- ZODB tools and ZEO shared blob directories can read the blob directory as it is.
- The S3 blob store and dedup by hash (F38) can use the hash that is already in every blob record.
- The blob files, their temporary directory, the intent files and every rename stay in the blob directory, on its device. A commit still renames each blob file.
- A copy of a store directory with the default blob directory copies its blobs. A store with another blob directory needs a copy of that directory too, and the copy must open with its own copy, as with FileStorage.
- A forgotten or wrong `blob_dir`, or a blob directory that is not mounted, fails the open of a store with blobs. It never deletes a blob file.
- Two open stores never share a blob directory: the second one gets `Locked`.
- One sequential case remains. A store that crashed leaves an intent file in its blob directory. If another store opens later with that blob directory by mistake, a copy of the store directory with the same `blob_dir` for example, its recovery reads the intent file against its own database. It can remove a blob file that the crashed store committed. Only a store identity, written in the store and in the blob directory, would tell the two stores apart. That is a format change, left for later. Until then, each store needs its own blob directory, and a copy of a store needs a copy of its blob directory.
- A directory of another program given as `blob_dir` keeps its files: the store refuses it.
- ZODB's blob tests can remove their directories.

## Alternatives

- **The hash in the typed-field slots of the record frame.** The hash would be durable with the record by construction. But `put_record` and `Record` would change, which is a bigger trait change. The typed-field room would also hold blob data, and pack and the S3 store would lose the `blobs` key space that they need.
- **A hash file beside each blob file.** The traits would not change. But the hash would not be in the data model, the record checksum would not cover it, the blob tree would hold twice as many files, and dedup by hash (F38) could not use it.
- **A walk of the blob tree at open, with no intent file.** It saves two syncs per blob commit, but it breaks N7 on a large store.
- **A symbolic link `<path>/blobs` to another blob directory**, with no core change. A copy of the store directory with `cp -a`, rsync or tar keeps the link, so the copy shares the blob directory of the original. The open of the copy then runs the blob recovery against the older database of the copy: a pending blob of the original vanishes, and with an intent file left behind, a committed blob file of the original is deleted. A read-only `zoe.Storage` triggers it too, because it opens the store as a writer. Tools that walk the store directory see the link, and the `rmtree` of `zope.testing` cannot remove a symbolic link to a directory.
- **The blob directory in `meta`.** The store would find its blob directory alone. But it is a data-model change, a red zone, and a copied store would still point at the blob directory of the original.
- **A lock file in the blob directory.** It would stop two open stores from sharing one blob directory, as the lock on the directory does. But it adds a file to the layout of the blob directory, which is a format change.
- **A store identity in the blob directory.** It would also stop a store from using the blob directory of another store after that store closed. It is a format change, and it is not built.
- **Only `<path>/blobs`.** The acceptance test and the configuration need another directory.

## Merged ADRs

- ADR 0027, the blob directory is an option of the core: Decision, "The blob directory is an option of the core", with the default place in "Layout" and the recovery rule in "Recovery".
