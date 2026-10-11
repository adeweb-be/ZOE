# 0020. The record frame

Status: accepted

## Context

N13 asks for a checksum on every record, N14 for compression and encryption, and N19 for a codec byte, a branch id and typed-field slots on every record from the first store written. The first store needs one layout that leaves room for all of them.

## Decision

Every record, transaction row and blob chunk is a frame: a 32-byte little-endian header, the typed-field slots, then the payload. The header holds the frame version, the flags, the codec, a reserved byte, the branch id as a `u32`, the previous serial, the payload length, the length of the slots, two reserved bytes and an xxh3-64 checksum. The checksum hashes, with seed 0, the key that the frame is stored under, then the first 24 bytes of the header, the slots and the payload, in that order. The key is the 16-byte `(oid, tid)` key of a record or the 8-byte tid key of a transaction row, exactly as the backend stores it. The branch id is in the header, not in the key: keys stay `(oid, tid)`, big-endian, so their byte order is their numeric order.

The codec byte says how the payload holds the data. Codec 0 is the data as it is. Codec 1 is one zstd frame that states the data length in its own frame header. Every other value is reserved for a later codec, such as zstd with a dictionary trained on the store. A build refuses a frame with a codec that it does not know, as a frame of a newer format. The payload length in the header is the stored length, and the checksum covers the payload as stored, so a read verifies the stored bytes before it decompresses them.

A store writes each record with zstd at a configured level, 3 by default, and keeps the zstd payload only when it is smaller than the data. Otherwise it writes the data as it is, with codec 0. A store with compression off writes codec 0. Transaction rows and blob chunks use codec 0. A store reads every codec, whatever its own setting.

Decompression is bounded. The zstd payload must be exactly one standard zstd frame, with the standard magic number, and must state the data length. That length must be at least 1, because empty data marks a deletion and the writer never compresses it. It must be at most `u32::MAX`, the limit of the payload length field, and at most 32,768 times the stored length, because a zstd block of 4 bytes decodes to at most 128 KiB. The reader then decodes into a buffer of that length, and the payload must decode to exactly that length. A payload that breaks one of these rules is a `CorruptRecord`, not an allocation.

## Consequences

- Every read verifies the checksum, so a flipped bit in a header field or in the payload is an error, never data.
- A frame stored under the wrong key, or read through a damaged key, fails the checksum, so a misplaced or damaged key is detected.
- Forks can mark their records without a new key layout.
- Each record costs 32 bytes on top of its pickle.
- The data length of a compressed record comes from the zstd frame header, so the frame needs no data-length field.
- A record that zstd does not make smaller costs nothing more than with compression off, and a read of it does not decompress.
- The compression setting can change between two opens of a store. Records of both codecs then sit side by side and read the same.
