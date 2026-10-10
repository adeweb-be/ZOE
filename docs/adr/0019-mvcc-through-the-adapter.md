# 0019. MVCC in-process through ZODB's MVCCAdapter

Status: accepted

## Context

ZODB wraps any storage that does not implement `IMVCCStorage` in `MVCCAdapter`, which builds per-connection snapshots from `loadBefore`. Implementing `IMVCCStorage` directly is a second MVCC implementation.

## Decision

The in-process storage implements `loadBefore` and lets ZODB's own `MVCCAdapter` provide snapshots, as `FileStorage` does. `IMVCCStorage` is a later optimisation, only if profiling shows the adapter in the way.

## Consequences

- One MVCC implementation, the one ZODB already tests.
- `loadBefore` latency is on the hot path of every cache miss.
