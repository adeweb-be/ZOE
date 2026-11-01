# 0020. The record frame: the header, the checksum with its key space, and the codecs

Status: accepted.

## Context

N13 asks for a checksum on every record, N14 for compression, and N19 for format room in every record from the first store written. A field added to every record later is a format migration on every store.

A checksum over the key does not prove that a frame sits in its own key space: records and blob records share the `(oid, tid)` key. In the bit-flip harness (`crates/zoe-core/tests/bit_flip_harness.rs`), one flip in a branch page of `records` leads `load` to a blob record, which such a checksum passes as a pickle. redb checks its page checksums only in `check_integrity`, never on a read.

zstd costs about 60 µs to decompress a 100 KB record, of about 140 µs for a cold read through ZEO. With zstd, `zoe serve` ties with ZEO on `read_cold` at 100 KB and concurrency 1; stored raw, it is 1.25× faster. FileStorage and ZEO store their records raw.

## Decision

### 1. The frame

Every record, transaction row, blob record, intent file, sequence row and KV namespace row is a frame: a 32-byte little-endian header, the typed-field slots, then the payload. The fields of the header: specification, "Storage core", "Record framing: checksums, compression, encryption". The branch id is in the header, not in the key, so keys stay `(oid, tid)` big-endian and sort in numeric order.

### 2. Format room in every record

Every record carries a codec byte, a branch id and a typed-field-slot header, and every blob record its BLAKE3 hash (ADR 0023), from the first store written. A field stays unused until its feature ships.

### 3. The checksum covers the key space

The checksum is xxh3-64 with seed 0 over the one-byte tag of the key space, the key as stored, header bytes 0 to 23, the slots and the payload as stored. The tag is not stored.

| Tag | Key space | Key |
| --- | --- | --- |
| `0x00` | none: 0 is never a tag | |
| `0x01` | `records` | `(oid, tid)`, 16 bytes |
| `0x02` | `blobs`, the blob records | `(oid, tid)`, 16 bytes |
| `0x03` | `txns`, the transaction rows | tid, 8 bytes |
| `0x04` | the intent files in `blobs/tmp/` | tid, 8 bytes |
| `0x05` | `sequences`, the sequence rows | the UTF-8 name of the sequence |
| `0x06` | reserved for `vectors` | |
| `0x07` | `kv_ephemeral` (ADR 0043) | name length, name, key |
| `0x08` | `kv_durable` (ADR 0043) | name length, name, key |
| `0x09` to `0xff` | free | |

A reader gives the tag of the table or file that it reads, never a stored byte. So a frame from another key space fails the checksum with the error of the key space read: `CorruptRecord`, `CorruptBlob`, `CorruptTxn` or `CorruptSequence`. An intent file that fails it names nothing.

The rules of the tags:

- A tag never changes and is never reused, also after its key space is removed.
- 0 is never a tag, so a zeroed tag never verifies.
- A new kind of frame takes its reserved tag, else the lowest free tag, in the change that adds it, with one new variant of `KeySpace` (`crates/zoe-core/src/format/mod.rs`).
- A key space whose values are not frames, such as `meta`, has no tag.

### 4. A store without the tag in its checksums

The tag is in the checksum input of `ZOE format 1` and of every later format. A store whose frames hash without the tag opens, and each read of a frame raises a `Corrupt*` error, never data. No migration exists.

### 5. The codecs

Codec 0 is the data as it is. Codec 1 is one zstd frame that states the data length. Every other value is reserved for a later codec, such as zstd with a dictionary trained on the store. A build refuses a frame with an unknown codec, any flag that it does not know, or a non-zero reserved byte, as a frame of a newer format: a `Format` error (`FrameError::Unsupported`). The checksum covers the stored bytes, so a read verifies them before it decompresses.

### 6. Compression is a setting, and records are raw by default

The setting `compression` takes `none`, `zstd` (level 3) or `zstd:LEVEL`. The default is `none` (`Compression::None` in `Options::default()`). It is `--compression` of `zoe serve`, `ZOE_COMPRESSION`, the key `compression` of the configuration file, `<zoe-server>`, `<zoe>` and `zoe://`, and the argument `compression` of `zoe.Storage`. One parser, `FromStr` for `Compression` in `zoe-core`, reads them all.

With zstd, a record keeps its zstd payload only when it is smaller, else it gets codec 0. The other frames use codec 0. A store reads every codec, so the setting can change between opens.

### 7. Decompression is bounded

The rules of a valid zstd payload: specification, "Storage core", "Record framing: checksums, compression, encryption". The reader allocates the stated length fallibly. A payload that breaks a rule, or a failed allocation, gives `CorruptRecord`, never an abort.

## Consequences

- Every read verifies the checksum: a flipped bit in a header, payload or key is an error, never data.
- The harness flips that gave a blob record as a pickle give `CorruptRecord`. Every single-bit flip of each non-zero byte outside the stored frames gives no wrong read: 0 in 102,936 and in 116,928 flips.
- A flip in the pages of redb stays outside the phase 1 gate: it can still hide data, give a stale read, fail the open, panic, abort or hang.
- A feature that uses the format room needs no migration. Each record costs 32 bytes on top of its pickle.
- A new store is as large as its records. N14's target, a Plone store at least 3× smaller than `Data.fs`, holds only with `compression zstd`.
- The tests of zstd behaviour open their stores with zstd.
- The write-ahead log carries the frames as the tables store them, byte for byte, and the fold copies them into the tables as they are (ADR 0056). A frame of the log passes its checksum under its key space and its key before a read or the fold uses it.

## Alternatives

- **A key-space prefix in each key, or one table for all key spaces.** One more byte in every key, a change to every range scan, no reach to the intent files; one table makes a scan walk the others.
- **A new format name for the tag in the checksum.** The specification bumps it only with a migration tool, and no store needs one.
- **zstd by default, `none` for the gate only.** The default then loses the cold reads of large records against ZEO.
- **zstd for small records and raw for large ones.** The specification names no such policy, and a store would mix codecs.

## Merged ADRs

- ADR 0009, checksums and zstd: Decision, sections 1, 3, 5 and 6; the trained dictionary is a reserved codec (section 5).
- ADR 0016, format room per record: Decision, section 2.
- ADR 0029, the key space in the checksum: Decision, sections 3 and 4; the key prefix and the format name are in Alternatives.
- ADR 0041, records raw by default: Decision, section 6; its alternatives are in Alternatives.
