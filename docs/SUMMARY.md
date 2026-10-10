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

# Reference

- [Glossary](glossary.md)
- [Spikes](spikes/README.md)
  - [Free-threading in persistent, BTrees and zodbpickle](spikes/free-threading-upstream.md)
