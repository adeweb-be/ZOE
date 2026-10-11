# 0029. The frame checksum covers the key space

Status: accepted. It amends the checksum input of ADR 0020.

## Context

ADR 0020 makes the checksum of a frame cover the key that the frame is stored under, then header bytes 0 to 23, the typed-field slots and the payload. Two pairs of frame kinds share a key. Records and blob records both use the 16-byte `(oid, tid)` key. Transaction rows and intent files both use the 8-byte tid key. Sequence rows use the UTF-8 name of the sequence, which can be any text. So the checksum proves that a frame sits under its own key, but not that it sits in its own key space.

The bit-flip harness (`crates/zoe-core/tests/bit_flip_harness.rs`) found what this costs. In a release build, a flip of byte 82408 bit 1, or of byte 82440 bit 0, of its database file moves a child pointer of the root branch page of `records` onto a leaf of `blobs`. `load` then returns the 40-byte blob record as the pickle, and the checksum passes. redb checks its page checksums only in `check_integrity`, never on a read. N13 and the phase 1 gate ask that a read never returns wrong data.

No real store exists yet. Only test and development stores use the format.

## Decision

**A tag for each key space.** Each kind of frame has a one-byte tag. The tag is not stored. It is the first byte of the checksum input.

| Tag | Key space | Key |
| --- | --- | --- |
| `0x00` | none: 0 is never a tag | |
| `0x01` | `records` | `(oid, tid)`, 16 bytes |
| `0x02` | `blobs`, the blob records | `(oid, tid)`, 16 bytes |
| `0x03` | `txns`, the transaction rows | tid, 8 bytes |
| `0x04` | the intent files in `blobs/tmp/` | tid, 8 bytes |
| `0x05` | `sequences`, the sequence rows | the UTF-8 name of the sequence |
| `0x06` | reserved for `vectors` | |
| `0x07` to `0xff` | free | |

**The checksum input.** The checksum is xxh3-64 with seed 0 over these bytes, in this order:

1. the tag of the key space, 1 byte;
2. the key, as the backend stores it: 16 bytes for `records` and `blobs`, 8 bytes for `txns` and the intent files, the UTF-8 name for `sequences`;
3. header bytes 0 to 23;
4. the typed-field slots;
5. the payload, as stored.

The header layout does not change. A frame costs no more bytes on disk.

**The reader gives the tag.** A reader passes the tag of the table or of the kind of file that it reads, never a byte from the stored data. So a frame that another key space wrote fails the checksum, and the read raises the error of the key space that it reads: `CorruptRecord` in `records`, `CorruptBlob` in `blobs`, `CorruptTxn` in `txns`, `CorruptSequence` in `sequences`. An intent file that fails its checksum names nothing, as an intent file that was never synced does, and the next writer open removes it. Because the tag is the first byte and has a fixed length, the inputs of two key spaces always differ in their first byte. A frame can pass in another key space only through a 64-bit hash collision.

**The rules of the tags.**

- A tag never changes.
- A tag is never given to another key space, also after its key space is removed.
- 0 is never a tag, so a zeroed tag never verifies.
- A new kind of frame takes a tag in the change that adds it: a reserved tag if it has one, otherwise the lowest free tag. The code adds one variant to `KeySpace` in `crates/zoe-core/src/format/mod.rs`.
- A key space whose values are not frames, such as `meta`, has no tag.

**The format name stays `ZOE format 1`.** Every frame of a store written before this change fails its checksum now. A build with this change opens such a store, and each read of a frame raises a `Corrupt*` error, never data. The format name does not change, for these reasons:

- The specification bumps the on-disk format name only with a migration tool. No store needs one, and a bump without a tool breaks that rule.
- No release has shipped `ZOE format 1`. The first release fixes it, and this change comes before it.
- A bump spends a version number on stores that do not exist.

**No migration.** Only test and development stores exist. A store of the earlier checksum input is written again from its source. No code reads the earlier input.

**Why not a prefix of the key space in each key.** Another design puts the key space in the key: a prefix byte on every key, or one table for all key spaces with that prefix. The checksum covers the key, so it would cover the key space too. This decision does not take that design:

- A prefix stores one more byte in every key of every leaf and branch page. The tag in the checksum stores nothing.
- A prefix changes the keys of the data model, and every range scan and every split of a key. The tag changes only the checksum input.
- The intent files are files, not rows. A key prefix does not reach them, but the tag does.
- With one table, a scan of one key space walks the pages of the others.

## Consequences

- A frame that a damaged branch page of redb finds in another key space, under the same key, fails its checksum. The two flips of the harness that gave a blob record as a pickle now give `CorruptRecord`.
- An exhaustive sweep of the harness database file, every single-bit flip of each non-zero byte outside the stored frames, gives no wrong read: 0 in 102,936 flips on the database of the finding, and 0 in 116,928 flips on the database with `txn_oids` and `sequences`. The harness builds its database in memory at each run, so no fixture file needs to be generated again. Only the checksum bytes of the frames change: the page layout and the offsets of the two flips stay the same.
- A flip in the pages of redb stays outside the phase 1 gate. It can still hide data, give a stale read, make the open fail, panic, abort the process or hang a read.
- Each later kind of frame must take a tag. `vectors` takes `0x06`.
- A development store written before this change is unreadable and must be written again.
