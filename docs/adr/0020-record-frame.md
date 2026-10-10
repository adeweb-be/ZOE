# 0020. The record frame

Status: accepted

## Context

N13 asks for a checksum on every record, N14 for compression and encryption, and N19 for a codec byte, a branch id and typed-field slots on every record from the first store written. The first store needs one layout that leaves room for all of them.

## Decision

Every record, transaction row and blob chunk is a frame: a 32-byte little-endian header, the typed-field slots, then the payload. The header holds the frame version, the flags, the codec, a reserved byte, the branch id as a `u32`, the previous serial, the payload length, the length of the slots, two reserved bytes and an xxh3-64 checksum. The checksum hashes, with seed 0, the key that the frame is stored under, then the first 24 bytes of the header, the slots and the payload, in that order. The key is the 16-byte `(oid, tid)` key of a record or the 8-byte tid key of a transaction row, exactly as the backend stores it. The branch id is in the header, not in the key: keys stay `(oid, tid)`, big-endian, so their byte order is their numeric order.

## Consequences

- Every read verifies the checksum, so a flipped bit in a header field or in the payload is an error, never data.
- A frame stored under the wrong key, or read through a damaged key, fails the checksum, so a misplaced or damaged key is detected.
- Forks can mark their records without a new key layout.
- Each record costs 32 bytes on top of its pickle.
