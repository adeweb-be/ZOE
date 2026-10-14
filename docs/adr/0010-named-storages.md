# 0010. Named storages instead of multi-tenancy

Status: accepted

## Context

Plone sites use mount points, which need several databases. ZEO serves several storages from one process. Real tenant isolation inside one server is a large feature with security weight.

## Decision

One `zoe serve` process serves any number of named storages, selected by `register(storage_id)`. Each has its own path, blob store, lock manager, read-only flag, quotas and metric labels. Isolation between sites is by process and by named storage; there is no multi-tenancy inside one storage.

## Consequences

- Mount points work unchanged in both shapes.
- Commits on different storages never wait for each other.
- Per-storage quotas are the only shared-server feature.
- `zoe serve` takes the stores from `--path` (the storage name `1`), from `--storage NAME=PATH`, and from the `[storages.NAME]` tables of its TOML configuration file, which also give the blob directory of each store. A storage name that comes twice is a usage error.
- Each store has its own lock manager and its own invalidations: a client registered with one store never gets the invalidations of another, and a commit on one store never waits for a lock of another.
