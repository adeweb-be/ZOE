# 0016. Format room is reserved per record from the first store

Status: accepted

## Context

Several planned features need per-record data: codecs for compression, forks, typed fields, blob deduplication. Adding a field to every record later is a format migration on every store.

## Decision

Every record carries a codec byte, a branch id and a typed-field-slot header, and every blob record its BLAKE3 hash, from the first store written. The fields stay unused until their feature ships.

## Consequences

- A phase 1 store opens unchanged under every later build.
- Every record pays a few bytes for features that may come later.
