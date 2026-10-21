# 0020. The record frame: the header, the checksum with its key space, and the codecs

Status: accepted.

## Context

N13 asks for a checksum on every record, N14 for compression and encryption, and N19 for a codec byte, a branch id and typed-field slots on every record from the first store written. A silent bit flip in a ZODB store surfaces later as a `POSKeyError` or as wrong data. Several planned features need per-record data: codecs for compression, forks, typed fields, blob deduplication. A field added to every record later is a format migration on every store. So the first store needs one layout that leaves room for all of them.

Several kinds of frames share a key shape. Records and blob records both use the 16-byte `(oid, tid)` key. Transaction rows and intent files both use the 8-byte tid key. Sequence rows use the UTF-8 name of the sequence, which can be any text. A checksum over the key proves that a frame sits under its own key, but not that it sits in its own key space. The bit-flip harness (`crates/zoe-core/tests/bit_flip_harness.rs`) shows the cost. In a release build, a flip of byte 82408 bit 1, or of byte 82440 bit 0, of its database file moves a child pointer of the root branch page of `records` onto a leaf of `blobs`. `load` then returns the 40-byte blob record as the pickle, and a checksum without the key space passes. redb checks its page checksums only in `check_integrity`, never on a read. N13 and the phase 1 gate ask that a read never returns wrong data.

Pickle-heavy Plone stores compress well. A smaller store keeps more objects in the page cache, and a commit of small records writes fewer bytes. But zstd costs CPU on each read and each write. On the development laptop, the zstd decompression of one 100 KB record of zodbshootout costs about 60 µs, and a cold read of it through ZEO takes about 140 µs in all. With zstd, `zoe serve` through the native client ties with ZEO on `read_cold` at 100 KB and concurrency 1, and stored raw it is 1.25× faster. FileStorage and ZEO store their records raw.

## Decision

### 1. The frame

Every record, transaction row, blob record, intent file, sequence row and KV namespace row is a frame: a 32-byte little-endian header, the typed-field slots, then the payload. The header holds the frame version, the flags, the codec, a reserved byte, the branch id as a `u32`, the previous serial, the payload length, the length of the slots, two reserved bytes and an xxh3-64 checksum. The branch id is in the header, not in the key: keys stay `(oid, tid)`, big-endian, so their byte order is their numeric order.

### 2. Format room in every record

Every record carries a codec byte, a branch id and a typed-field-slot header, and every blob record its BLAKE3 hash (ADR 0023), from the first store written. A field stays unused until its feature ships.

### 3. The checksum covers the key space

Each kind of frame has a one-byte tag. The tag is not stored. It is the first byte of the checksum input.

| Tag | Key space | Key |
| --- | --- | --- |
| `0x00` | none: 0 is never a tag | |
| `0x01` | `records` | `(oid, tid)`, 16 bytes |
| `0x02` | `blobs`, the blob records | `(oid, tid)`, 16 bytes |
| `0x03` | `txns`, the transaction rows | tid, 8 bytes |
| `0x04` | the intent files in `blobs/tmp/` | tid, 8 bytes |
| `0x05` | `sequences`, the sequence rows | the UTF-8 name of the sequence |
| `0x06` | reserved for `vectors` | |
| `0x07` | `kv_ephemeral` (ADR 0043) | the length of the name, the name, the key |
| `0x08` | `kv_durable` (ADR 0043) | the length of the name, the name, the key |
| `0x09` to `0xff` | free | |

**The checksum input.** The checksum is xxh3-64 with seed 0 over these bytes, in this order:

1. the tag of the key space, 1 byte;
2. the key, exactly as the backend stores it: 16 bytes for `records` and `blobs`, 8 bytes for `txns` and the intent files, the UTF-8 name for `sequences`;
3. header bytes 0 to 23;
4. the typed-field slots;
5. the payload, as stored.

The tag adds no byte on disk.

**The reader gives the tag.** A reader passes the tag of the table or of the kind of file that it reads, never a byte from the stored data. So a frame that another key space wrote fails the checksum, and the read raises the error of the key space that it reads: `CorruptRecord` in `records`, with the oid and the tid; `CorruptBlob` in `blobs`; `CorruptTxn` in `txns`; `CorruptSequence` in `sequences`. An intent file that fails its checksum names nothing, as an intent file that was never synced does, and the next writer open removes it. Because the tag is the first byte and has a fixed length, the inputs of two key spaces always differ in their first byte. A frame can pass in another key space only through a 64-bit hash collision.

**The rules of the tags.**

- A tag never changes.
- A tag is never given to another key space, also after its key space is removed.
- 0 is never a tag, so a zeroed tag never verifies.
- A new kind of frame takes a tag in the change that adds it: a reserved tag if it has one, otherwise the lowest free tag. The code adds one variant to `KeySpace` in `crates/zoe-core/src/format/mod.rs`.
- A key space whose values are not frames, such as `meta`, has no tag.

### 4. A store without the tag in its checksums

The tag is part of the checksum input of `ZOE format 1` and of every later format; ADR 0033 names format 2 for its own reason. A store whose frames hash without the tag opens, and each read of a frame raises a `Corrupt*` error, never data. No code reads that input, and no migration exists: only test and development stores used it, and each is written again from its source.

### 5. The codecs

The codec byte says how the payload holds the data. Codec 0 is the data as it is. Codec 1 is one zstd frame that states the data length in its own frame header. Every other value is reserved for a later codec, such as zstd with a dictionary trained on the store. A build refuses a frame with a codec that it does not know, as a frame of a newer format. The payload length in the header is the stored length, and the checksum covers the payload as stored, so a read verifies the stored bytes before it decompresses them.

### 6. Compression is a setting, and records are raw by default

The default compression of a store is `none`: each new record is written as it is, with codec 0. `Options::default()` of `zoe-core` has `Compression::None`.

The setting `compression` chooses it: `none`, `zstd` (zstd at level 3) or `zstd:LEVEL`. `zoe serve` takes it as `--compression` (`ZOE_COMPRESSION`, the key `compression` of the configuration file and of `<zoe-server>`), and the in-process storage as the key `compression` of `<zoe>` (`ZOE_COMPRESSION`, the query of `zoe://`) and the argument `compression` of `zoe.Storage`. One parser, `FromStr` for `Compression` in `zoe-core`, reads the text for all of them.

With zstd, a store keeps the zstd payload of a record only when it is smaller than the data. Otherwise it writes the data as it is, with codec 0. Transaction rows and the other frames use codec 0. A store reads every codec, whatever its own setting, so a store can change its setting between opens, and a store written with zstd stays readable.

### 7. Decompression is bounded

The zstd payload must be exactly one standard zstd frame, with the standard magic number, and must state the data length. That length must be at least 1, because empty data marks a deletion and the writer never compresses it. It must be at most `u32::MAX`, the limit of the payload length field, and at most 32,768 times the stored length. A zstd writer puts at most 128 KiB in a block of at least 4 bytes, so its frames stay inside this ratio; libzstd accepts larger blocks on read, so the reader enforces the ratio itself. The reader then decodes into a buffer of that length, and the payload must decode to exactly that length. A payload that breaks one of these rules is a `CorruptRecord`, not an allocation. The reader asks for that buffer with a fallible allocation, so a failed allocation is also a `CorruptRecord`, not an abort of the process.

## Consequences

- Every read verifies the checksum, so a flipped bit in a header field or in the payload is an error, never data. Corruption is detected on read and by scrub.
- A frame stored under the wrong key, or read through a damaged key, fails the checksum, so a misplaced or damaged key is detected.
- A frame that a damaged branch page of redb finds in another key space, under the same key, fails its checksum. The two flips of the harness that gave a blob record as a pickle give `CorruptRecord`.
- An exhaustive sweep of the harness database file, every single-bit flip of each non-zero byte outside the stored frames, gives no wrong read: 0 in 102,936 flips on the database of the finding, and 0 in 116,928 flips on the database with `txn_oids` and `sequences`. The harness builds its database in memory at each run, so no fixture file needs to be generated again.
- A flip in the pages of redb stays outside the phase 1 gate. It can still hide data, give a stale read, make the open fail, panic, abort the process or hang a read.
- Each later kind of frame must take a tag. `vectors` takes `0x06`.
- Forks can mark their records without a new key layout.
- A feature that uses the format room needs no migration of a store.
- Each record costs 32 bytes on top of its pickle, and pays a few bytes for features that may come later.
- The format is ZOE's own; tools read it through the core, not as raw pickles.
- The data length of a compressed record comes from the zstd frame header, so the frame needs no data-length field.
- A new store is as large as its records. N14's target, a Plone store at least 3× smaller than `Data.fs`, holds with `compression zstd` only.
- A record that zstd does not make smaller costs nothing more than with compression off, and a read of it does not decompress.
- The compression setting can change between two opens of a store. Records of both codecs then sit side by side and read the same.
- The tests of zstd behaviour (the bit flips of zstd payloads, the attached payloads, the compression pool of `zoe serve`) open their stores with zstd.
- The phase 3 gate runs `zoe serve` with its defaults.

## Alternatives

- **A prefix of the key space in each key**, a prefix byte on every key, or one table for all key spaces with that prefix. The checksum covers the key, so it would cover the key space too. But a prefix stores one more byte in every key of every leaf and branch page, where the tag stores nothing. It changes the keys of the data model, and every range scan and every split of a key, where the tag changes only the checksum input. A key prefix does not reach the intent files, which are files, not rows. With one table, a scan of one key space walks the pages of the others.
- **A new format name for the tag in the checksum.** The specification bumps the on-disk format name only with a migration tool, no store needs one, and a bump spends a version number on stores that do not exist.
- **zstd by default, `none` for the gate only.** The default then loses the cold reads of large records against ZEO, the reads that the gate measures.
- **zstd for small records and raw for large ones.** It is a policy that the specification does not name, and a store would then mix codecs by size.

## Merged ADRs

- ADR 0009, checksums and zstd in the on-disk format: Decision, sections 1, 3, 5 and 6. The dictionary trained on the store is a reserved codec (section 5).
- ADR 0016, format room is reserved per record from the first store: Decision, section 2.
- ADR 0029, the frame checksum covers the key space: Decision, sections 3 and 4; the key prefix and the format name are in Alternatives.
- ADR 0041, records are raw by default; zstd is a setting: Decision, section 6; its alternatives are in Alternatives.
