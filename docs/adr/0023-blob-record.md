# 0023. The blob record

Status: accepted

## Context

F1 asks for `IBlobStorage`. N13 asks for a checksum on every blob, verified on read. N19 and ADR 0016 reserve a BLAKE3 hash in every blob record from the first store written. N1 asks that an acknowledged commit survive a crash at any instruction, blobs included.

A blob is a file beside the store, immutable per `(oid, tid)`, so that ZODB tools and ZEO shared blob directories can read it. ZODB's BushyLayout is the layout those tools know. The `blobs` row of the data model listed a file name, a size and a checksum, with no byte layout, and the specification gave the path `blobs/<oid hex>/<tid hex>.blob`, which is not BushyLayout.

The blob file lives in the filesystem and its record lives in the backend. No single sync makes both durable, so the order of the steps decides what a crash can leave behind.

## Decision

**Layout.** The blob store is the directory `blobs/` in the store directory, in ZODB's BushyLayout:

- `blobs/.layout` holds `bushy`.
- The file of `(oid, tid)` is `blobs/0xAA/0xBB/…/0xHH/<tid>.blob`: one directory for each of the 8 big-endian bytes of the oid, then the tid as ZODB's `tid_repr` writes it. That is 16 hex digits for every tid that the core allocates.
- `blobs/tmp/` holds the files of uncommitted blobs and the intent files.

A committed blob file is read-only and never changes. The core creates `blobs/`, `blobs/tmp/` and the layout file with the first blob, so a store without blobs has no blob directory. The layout file is written under a temporary name and renamed into place, so a crash cannot leave a partial one. An open refuses a layout file that does not say `bushy`.

**The blob record.** The `blobs` key space is a fourth redb table:

- The key is the 16-byte big-endian `(oid, tid)`, the same key as in `records`.
- The value is a frame (ADR 0020) with previous serial 0, no flags, codec 0 and no typed-field slots.
- The payload is 40 bytes: the BLAKE3 hash of the blob file (32 bytes), then the size of the file in bytes as a little-endian `u64`.

The xxh3 checksum of the frame covers the key, so a damaged hash, or a row stored under another key, is an error and never a wrong hash. The row holds no file name, because the layout gives the name from `(oid, tid)`. A store written before this table has no blob records. The first write of a blob record creates the table, so the format name stays `ZOE format 1`.

**The hash goes in the backend transaction.** `WriteTxn::put_blob` stages the blob record in the write transaction that holds the record, and `Backend::load_blob` reads it. So the hash becomes durable in the same commit as the record, never before and never after. These are two new required methods. No existing signature changes.

**Finish.** `store_blob` syncs the temporary file. The hash is computed while the client writes the file, so finish does not read the file again. After finish allocates the tid and before the backend commit, it does these steps in order:

1. It writes the intent file `blobs/tmp/0x<16 hex digits of the tid>.intent`, syncs it and syncs `blobs/tmp/`. The intent is a frame under the 8-byte tid key. Its payload is the oids of the blobs, 8 big-endian bytes each.
2. For each blob, it creates the missing oid directories one level at a time and syncs the parent of each new one. Then it makes the file read-only, renames it into its place and syncs the oid directory.
3. It commits the backend write transaction, which holds the records, the blob records, the transaction row and the meta update.
4. It removes the intent file without a sync.

**Recovery.** Opening a store reads only `blobs/tmp/`. For each intent file, it removes every named blob file that has no blob record and syncs that file's directory. Then it removes the intent files and the temporary files and syncs `blobs/tmp/`. An intent file whose frame does not decode was never synced, so its finish renamed no file, and the open only removes it.

**Reads.** `open_blob` returns a reader that hashes the bytes as it reads them. At the end of the file it compares the size and the hash with the blob record. A mismatch fails that read with `InvalidData`, and the error holds `CorruptBlob { oid, tid }`. `verify_blob` reads the whole file and returns `CorruptBlob`. A missing file is also `CorruptBlob`.

## Why a crash at any point is safe

- **Before the intent file is durable.** No file has been renamed yet. The open removes the temporary files. The commit is absent and was not acknowledged.
- **After the intent file, before the backend commit.** Some blob files can be in place without a blob record. The intent file names them, and the open removes them. The commit is absent and was not acknowledged.
- **After the backend commit.** Each blob file and its directory entry were synced before the commit, so every blob record has its file. If the intent file is still there, it names files that have their blob records, and the open keeps them.

The crash simulation checks this. `MemVfs` records each sync per directory, and the acceptance test opens the store on every crash image of a blob commit.

## Why the open reads only `blobs/tmp/`

N7 bounds the startup time of a large store and allows no index rebuild. A walk of the whole blob tree costs time in proportion to the number of blobs, and a content site can hold hundreds of thousands of them. The intent file names exactly the files that a crash can leave without a record. So the work of an open depends only on the finishes that were in progress at the crash, not on the size of the store.

## Alternatives

- **The hash in the typed-field slots of the record frame.** The hash would be durable with the record by construction. But `put_record` and `Record` would change, which is a bigger trait change. The typed-field room would also hold blob data, and pack and the S3 store would lose the `blobs` key space that they need.
- **A hash file beside each blob file.** The traits would not change. But the hash would not be in the data model, the record checksum would not cover it, the blob tree would hold twice as many files, and dedup by hash (F38) could not use it.
- **A walk of the blob tree at open, with no intent file.** It saves two syncs per blob commit, but it breaks N7 on a large store.

## Consequences

- After any crash, every blob record has its file, and a blob file without a record lives only until the next open.
- A blob commit costs these syncs on top of the backend commit: one for each blob file, one for the intent file, one for `blobs/tmp/`, one for each oid directory, and one for the parent of each new directory.
- A read through `open_blob` verifies the blob. A reader that stops before the end, or a client that opens the path from `blob_path` itself, checks nothing. `verify_blob` and scrub cover those cases.
- ZODB tools and ZEO shared blob directories can read `blobs/` as it is.
- The S3 blob store and dedup by hash (F38) can use the hash that is already in every blob record.
- Every backend implements `put_blob` and `load_blob`.
