# Architecture decision records

One record per decision taken in the specification, numbered. Each record gives the context, the decision and its consequences. A new decision goes through a card of its own and adds the next number.

| Number | Decision |
| --- | --- |
| [0001](0001-synchronous-rust-core.md) | A synchronous Rust core |
| [0002](0002-redb-first-backend.md) | redb is the first backend |
| [0003](0003-msgpack-only-zeo-server.md) | The server speaks only the ZEO msgpack protocol |
| [0004](0004-no-python-on-the-server.md) | No Python on the server; three conflict tiers |
| [0005](0005-zodb-6-and-zeo-6-only.md) | ZODB 6 and ZEO 6 only |
| [0006](0006-python-and-platform-targets.md) | CPython 3.12 to 3.14 and 3.14t, on Linux x86-64 |
| [0007](0007-uv-for-packages-buildout-for-plone.md) | uv for the project's packages, buildout for Plone instances |
| [0008](0008-dagger-as-the-ci.md) | Dagger is the CI |
| [0009](0009-checksums-and-zstd-in-the-format.md) | Checksums and zstd in the on-disk format |
| [0010](0010-named-storages.md) | Named storages instead of multi-tenancy |
| [0011](0011-apache-2-licence.md) | Apache-2.0 for everything |
| [0012](0012-docs-in-git-published-to-pages.md) | docs/ in git is the source of truth, published to GitHub Pages |
| [0013](0013-devenv-and-just.md) | devenv is the only environment, just is the only entry point |
| [0014](0014-per-oid-locks-and-core-tids.md) | Per-OID locks in oid order; the core allocates tids |
| [0015](0015-one-format-for-both-shapes.md) | One on-disk format for both shapes |
| [0016](0016-format-room-per-record.md) | Format room is reserved per record from the first store |
| [0017](0017-names-zoedb-and-zoe.md) | The distribution is zoedb; every identifier uses the stem zoe |
| [0018](0018-board-and-trunk.md) | GitHub Issues and Projects are the board; trunk-based development |
| [0019](0019-mvcc-through-the-adapter.md) | MVCC in-process through ZODB's MVCCAdapter |
| [0020](0020-record-frame.md) | The record frame |
| [0021](0021-interactive-sessions.md) | Interactive sessions instead of automated loops |
| [0022](0022-narrow-red-zones.md) | Red zones: the on-disk format, pack and the process files |
| [0023](0023-blob-record.md) | The blob record |
| [0024](0024-sequences.md) | Sequences: one row per name, reserved in durable blocks of 100 |
| [0025](0025-pack.md) | Pack: what it removes, batches, and the progress record |
| [0026](0026-transaction-index.md) | The transaction index: a `(tid, oid)` key space |
| [0027](0027-blob-directory.md) | The blob directory is an option of the core |
| [0028](0028-the-zoe-command.md) | The zoe command: one binary in zoe-server, one code path for the console script |
| [0029](0029-key-space-in-the-checksum.md) | The frame checksum covers the key space |
| [0030](0030-dump-files.md) | Dump files: a format of ZOE's own, one file per tid range |
| [0031](0031-the-in-process-storage-closes-its-stores-on-a-signal.md) | The in-process storage closes its stores on SIGTERM and SIGHUP |
| [0032](0032-a-tool-that-reads-a-store-repairs-it.md) | A tool that reads a store repairs it when a writer left it open; it supersedes the opens of ADR 0028 |
| [0033](0033-object-count.md) | The store counts its objects: `object_count` in `meta`, ZOE format 2 |
| [0034](0034-the-client-cache-file.md) | The client cache file (accepted) |
| [0035](0035-the-extension-links-zoe-client-and-tokio.md) | The extension links zoe-client and tokio for the native client; it supersedes the rule of ADR 0001 and ADR 0028 that the extension never links tokio |
| [0036](0036-concurrent-transactions-in-a-client.md) | Several transactions at once in one client process (accepted) |
| [0037](0037-the-phase-3-gate-counts-the-cost-of-the-storage.md) | The phase 3 gate counts the cost of the storage against a floor (superseded by ADR 0039) |
| [0038](0038-macos-for-development.md) | macOS is a target (accepted) |
| [0039](0039-the-phase-3-gate-is-a-quick-head-to-head.md) | The phase 3 gate is a quick head-to-head: faster than ZEO in every cell; it supersedes ADR 0037 (accepted) |
| [0040](0040-a-sync-on-macos-is-fsync.md) | A sync on macOS is fsync(2), as for FileStorage (accepted) |
| [0041](0041-records-are-raw-by-default.md) | Records are raw by default; zstd is a setting (accepted) |
| [0042](0042-the-posting-dict-rule.md) | The posting-dict rule, the first-key rule, and the settings of the structural merges (accepted) |
| [0043](0043-kv-namespaces.md) | KV namespaces: an ephemeral namespace with a TTL, and a durable namespace (accepted) |
| [0044](0044-the-lock-hand-off.md) | The lock hand-off: a merge at the vote takes the lock of a commit that is in a commit batch (accepted) |
| [0045](0045-lock-intent.md) | Early write-intent locks: the lock before the read of a declared hot object (accepted) |
| [0046](0046-the-merge-policy.md) | The merge policy: a persistent object under the root, read at the vote, and the field merges of tier 2 (accepted) |
| [0047](0047-queued-indexing.md) | Queued indexing: a save writes its catalog operations to a durable namespace, and one indexer applies them (proposed) |
