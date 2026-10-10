# 0014. Per-OID locks in oid order; the core allocates tids

Status: accepted

## Context

`FileStorage` takes one storage-wide commit lock from `tpc_begin` to `tpc_finish`, so every commit waits for every other. Deadlocks and tid ordering must stay impossible to get wrong.

## Decision

At vote, the lock manager takes per-OID locks for the written oids (exclusive) and the read-current oids (shared), always in ascending oid order; a request out of order is refused. A lock wait past the timeout becomes a `ConflictError`. The core allocates the tid at finish under the commit sequencer; clients pass a tid only through `restore`.

## Consequences

- Transactions on disjoint oids commit in parallel.
- Two transactions cannot deadlock on each other.
- The backend never allocates tids.
