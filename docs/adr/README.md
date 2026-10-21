# Architecture decision records

One record per decision taken in the specification, numbered. Each record gives the context, the decision and its consequences. A new decision goes through a card of its own and adds the next number.

The records merge by theme. In a theme, one record holds the current decision, and each other record of the theme is a short stub that points to it: its status says `merged into ADR MMMM` and names the section. A number never changes, and the file of a number stays, so every citation of a number still finds its subject. The numbers 0051 and 0053 belong to the card ZOE-212, which is parked: their records are on the branch `card/ZOE-212`.

| Number | Decision | Status | Theme |
| --- | --- | --- | --- |
| [0001](0001-synchronous-rust-core.md) | A synchronous Rust core, and where async lives | accepted | Async boundary |
| [0002](0002-redb-first-backend.md) | redb is the first backend | accepted | |
| [0003](0003-msgpack-only-zeo-server.md) | The server speaks only the ZEO msgpack protocol | accepted | |
| [0004](0004-no-python-on-the-server.md) | No Python on the server; three conflict tiers | accepted | |
| [0005](0005-zodb-6-and-zeo-6-only.md) | ZODB 6 and ZEO 6 only | merged into 0006 | Platforms |
| [0006](0006-python-and-platform-targets.md) | The targets: CPython 3.12 to 3.14 and 3.14t, ZODB 6 and ZEO 6, on Linux x86-64 and macOS | accepted | Platforms |
| [0007](0007-uv-for-packages-buildout-for-plone.md) | uv for the project's packages, buildout for Plone instances | merged into 0013 | Tooling |
| [0008](0008-dagger-as-the-ci.md) | Dagger is the CI | merged into 0013 | Tooling |
| [0009](0009-checksums-and-zstd-in-the-format.md) | Checksums and zstd in the on-disk format | merged into 0020 | Record format |
| [0010](0010-named-storages.md) | Named storages instead of multi-tenancy | accepted | |
| [0011](0011-apache-2-licence.md) | Apache-2.0 for everything | merged into 0018 | Process |
| [0012](0012-docs-in-git-published-to-pages.md) | docs/ in git is the source of truth, published to GitHub Pages | merged into 0013 | Tooling |
| [0013](0013-devenv-and-just.md) | The tools: devenv and just, uv and buildout, Dagger for CI, and docs/ on GitHub Pages | accepted | Tooling |
| [0014](0014-per-oid-locks-and-core-tids.md) | Per-OID locks in oid order; the core allocates tids | accepted | |
| [0015](0015-one-format-for-both-shapes.md) | One on-disk format for both shapes | accepted | |
| [0016](0016-format-room-per-record.md) | Format room is reserved per record from the first store | merged into 0020 | Record format |
| [0017](0017-names-zoedb-and-zoe.md) | The distribution is zoedb; every identifier uses the stem zoe | accepted | |
| [0018](0018-board-and-trunk.md) | The process: the board, trunk-based development, interactive sessions, narrow red zones and the licence | accepted | Process |
| [0019](0019-mvcc-through-the-adapter.md) | MVCC in-process through ZODB's MVCCAdapter | accepted | |
| [0020](0020-record-frame.md) | The record frame: the header, the checksum with its key space, and the codecs | accepted | Record format |
| [0021](0021-interactive-sessions.md) | Interactive sessions instead of automated loops | merged into 0018 | Process |
| [0022](0022-narrow-red-zones.md) | Red zones: the on-disk format, pack and the process files | merged into 0018 | Process |
| [0023](0023-blob-record.md) | The blob record and the blob directory | accepted | Blobs |
| [0024](0024-sequences.md) | Sequences: one row per name, reserved in durable blocks of 100 | accepted | |
| [0025](0025-pack.md) | Pack: what it removes, batches, and the progress record | accepted; one point waits for the confirmation of the human | |
| [0026](0026-transaction-index.md) | The transaction index: a `(tid, oid)` key space | accepted | |
| [0027](0027-blob-directory.md) | The blob directory is an option of the core | merged into 0023 | Blobs |
| [0028](0028-the-zoe-command.md) | The zoe command: one binary in zoe-server, one code path for the console script, and tools that repair what they read | accepted | The zoe command |
| [0029](0029-key-space-in-the-checksum.md) | The frame checksum covers the key space | merged into 0020 | Record format |
| [0030](0030-dump-files.md) | Dump files: a format of ZOE's own, one file per tid range | accepted | |
| [0031](0031-the-in-process-storage-closes-its-stores-on-a-signal.md) | The in-process storage closes its stores on a signal | accepted | |
| [0032](0032-a-tool-that-reads-a-store-repairs-it.md) | A tool that reads a store repairs it when a writer left it open | merged into 0028 | The zoe command |
| [0033](0033-object-count.md) | The store counts its objects: `object_count` in `meta`, ZOE format 2 | accepted | |
| [0034](0034-the-client-cache-file.md) | The client cache file | accepted | |
| [0035](0035-the-extension-links-zoe-client-and-tokio.md) | The extension links zoe-client and tokio for the native client | merged into 0001 | Async boundary |
| [0036](0036-concurrent-transactions-in-a-client.md) | Several transactions at once in one client process | accepted | |
| [0037](0037-the-phase-3-gate-counts-the-cost-of-the-storage.md) | The phase 3 gate counts the cost of the storage against a floor | merged into 0039, which replaces its decision | Phase 3 gate |
| [0038](0038-macos-for-development.md) | macOS is a target | merged into 0006 | Platforms |
| [0039](0039-the-phase-3-gate-is-a-quick-head-to-head.md) | The phase 3 gate is a quick head-to-head: faster than ZEO in every cell | accepted | Phase 3 gate |
| [0040](0040-a-sync-on-macos-is-fsync.md) | A sync on macOS is fsync(2), as for FileStorage | merged into 0006 | Platforms |
| [0041](0041-records-are-raw-by-default.md) | Records are raw by default; zstd is a setting | merged into 0020 | Record format |
| [0042](0042-the-posting-dict-rule.md) | The posting-dict rule, the first-key rule, and the settings of the structural merges | accepted | |
| [0043](0043-kv-namespaces.md) | KV namespaces: an ephemeral namespace with a TTL, a durable namespace, and the backends of zoe.plone on them | accepted | KV namespaces |
| [0044](0044-the-lock-hand-off.md) | The locks of hot objects: the lock hand-off at the vote, and early write-intent locks before the read | accepted | Locks of hot objects |
| [0045](0045-lock-intent.md) | Early write-intent locks: the lock before the read of a declared hot object | merged into 0044 | Locks of hot objects |
| [0046](0046-the-merge-policy.md) | The merge policy: a persistent object under the root, read at the vote, and the field merges of tier 2 | accepted | |
| [0047](0047-queued-indexing.md) | Queued indexing: a save writes its catalog operations to a durable namespace, and one indexer applies them | accepted | |
| [0048](0048-the-split-merge.md) | The split merge: tier 1 merges the splits of a BTree, and rule 3 keeps the bucket of the committed side | accepted | Split merge |
| [0049](0049-the-split-merge-keeps-the-bucket.md) | The split merge keeps the bucket of the committed side | merged into 0048 | Split merge |
| [0050](0050-the-registry-backend.md) | The registry backend: a moved registry keeps its records in a durable namespace of its own | merged into 0043 | KV namespaces |
| [0052](0052-the-order-of-a-folder-merges-at-the-vote.md) | The order of a folder of plone.folder merges at the vote | accepted | |
| [0054](0054-one-published-format-for-every-benchmark.md) | One published format for every benchmark: one layout, the index of version 2, the B5 summary and `just bench publish` | accepted | |
| [0055](0055-zoe-wire.md) | zoe-wire: the ZEO frame codec and the TLS certificate reader, written once in a sans-IO crate | accepted | |
