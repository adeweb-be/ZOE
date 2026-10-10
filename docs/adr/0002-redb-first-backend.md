# 0002. redb is the first backend

Status: accepted

## Context

The core stores records under `(oid, tid)` keys and needs ACID write transactions, MVCC reads and an fsync on commit. Candidates were redb (embedded, pure Rust), an append-only log with an in-memory index, and a distributed key-value store.

## Decision

redb is the first backend, in both shapes. The `Backend` trait keeps the others possible: the append-only log when benchmarks show that redb's write path is the bottleneck, FoundationDB or TiKV for deployment tier 3.

## Consequences

- The first release builds on a store that is ACID by construction and simplest to get right.
- redb's copy-on-write B-tree has write amplification; the phase 2 numbers decide whether the log backend is built.
- The backend trait stays small, because the core, not the backend, implements locking, two-phase commit and resolution.
