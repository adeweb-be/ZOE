# 0009. Checksums and zstd in the on-disk format

Status: accepted

## Context

A silent bit flip in a ZODB store surfaces later as a `POSKeyError` or as wrong data. Pickle-heavy Plone stores compress well, and a smaller store keeps more objects in the page cache.

## Decision

Every record and every blob chunk is framed with flags, length, checksum and payload. The checksum is verified on every read; a mismatch raises a `CorruptRecord` error with the oid and tid. The payload is zstd-compressed with a dictionary trained on the store's own records. The format carries this from the first store written.

## Consequences

- Corruption is detected on read and by scrub, never returned as data.
- A store written by phase 1 needs no migration when these features are used.
- The format is ZOE's own; tools read it through the core, not as raw pickles.
