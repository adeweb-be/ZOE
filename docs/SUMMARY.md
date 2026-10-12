# Summary

[Introduction](README.md)

# Specification

- [Technical specification](spec/technical-specification.md)
- [Working methods and roadmap](spec/working-methods.md)

# Decisions

- [Architecture decision records](adr/README.md)
  - [0001. A synchronous Rust core](adr/0001-synchronous-rust-core.md)
  - [0002. redb is the first backend](adr/0002-redb-first-backend.md)
  - [0003. The server speaks only the ZEO msgpack protocol](adr/0003-msgpack-only-zeo-server.md)
  - [0004. No Python on the server; three conflict tiers](adr/0004-no-python-on-the-server.md)
  - [0005. ZODB 6 and ZEO 6 only](adr/0005-zodb-6-and-zeo-6-only.md)
  - [0006. CPython 3.12 to 3.14 and 3.14t, on Linux x86-64](adr/0006-python-and-platform-targets.md)
  - [0007. uv for the project's packages, buildout for Plone instances](adr/0007-uv-for-packages-buildout-for-plone.md)
  - [0008. Dagger is the CI](adr/0008-dagger-as-the-ci.md)
  - [0009. Checksums and zstd in the on-disk format](adr/0009-checksums-and-zstd-in-the-format.md)
  - [0010. Named storages instead of multi-tenancy](adr/0010-named-storages.md)
  - [0011. Apache-2.0 for everything](adr/0011-apache-2-licence.md)
  - [0012. docs/ in git is the source of truth](adr/0012-docs-in-git-published-to-pages.md)
  - [0013. devenv and just](adr/0013-devenv-and-just.md)
  - [0014. Per-OID locks in oid order; the core allocates tids](adr/0014-per-oid-locks-and-core-tids.md)
  - [0015. One on-disk format for both shapes](adr/0015-one-format-for-both-shapes.md)
  - [0016. Format room is reserved per record](adr/0016-format-room-per-record.md)
  - [0017. The distribution is zoedb](adr/0017-names-zoedb-and-zoe.md)
  - [0018. The board and trunk-based development](adr/0018-board-and-trunk.md)
  - [0019. MVCC in-process through ZODB's MVCCAdapter](adr/0019-mvcc-through-the-adapter.md)
  - [0020. The record frame](adr/0020-record-frame.md)
  - [0021. Interactive sessions instead of automated loops](adr/0021-interactive-sessions.md)
  - [0022. Red zones: the on-disk format, pack and the process files](adr/0022-narrow-red-zones.md)
  - [0023. The blob record](adr/0023-blob-record.md)
  - [0024. Sequences: one row per name, reserved in durable blocks of 100](adr/0024-sequences.md)
  - [0025. Pack: what it removes, batches, and the progress record](adr/0025-pack.md)
  - [0026. The transaction index: a (tid, oid) key space](adr/0026-transaction-index.md)
  - [0027. The blob directory](adr/0027-blob-directory.md)
  - [0028. The zoe command](adr/0028-the-zoe-command.md)
  - [0029. The frame checksum covers the key space](adr/0029-key-space-in-the-checksum.md)

# Reference

- [Glossary](glossary.md)
- [Spikes](spikes/README.md)
  - [Free-threading in persistent, BTrees and zodbpickle](spikes/free-threading-upstream.md)
  - [Byte-identical pickle round trip in zoe-pickle](spikes/pickle-round-trip.md)
  - [redb versus an append-only log for the record store](spikes/record-store.md)
  - [The ZEO 6 wire contract, recorded from a stock server](spikes/zeo-wire.md)
  - [A deterministic simulator for the server](spikes/deterministic-simulation.md)
- [Finding: persistent loses the state of an object on CPython 3.14t](findings/persistent-inline-values.md)
- [Finding: zodbpickle turns the GIL on at `import ZODB` on CPython 3.14t](findings/zodbpickle-free-threading.md)
- [Finding: BTrees turns the GIL on in a ZODB application on CPython 3.14t](findings/btrees-free-threading.md)
