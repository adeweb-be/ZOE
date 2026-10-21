# 0023. The blob record and the blob directory

Status: accepted.

## Context

F1 asks for `IBlobStorage`, N13 for a checksum on every blob, verified on read, and N1 for a commit that survives a crash at any instruction, blobs included. ADR 0020 reserves a BLAKE3 hash in every blob record. ZODB tools and ZEO shared blob directories read blob files in ZODB's BushyLayout.

The blob file lives in the filesystem and its record in the backend. No single sync makes both durable, so the order of the steps decides what a crash can leave. N7 allows no walk of the blob tree at open: a site can hold hundreds of thousands of blobs.

The `blob-dir` key, the acceptance test of `IBlobStorage` and ZODB's blob tests put the blob directory outside the store directory.

## Decision

The blob store as built: specification, "Storage core", "Blobs", and the blob invariants in `crates/zoe-core/CLAUDE.md`.

### Layout

The blob directory holds the blob files in ZODB's BushyLayout: `.layout` holds `bushy`, and the file of `(oid, tid)` is one directory per big-endian byte of the oid, then the tid as `tid_repr` writes it. `tmp/` holds the uncommitted blob files and the intent files. By default the blob directory is `blobs/` in the store directory. Below, `blobs/` names it, wherever it is.

A committed blob file is read-only and never changes. The first blob creates `blobs/`, `blobs/tmp/` and the layout file, so a store without blobs has no blob directory. An open refuses a layout file that does not say `bushy`.

### The blob record

The `blobs` key space has the `(oid, tid)` key of `records`. Its value is a frame (ADR 0020, tag 2) whose 40-byte payload is the BLAKE3 hash of the file, then its size as a little-endian `u64`. The row holds no file name: the layout gives it. A new store creates the table; a store without it gets it at its first blob commit.

### The hash goes in the backend transaction

`WriteTxn::put_blob` stages the blob record in the write transaction that holds the record, and `Backend::load_blob` reads it. Every backend implements both. So the hash is durable in the same commit as the record.

### Finish

`store_blob` computes the hash while the client writes the temporary file, then syncs the file. After finish allocates the tid, and before the backend commit, it writes and syncs the intent file `blobs/tmp/0x<16 hex digits of the tid>.intent` and syncs `blobs/tmp/`. The intent file is a frame under the tid key (tag 4) whose payload is the oids of the blobs, 8 big-endian bytes each. Then it creates the missing oid directories, each synced into its parent, and makes each blob file read-only, renames it into place and syncs its directory. Last, it commits and removes the intent file without a sync.

### Recovery

A writer open runs the recovery only when the layout file exists. It reads only `blobs/tmp/` and the oid directories that its intent files name, so its work does not grow with the store. It removes each named blob file that has no blob record and syncs its directory, then removes the intent files and the temporary files. A read-only open only checks the layout file: a blob file without its blob record stays invisible, because every blob read starts from its record.

A directory that exists is not always durable: a killed process can stop between a mkdir and the sync of its parent. So the first blob file of a process, and the recovery, sync the parent directories again even when they exist.

### Reads

`open_blob` hashes the bytes as it reads them and checks the size and the hash at the end of the file. A mismatch fails that read with `InvalidData` that holds `CorruptBlob { oid, tid }`. `verify_blob` reads the whole file. A missing file is also `CorruptBlob`.

### The blob directory is an option of the core

`Options::blob_dir` gives the blob directory, and `None` keeps `blobs` in the store directory. `zoe.Storage(path, blob_dir=...)` and the `blob-dir` key map to it (specification, "In-process mode", "Opening").

**The names do not change.** The crate-private `Relocated` maps `blobs` and its names onto the blob directory, and the store directory `""` onto its parent. Every other name stays in the store directory, which keeps the store lock. All goes through the `Vfs`, so the crash simulation covers it. No `Vfs` or `Backend` signature and no on-disk format changes.

**The open checks the path first.** Before it creates anything, `Store::open_with` refuses a path that does not end in a UTF-8 name and an existing entry that is not a directory.

**The store does not record its blob directory.** Each open must give the same one, as with FileStorage. A writer open refuses with `MissingBlobDir` a store with blob records (`Backend::has_blobs`) whose blob directory has no layout file, because the first blob makes the layout file durable before any blob record. zoe-py maps it to `StorageError`.

**One open store per blob directory.** A writer takes an exclusive `flock` on the blob directory itself (`Vfs::lock_dir`) before it reads or removes anything there, and keeps it until the store closes. Another holder fails with `Locked`. A filesystem that cannot lock a directory fails the open.

**A blob directory without a layout file is new or foreign.** The first blob writes the layout file before any other file, so a blob directory without it may hold nothing, an empty `tmp/`, or a `tmp/` with only `layout.tmp`. The open and the first blob refuse anything else with `NotBlobDir`, and change nothing in it.

### Why a crash at any point is safe

A crash before the backend commit leaves only temporary files and blob files that an intent file names, and the next writer open removes them. After the commit, every blob record has its synced file. The open syncs its removals before it removes an intent file. The crash simulation checks every crash image of a blob commit and of a recovery.

## Consequences

- After any crash, every blob record has its file. A blob file without a record lives until the next writer open.
- A blob commit costs one sync per blob file, one for the intent file, one for `blobs/tmp/`, one per oid directory and one per new directory, on top of the backend commit.
- A partial read, or a client that opens the path from `blob_path`, checks nothing. `verify_blob` and scrub cover them.
- The S3 blob store and dedup by hash (F38) can use the hash in every blob record.
- A copy of a store with another blob directory needs a copy of that directory, as with FileStorage.
- A wrong or missing `blob_dir` fails the open of a store with blobs, and deletes no blob file.
- One case stays open: another store that opens by mistake with the blob directory of a crashed store can remove a blob file that the crashed store committed. Only a store identity, a format change, would tell them apart. So each store needs its own blob directory.

## Alternatives

- **The hash in the typed-field slots of the record frame.** A bigger trait change, and pack and the S3 store lose the `blobs` key space.
- **A hash file beside each blob file.** Outside the data model and its checksum, twice the files, no dedup by hash.
- **A walk of the blob tree at open, with no intent file.** It saves two syncs per commit but breaks N7.
- **A symbolic link `<path>/blobs` to another directory.** A copy of the store keeps the link, so its recovery can delete committed blob files of the original. `zope.testing` cannot remove the link.
- **The blob directory in `meta`.** A data-model change, and a copied store still points at the original.
- **A lock file in the blob directory.** A format change; the lock on the directory does the same work.
- **A store identity in the blob directory.** A format change.
- **Only `<path>/blobs`.** The acceptance test and the configuration need another directory.

## Merged ADRs

- ADR 0027, the blob directory is an option of the core: Decision, "The blob directory is an option of the core", with the default place in "Layout" and the recovery rule in "Recovery".
