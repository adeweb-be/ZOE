# ZOE — technical specification

@Antoine Duchêne

This document is the technical specification: what ZOE is and how it works. The way of working, the units of work, the sessions, the roadmap and the project organisation are in ZOE — working methods and roadmap.

## Summary

ZOE (Zope Object Engine) is a ZODB storage written in Rust that Plone can use unchanged: in-process for a single Zope instance, or behind a tokio server that speaks the existing ZEO wire protocol for a cluster. It aims to be the fastest storage backend available for Plone while staying ACID, and it attacks ConflictError at the storage layer with per-object locking and native conflict resolution.

**Goals**

1. Drop-in for Plone 6: the only configuration change is the storage stanza in `zope.conf` (`<zoe>` or a `<zeoclient>` pointed at the Rust server). No change to application code, `persistent`, `BTrees` or `transaction`.
2. Fastest measured Plone storage: beats `FileStorage`, ZEO and RelStorage on Postgres on the same `zodbshootout` harness, cold, warm and hot.
3. Async where it pays: the server is fully asynchronous (tokio), handles pipelined requests per connection, and exposes an async storage interface for future async clients. The ZODB object model stays synchronous.
4. ACID by construction: atomic two-phase commit, durable on `tpc_finish` (fsync or replicated acknowledgement), snapshot isolation through MVCC keyed by `(oid, tid)`, and crash recovery without a repair step.
5. Fewer ConflictErrors under heavy writes: per-OID commit locks, optional early write-intent locks, and conflict resolution in Rust for structures and for fields, with Python only on the client for the long tail.
6. Two deployment shapes from one crate: in-process (no server) and clustered (server), same on-disk format, switchable by restarting.
7. Cloud-native on Kubernetes in three tiers: single-replica StatefulSet, leader/follower with automatic failover, stateless fronts over FoundationDB or TiKV.
8. Free-threading ready: the Python extension declares `Py_mod_gil_not_used` and commits in parallel inside one free-threaded Zope process.
9. Loupe, a Tauri desktop inspector and editor for ops and developers: browse, time-travel, tail live commits, full CRUD without Python, and converters from `Data.fs` and RelStorage, so ZOE replaces `Data.fs` entirely.
10. A faster catalog behind the unchanged ZCatalog API: an in-memory mirror answering queries in Rust as of the client's snapshot, a native BM25 text index on tantivy with real multilingual handling, and later a delta-based projection that ends bucket conflicts.
11. AI-native: an MCP server for ops and content agents with Plone's permissions enforced before ranking, and native retrieval with chunked passages, local-by-default embeddings, hybrid BM25 and vector search, and a choice of vector backend, ZOE's own index stored in the DB or Qdrant.
12. Operations as a feature: a checksum on every record, compression and encryption at rest, repozo-style dumps (continuous backup with point-in-time recovery comes after the roadmap), copy-on-write forks, a public change feed, named storages for mount points, and the data tools (verify, rename-class, export) in Rust.

**Non-goals**

- Rewriting `persistent` and `BTrees` in Rust. That is a separate, client-side project (section 8 says what is left there).
- An asynchronous persistent-object API (`await obj.load()`). It would break every Plone add-on; the design keeps lazy loading synchronous.
- ZODB 5 and ZEO 5. ZOE targets ZODB ≥ 6.0 and ZEO ≥ 6.0, which means Plone 6.1 and later; older clients are not supported.
- Requiring a new client. `ZEO.ClientStorage` stays supported for the life of the project; the native Rust client of F28 ships in phase 8 as a drop-in, never a requirement; the server replaces `runzeo`.
- Multi-tenancy inside one server. Isolation is by process and by named storage (F22); per-storage quotas are the only shared-server feature.
- The sixteen features triaged as Won't in the roadmap's Feature triage section, among them agent branches with merge-back, actor-tagged transactions, read replicas with session tokens, WASM plugins, server-side subtree clone and blocks as CRDT documents. Everything else from that triage has a requirement above (F29 to F38, N18, N19) or waits after the roadmap.

**Success criteria**

- Plone 6 test suites (`Products.CMFPlone`, `plone.app.contenttypes`, `plone.restapi`) pass against both modes with no skips attributed to the storage.
- `ZODB.tests` conformance suites and the ZEO client test suite pass against the server.
- On the reference benchmark machine, `zodbshootout` shows lower latency and higher throughput than ZEO and RelStorage/Postgres in every scenario, by a margin recorded in section 11.
- A crash test (`kill -9` or power cut at any point of `tpc_finish`) never loses an acknowledged transaction and never needs manual repair.
- On a write-heavy replay of production traffic, ConflictErrors raised to Zope drop by at least an order of magnitude against ZEO with the same data.

## Naming

ZOE, the Zope Object Engine, is the project name: an anagram of ZEO, a first name, and an acronym in the ZODB/ZEO/ZRS tradition. The stem for every identifier is `zoe`, written ZOE in prose. The PyPI name `ZOE` is held by an abandoned OpenGL toy and the crates.io name `zoe` by an active CDC genomics crate, so the distribution ships as `zoedb` while a PEP 541 transfer of `ZOE` is requested, and no bare `zoe` crate is published.

**Candidates**

| Name | Why | Outcome |
| --- | --- | --- |
| ZOE — Zope Object Engine | anagram of ZEO; a first name like Jim Fulton's Newt DB; Z-prefixed acronym like ZODB, ZEO, ZRS | chosen |
| ZEAL — Zope Enterprise Append Log | echoes ZEO's expansion; a real word; close to *zèle* in French | runner-up |
| rebost | Catalan for pantry, in the Pastanaga/Barceloneta/Volto tradition | not chosen |
| meuse | the river at Namur; local rather than community | not chosen |
| keel, ferro, rustzodb | generic, Rust-flavoured or dull | not chosen |

**Conventions derived from the stem `zoe`**

| Thing | Name | Rule |
| --- | --- | --- |
| repository | `adeweb-be/ZOE` (the GitHub organisation is `adeweb-be`; the owner is adeweb) | one monorepo: `crates/` for Rust, `python/` for the extension and Python packages, `deploy/` for Helm and manifests |
| Rust crates | `zoe-core`, `zoe-server`, `zoe-client`, `zoe-py`, zoe-catalog, zoe-vectors, zoe-mcp, `zoe-bench` | hyphens in crate names, `snake_case` modules, newtypes `Oid`, `Tid`, errors `ZoeError`; no bare `zoe` crate (taken on crates.io) |
| Python distribution | `zoedb` on PyPI (`ZOE` pending a PEP 541 transfer) | same version number as the crates; the wheel pins the exact core version |
| Python import | `zoe` with `zoe.storage`, `zoe.serve`, `zoe.zeo`, `zoe.zodb`, `zoe.asyncio`, `zoe.testing` | no hyphens, PEP 8 inside |
| CLI | `zoe` with subcommands `serve`, `pack`, `backup`, `restore`, `verify`, `scrub`, `convert`, `fork`, `feed`, `rename-class`, `export`, `admin`, `mcp` | one binary, one console script of the same name |
| `zope.conf` stanza | `<zoe>` after `%import zoe`; keys in kebab-case (`lock-timeout`, `early-locks`) | mirrors `<filestorage>` and `<zeoclient>` |
| URI scheme | `zoe://` for `zodburi` | path and query mirror the stanza keys |
| environment | `ZOE_*` in SCREAMING\_SNAKE | every stanza key has an environment twin |
| protocol extensions | `lock_intent`, `client_resolve`, `trace_context` | snake\_case like ZEO method names |
| metrics | `zoe_*` Prometheus names | histograms in seconds, counters with `_total` |
| container image | `ghcr.io/adeweb-be/zoe` | tags equal release versions |
| Kubernetes | `app.kubernetes.io/name: zoe`, role label \`zoe/role: leader | follower\` |
| on-disk format | `ZOE format 1` in the store header | bumps only with a migration tool |
| product name in prose | ZOE, expanded once per document as Zope Object Engine | code identifiers stay lowercase `zoe` |
| desktop app | Loupe; crate zoe-loupe; bundle identifier dev.adeweb.zoe.loupe; crates zoe-pickle and zoe-convert beside it | the only second product name; "Loupe" in prose, lowercase in identifiers |
| Plone integration (later) | zoe.plone, an optional extra of the wheel: zoedb\[plone\]; auto-included in Plone through the z3c.autoinclude entry point | phase 3 for the merge policy derived from Dexterity schemas and the reindex queue, phase 5 for the catalog hook and ZoeTextIndex; the wheel itself never imports Plone, only the extra does |
| tags and versions | v0.3.0 for the Cargo workspace and the zoedb wheel together; loupe-v0.1.0 for Loupe | semantic versioning; the on-disk format version is independent of the release version |
| licences | Apache-2.0 for everything, the Plone extra included | Apache-2.0 is the Rust ecosystem's norm and keeps the door open to the Plone Foundation and to any adopter |
| documentation site | GitHub Pages of the adeweb-be/ZOE repository, built with mdBook from docs/ on every merge to trunk | docs/ in git is the source of truth: this specification, the working-methods document and the ADRs live there and the documenter keeps them current; nothing is hosted on a private domain |

## Context

The ZODB stack is mostly Python; the C lives in three client-side packages, and the upstream projects have started free-threading work. Line counts are from fresh clones made during the analysis (tests excluded).

| Package | Language | Size | Role | Free-threading status |
| --- | --- | --- | --- | --- |
| [ZODB](https://github.com/zopefoundation/ZODB) | Python | 17.6k lines | Connection, DB, FileStorage, MVCC adapter, 2PC | pure Python; thread model is one Connection per thread |
| [ZEO](https://github.com/zopefoundation/ZEO) 6.3.dev0 | Python + 2 optional Cython modules | 10.3k lines | asyncio client and server, msgpack wire protocol | pure Python |
| [persistent](https://pypi.org/project/persistent/) 6.8 | C + Python reference impl | 4.1k lines C | `Persistent` base class, pickle cache, LRU ring | compiles on 3.14t, cp314t wheels, `Per_dealloc` crash fixed in 6.6; does not declare `Py_mod_gil_not_used` |
| [BTrees](https://github.com/zopefoundation/BTrees) | C templates + Python reference impl | 10.7k lines C, 21 instantiations | persistent containers, bucket conflict resolution | no free-threading work; uses persistent's C capsule |
| [zodbpickle](https://github.com/zopefoundation/zodbpickle) | C (fork of CPython `_pickle.c`) | 7.2k lines | `noload`, `binary` shim for py2 data | compiles on 3.14t; does not declare `Py_mod_gil_not_used` |
| [zope.interface](https://pypi.org/project/zope-interface/) 8.6 | C + Python | — | component architecture | 8.3 declares `Py_mod_gil_not_used`, 8.5 ships cp314t/cp315t wheels; cache races found ([#384](https://github.com/zopefoundation/zope.interface/issues/384)) |

What this means for the design:

- A storage server or in-process storage touches none of the C above. It plugs in through `ZODB.interfaces.IStorage`, the same seam `FileStorage` and `RelStorage` use.
- Any extension loaded on 3.14t without `Py_mod_gil_not_used` silently re-enables the GIL. Today `persistent`, `BTrees` and `zodbpickle` all do, so free-threaded parallelism in Zope needs those three declared before ZOE's own declaration matters.
- `persistent` and `BTrees` ship pure-Python reference implementations with full test suites, which makes a later Rust port of them a differential-testing job rather than a rewrite in the dark.

**Python free-threading timeline.** 3.13 (Oct 2024) experimental; 3.14 (Oct 2025) officially supported under [PEP 779](https://peps.python.org/pep-0779/); 3.15 (Oct 2026) adds the free-threaded stable ABI `abi3t` under [PEP 803](https://peps.python.org/pep-0803). Making it the default is left to a future PEP. PyO3 supports free-threaded builds since 0.23 and assumes modules are thread-safe since 0.28 ([guide](https://pyo3.rs/latest/free-threading.html)).

**Targets.** CPython 3.12, 3.13 and 3.14, with 3.14t as the free-threaded build; 3.13t is not a target and 3.15 joins when it ships. Linux x86-64 for the first phases; macOS and Windows later, for the wheels, the server binary and Loupe alike.

**Prior art**

| Project | What it is | What ZOE takes from it |
| --- | --- | --- |
| [byteserver](https://github.com/jimfulton/byteserver) (Jim Fulton, 2016–2021) | 3,043 lines of Rust; synchronous `std::net`, two OS threads per client; implements 10 of ZEO's \~35 server methods; no blobs, pack, undo, `checkCurrentSerialInTransaction` or invalidation replay; data treated as opaque; dependencies frozen in 2016 | the per-OID lock manager at vote time; the append-only segment design; the opaque-data server. Not the code. |
| [filestorage2](https://github.com/jimfulton/filestorage2) (2015–2016) | 11 commits, a format document: aligned segments, `PPPP`→`TTTT` marker flip after fsync, per-segment index, crc32 per record | the crash-safety pattern; candidate for the "fast log" backend |
| RelStorage on PostgreSQL | mature ZODB storage over SQL, history-preserving or history-free, row locks at vote | the `(zoid, tid)` data model; `zodbshootout`, its benchmark harness; the HA story it gets from Postgres operators |
| NEO (Nexedi) | distributed ZODB with replication and master election | proof that a global tid sequencer with replicated storage nodes works; its complexity is the warning |
| ZEO 5 | asyncio client, msgpack protocol, `prefetch`, server-side conflict resolution with Plone eggs installed | the wire protocol and the client, unchanged; the deployment model of "a Python process that hosts the server" |

Jim Fulton said in 2016 that the ZODB API is synchronous and that prefetch is the way to overlap I/O ([talk notes](https://2016.ploneconf.org/maurits.vanrees.org/weblog/archive/2016/10/jim-fulton-the-zodb.html)); by 2019 byteserver had been idle for three years ([thread](https://groups.google.com/g/zodb/c/u_QWRSRR14A)). `Connection.prefetch()` exists in ZODB and `ClientStorage.prefetch()` in ZEO today.

## Requirements

Every requirement has a test that decides it; section 11 names the harness for each. "Must" gates the first release, "should" the second, "may" is on the roadmap.

**Functional**

| # | Requirement | Level | Accepted when |
| --- | --- | --- | --- |
| F1 | Implements `ZODB.interfaces.IStorage`, `IBlobStorage`, `IStorageCurrentRecordIteration`, `IStorageIteration`, `IStorageRestoreable` and `loadBefore` in-process | must | `ZODB.tests` conformance suites pass |
| F2 | Serves the ZEO 6 msgpack protocol so an unmodified `ZEO.ClientStorage` works, blobs and shared blob directories included | must | ZEO client tests pass against the server; a Plone 6 site runs on `<zeoclient>` |
| F3 | Same on-disk format in both modes; switching mode is a restart | must | open a store written in-process with the server and back, no conversion |
| F4 | Two-phase commit with per-OID locks; concurrent transactions on disjoint objects commit in parallel | must | `MTStorage` tests; contention benchmark in 11 |
| F5 | Conflict resolution without Python on the server: structural merges (tier 1), policy-driven field-level three-way merges (tier 2), client-side `_p_resolveConflict` through the vote reply of ZEO 6, for every client (tier 3), shadow mode and a resolution log | must | `ConflictResolution` tests; BTrees and `Length` merge as today |
| F6 | Tier 1 structural merges for BTree buckets, sets and `Length` with int, float, string, bytes and tuple keys; tier 2 field-level merges driven by the policy that `zoe.plone` derives from Dexterity schemas | should | differential test against the Python resolver on generated conflicts |
| F7 | Early write-intent locks, negotiated per client, falling back to vote-time locks for stock clients | should | contention benchmark: zero ConflictErrors on a hot counter with the extension on |
| F8 | Pack with garbage collection, online: reads never wait, and a commit waits for at most one batch of the pack | must | pack during the write benchmark; `ZODB.tests.PackableStorage` |
| F9 | Invalidations delivered to every connected client after each commit; `getInvalidations(tid)` replay on reconnect | must | ZEO reconnect tests; cache verification test |
| F10 | Iterator, `history`, `loadSerial`, `restore` for backup, verification and `zodbconvert` | must | round-trip a Plone `Data.fs` through `zodbconvert` both ways, byte-identical pickles |
| F11 | Undo: `IStorageUndoable` (`undo`, `undoLog`, `undoInfo`); undoing a record that is no longer current is a three-way merge through the resolution tiers; no undo past the last pack; a blob undo re-links the previous blob file | should (phase 3) | `ZODB.tests.TransactionalUndoStorage` and the ZEO undo tests green; `supportsUndo()` returns `False` until then |
| F12 | `zope.conf` stanza `<zoe path="…">` and a `zodburi` scheme `zoe://` | must | a Plone instance starts with the stanza alone |
| F13 | Server extensions are negotiated through `register`/`get_info`; stock ZEO clients never see them | must | stock ZEO client passes with every extension enabled server-side |
| F14 | Converters in Rust: Data.fs → ZOE, RelStorage (PostgreSQL) → ZOE, ZOE → Data.fs, ZOE → ZOE; resumable, with a verification report | must (Data.fs), should (RelStorage) | round trip on a production copy: identical tids, byte-identical pickles, blobs present |
| F15 | Loupe desktop app: folder and server modes, object browser, time travel, live tail, space, integrity, operations | must (browser, time travel) / should (rest) | tauri-driver end-to-end suite green against a Plone 6 store |
| F16 | Loupe full CRUD through zoe-pickle with byte-identical round trip, expert mode, pickle diff before commit, and the optional zoe.plone reindex queue (not in V1) | should | pickle round-trip property tests pass on the corpus; an edit in Loupe is visible and reindexed in Plone within 10 s |
| F17 | Catalog mirror: ZCatalog queries answered by zoe-catalog as of the client's snapshot tid through the catalog\_query extension and the zoe.plone.catalog hook, with fallback to the Python path for unsupported indexes or old snapshots | should (phase 5) | differential suite: identical rids, order and counts on the Plone test suites and a production query log |
| F18 | Native BM25 text index on tantivy: ZoeTextIndex, delta records in the transaction, stemming per site language, accent folding, phrase, prefix, fuzzy, field boosts, snippets, ZCTextIndex query translation and compatibility switch | should (phase 5) | compatibility mode identical to ZCTextIndex on the Plone test suites; enhanced mode beats it on a labelled query set in the site's language; p99 under 20 ms |
| F19 | Catalog as a projection: delta records for every index type, persisted delta log with snapshots, proxy index classes, one-time rebuild migration | may (phase 9) | a production site runs on the projection with zero bucket conflicts and identical results over a full release cycle |
| F20 | MCP server: ops and dev tools and prompts read-only (phase 2), content tools with search, get\_document, list\_children, similar (phase 7); stdio and streamable HTTP; principal-based security with the roles bitmap applied before ranking | should (ops, phase 2) / should (content, phase 7) | MCP conformance with the reference client; security suite: an anonymous principal never receives a restricted passage |
| F21 | Native retrieval: versioned passages, pluggable embedding providers (local ONNX default), VectorIndex trait with builtin (stored in the DB) and Qdrant backends, hybrid search with reciprocal rank fusion, rebuild without Plone | should (phase 7) | builtin and qdrant agree on top-k within tolerance; hybrid beats BM25 on the labelled query set; as-of correctness under write load |
| F22 | Several named storages per server: one `zoe serve` process serves any number of stores selected by `register(storage_id)`, each with its own path, blob store, read-only flag, quotas and metric labels; Plone mount points work unchanged, in both modes | must | a Plone site with a mount point runs on one server; the ZEO multi-storage tests green; a read-only storage refuses `tpc_begin` |
| F23 | Change feed: a resumable stream of committed transactions (tid, oids, classes, user, description, extension) from any tid, on the admin port, for external indexers, cache purgers, Loupe and MCP | should (phase 6) | an external indexer driven by the feed misses no commit through the kill loop; resume from a tid replays exactly once |
| F24 | Forks: a copy-on-write overlay store (`zoe fork`), the native DemoStorage, for test layers, upgrade rehearsals and sandboxes on real data | should (phase 6) | a fork of a production copy opens without copying it and a Plone upgrade runs on it; `plone.app.testing` layers run on forks |
| F25 | Data tools in Rust: `zoe verify` (references, classes, checksums), `zoe rename-class` (zodbupdate without Python), `zoe export --parquet` for chosen classes and attributes | should (verify phase 2; rename-class and export phase 6) | same findings as `zodbverify` and `zodbupdate` on the corpus; the Parquet export loads in DuckDB |
| F26 | Pack safety: dry-run report, a retention window before physical deletion, incremental pack with no pack weekend | should (dry-run phase 1; the rest phase 6) | the dry-run report matches what the pack removes; a pack is reverted inside its window; incremental pack keeps p99 reads inside the benchmark budget |
| F27 | Read-only and maintenance modes: `zoe serve --read-only`, read-only fallback while a follower is promoted, a maintenance mode that drains clients cleanly | must (read-only phase 2; fallback phase 4; maintenance phase 6) | ZEO read-only tests green; reads continue through a failover; a drain completes every in-flight `tpc_finish` |
| F28 | Native Rust client for Python: `zoe.zeo.ClientStorage` as a drop-in for `ZEO.ClientStorage`, built on `zoe-client`, with an mmap'd compressed persistent cache and the extensions on by default | should (phase 8) | ZEO client tests green against it; beats `ZEO.ClientStorage` on the client benchmark, cold and warm cache |
| F29 | Sequences: storage.sequence(name).next() monotonic per store, allocated outside the 2PC lock path, in-process and over the protocol; zoe.plone wires \_nextwid, intids and Length-backed counters to them | must (primitive phase 1; protocol and zoe.plone wiring phase 2) | 0 conflicts on id allocation with 50 concurrent creators; ZEO conformance unchanged |
| F30 | Merge primitives: append-only fields (append(oid, field, record), materialised on load), CRDT sets and registers; zoe.plone behaviours for workflow\_history, comments and audit entries | should (phase 3) | 0 conflicts on workflow\_history on replay; the materialised list equals the Python one byte for byte |
| F31 | KV namespaces: an ephemeral namespace with TTL (sessions, CSRF keys, locks: no history, no pack) and a durable flavour for plone.app.registry; zoe.plone provides the session data manager, the locking backend and the registry storage | should (phase 3) | session, locking and registry suites green on the zoe.plone backends; control panels and GenericSetup import unchanged; an expired key is gone without a pack |
| F32 | Incidental writes: a writes-on-GET report in metrics and Loupe, then a context that commits side-effect writes (last login, member data, lock info) asynchronously at low priority, lost on crash by design | should (report phase 3; async path after the outbox) | a last-login write on a GET causes no ConflictError and no fsync on the request path |
| F33 | Mirror extensions: incremental security index (inherited allowedRolesAndUsers as parent pointers, resolved at query time), traversal prefetch of a path's chain from the path trie, reference index (back-references from the decode pass, for link integrity and relations), order index for ordered folders, shadow rebuilds with atomic swap | should (phase 5) | security bitmaps equal to reindexObjectSecurity on the corpus; a local-role change on a folder with 50,000 descendants writes O(1) records; a shadow rebuild serves queries throughout |
| F34 | Outbox and derivations: a transactional outbox with at-least-once post-commit delivery (the change feed is one consumer); derivations keyed (oid, tid, kind), computed by workers, invalidated by the store, with their own retention; plone.scale first, embeddings in phase 7 | should (phase 6) | no delivery lost through the kill loop; plone.scale writes nothing on GET once on derivations |
| F35 | Commit forensics: zoe replay \<tid> opens a fork at tid-1 with the recorded read set of the transaction; read sets recorded behind a sampling flag or for conflicting transactions | should (phase 6) | a production ConflictError reproduces on the fork |
| F36 | Versioning from history: a zoe.plone versioning behaviour backed by the store's (oid, tid) history and the retention window, replacing CMFEditions storage for sites that opt in; field diffs through zoe-pickle | should (phase 6, after retention) | plone.app.versioningbehavior tests green on the behaviour; the history view shows the same versions as CMFEditions on a converted site |
| F37 | Answer cache: answers stamped with their tid and cited oids, keyed by question embedding, invalidated through the feed, exposed by zoe mcp; the local ONNX provider behind a build flag | should (phase 7) | no cached answer served after any cited object changes; hit rate reported on the labelled query set |
| F38 | Fast paths: blob\_url(oid, ttl) on the S3 blob store with zoe.plone redirecting downloads; blob dedup by BLAKE3; one shared mmap'd cache per node for the native client | should (phase 8) | a download never passes through Zope; two workers on one node share one cache and apply each invalidation once |

**Non-functional**

| # | Requirement | Level | Accepted when |
| --- | --- | --- | --- |
| N1 | Durability: an acknowledged `tpc_finish` survives `kill -9` and power loss at any instruction | must | crash-consistency harness in 11, 10,000 runs, zero losses |
| N2 | Atomicity and isolation: readers see either none or all of a transaction; `loadBefore` is consistent at any tid | must | MVCC tests; concurrent reader during group commit |
| N3 | Latency: p99 `loadBefore` under 0.3 ms in-cluster for a cache miss on a 10 GB store | should | server benchmark, section 11 |
| N4 | Throughput: at least 3× ZEO 6 on the same hardware for mixed read/write, and faster than RelStorage/Postgres in every `zodbshootout` scenario | must | `zodbshootout`, section 11 |
| N5 | Commit cost: group commit coalesces fsyncs; on a 10 ms fsync device, 50 concurrent writers reach at least 1,000 commits/s | should | contention benchmark |
| N6 | Memory: server RSS bounded by configuration, independent of store size | must | 100 GB store under a 2 GB limit |
| N7 | Startup: ready to serve within 2 s on a 100 GB store (no index rebuild) | must | restart test |
| N8 | Free-threaded: extension declares `Py_mod_gil_not_used`; no data race under ThreadSanitizer in the core | must | TSan CI job; 3.14t in the matrix |
| N9 | Observability: Prometheus metrics, structured logs, health and readiness endpoints | must | Tier 1 manifest in section 10 |
| N10 | Graceful shutdown on SIGTERM: in-flight `tpc_finish` completes, staged transactions abort, clients get a clean close | must | shutdown test under load |
| N11 | Portability: Linux x86-64 wheels for CPython 3.12, 3.13 and 3.14 plus the 3.14t free-threaded build, and a static server binary; macOS and Windows (wheels, binary, Loupe packages) in a later phase | must | CI release job |
| N12 | Compatibility: ZODB ≥ 6.0 and ZEO ≥ 6.0 (Plone 6.1+); existing pickles, oids and tids unchanged; `zodbpickle.binary` honoured | must | F10 round trip on a production copy |
| N13 | Integrity: a checksum on every record and every blob, verified on read; `zoe scrub` walks the store at low priority and reports every mismatch | must (checksums phase 1; scheduled scrub phase 6) | bit-flip injection harness: every corruption detected on read; scrub finds every injected fault |
| N14 | At rest: zstd per record, with a dictionary trained on the store's own records and delta chains between revisions added only where the benchmark shows they pay; optional AES-GCM encryption of records and blobs with the key from a file, the environment or a KMS | should (compression phase 1; encryption phase 6) | a Plone store shrinks by at least 3× against `Data.fs`; conformance green on an encrypted store; no plaintext record or blob on disk |
| N15 | Backup: continuous shipping of the transaction log and blobs to a bucket, snapshots, `zoe restore --to-tid` for point-in-time recovery, backup verification as a scheduled job; repozo-style dumps as the fallback | must (dumps phase 2); could (continuous backup and PITR, after the roadmap) | a dump restores to a byte-identical store, verified by a scheduled job |
| N16 | Guardrails: configurable maximum record, transaction and blob sizes; per-storage quotas; long-lock and slow-transaction logging | must (limits phase 1; quotas phase 6) | an oversized store fails at `store` with a clear error; a lock held past the threshold is logged with its oid and client |
| N17 | Traces: OpenTelemetry spans per request, lock wait, fsync and pack step; a `trace_context` extension carries the client's traceparent so a slow Plone request and its server work share one trace | should (phase 2) | a Plone request trace shows its server spans in Jaeger or Tempo |
| N18 | Deterministic simulation: zoe-core, zoe-server and zoe-client written against clock, I/O and network traits; a simulator drives a whole cluster with seeded schedules and fault injection; every phase gate from phase 2 runs under it | should (traits phase 1; simulator phase 2) | the kill loop and the Tier 2 failover test run inside the simulator; a failing seed reproduces deterministically |
| N19 | Format room: every record carries a codec byte, a branch id and a typed-field-slot header, every blob record its BLAKE3 hash, from the first store written; the fields stay unused until their feature ships, so no format migration when it does | must (phase 1) | a phase 1 store opens unchanged under every later phase's build; conformance green on it |

## Architecture overview

One Rust crate implements the storage; two thin skins expose it, in-process and over the ZEO wire. Everything above the storage seam is today's Plone, untouched.

&#91;embedded content: architecture · two deployment modes over one storage core\]

The two grey containers are deployment choices; only the tinted core is new code, and both doors call it the same way.

**Layers, top to bottom**

1. **Plone / Zope, unchanged.** Worker threads, one `ZODB.Connection` and pickle cache each, `persistent` and `BTrees` as they are, `transaction` as it is. Lazy loading stays synchronous.
2. **Storage seam.** In simple mode, `zoe.Storage` implements `IStorage` and friends and is wrapped by ZODB's own `MVCCAdapter`, exactly like `FileStorage`. In clustered mode, the unmodified `ZEO.ClientStorage` talks msgpack to the server.
3. **Server skin (clustered mode only).** A tokio process: one task per client connection, pipelined requests with out-of-order replies, invalidation fan-out, blob streaming, health and metrics. A single static binary: no Python inside, conflict resolution in Rust, the long tail resolved by the client.
4. **Storage core (the crate).** Synchronous, no tokio inside, no Python inside. Transaction coordinator (2PC state machine), per-OID lock manager, conflict detection, resolution dispatch (structural, then field-level by policy, the long tail to the client), pack, blob store, and the `Storage` trait that backends implement.
5. **Durable store.** A backend behind the trait: redb first (embedded, ACID, single writer, MVCC), an append-only log with in-memory index as the speed option, FoundationDB or TiKV for Tier 3. Blobs are files beside the store, or an S3-compatible bucket with a local cache.

**Two modes from one core.** `zoe.Storage(path)` and `zoe serve --path …` open the same directory with the same format: `zoe.redb` (the database), `lock` (the single-writer lock, an exclusive `flock` held while the store is open) and the blob store. A new database is written as `zoe.redb.new`, synced, renamed to `zoe.redb` and the directory synced, so a crash during the creation leaves no store or a complete empty one. Moving from one Zope instance to a cluster is: stop the instance, start the server on the directory, point the instances at it.

**One write, clustered mode**

1. A worker thread changes objects; on commit, `ClientStorage` sends `tpc_begin`, one `storea` per changed pickle, then `vote`.
2. The protocol layer decodes each frame and hands it to the coordinator, which stages the pickles for that transaction in a per-transaction buffer.
3. On `vote`, the lock manager acquires per-OID locks for the written oids in oid order. A second transaction on the same oids queues; transactions on disjoint oids proceed in parallel.
4. For each staged oid whose current serial is newer than the serial the client saw, resolution runs in Rust: a structural merge for BTree buckets, sets and `Length`, then a field-level three-way merge where the merge policy allows it. What neither tier resolves goes back in the vote reply, as ZEO 6 lists a conflict for the client to resolve; the client runs the class's own `_p_resolveConflict` locally, re-stores and votes again, with the locks still held; a class that does not resolve raises `ConflictError` in the client.
5. On `tpc_finish`, the coordinator allocates the tid, writes all records under one backend transaction, fsyncs (group commit coalesces concurrent finishers), releases the locks, acknowledges the client, and broadcasts one invalidation message per other client with the oid list.

**One read.** `loadBefore(oid, tid)` → protocol layer → core → backend range lookup on `(oid, < tid)` → the pickle and its serial, written to the socket without an intermediate copy. In simple mode the same call is a Python method into the core with no wire.

**Where async lives.** In the server skin, and in the crates that talk to a network or to an agent: `zoe-client`, `zoe-bench`, `zoe-mcp` and the server mode of Loupe. The core is synchronous, and the extension module `zoe._zoe` never links tokio. The ZODB side of a client is synchronous; `prefetch` is how a client overlaps I/O, and the server answers prefetched loads out of order on the same connection.

## Storage core

The core is a synchronous Rust library, `zoe-core`, with no Python and no tokio inside; both skins call it. It owns correctness: the data model, 2PC, locking, conflict handling, pack and blobs. Backends own bytes.

**Data model**

| Key space | Key | Value | Notes |
| --- | --- | --- | --- |
| `records` | `(oid: u64, tid: u64)` big-endian, 16 bytes | a record frame, checksum tag 1; its header carries the serial of the previous record, its payload the pickle bytes or their zstd frame, as the codec byte says | one row per object version; a 0-length payload marks a deletion by `deleteObject`; pack removes rows and writes no deletion |
| `txn_oids` | `(tid: u64, oid: u64)` big-endian, 16 bytes | empty, not a frame, so no checksum tag | the transaction index: one key per record, deletions included, written in the same backend transaction as the record; a range scan over one tid gives the oids of that transaction in oid order; the iterator reads the oids of its range only from it; pack removes a key with its record; a store without the table does not open; ADR 0026 |
| `txns` | `tid: u64` big-endian, 8 bytes | a frame, checksum tag 3, whose payload is the status byte, the oid count as a `u64`, then user, description and extension, each with a `u32` length; the oid count and the three lengths are little-endian | the transaction log; status is `committing` until the finish record lands; pack lowers the oid count to the records that remain and removes a row that keeps none |
| `blobs` | `(oid: u64, tid: u64)` big-endian, 16 bytes | a frame, checksum tag 2, with previous serial 0 whose payload is the BLAKE3 hash of the blob file (32 bytes), then the size of the file in bytes as a little-endian `u64` | one row per blob revision, written in the same backend transaction as its record; the file lives in the blob store, at the BushyLayout path of `(oid, tid)`; a store without the table has no blobs |
| `sequences` | the sequence name, UTF-8 text | a frame, checksum tag 5, with codec 0 and previous serial 0 whose payload is the end of the reserved values as a little-endian `u64`: the first value that the store has not reserved; the checksum covers the name | one row per sequence; a reservation writes it in a backend write transaction of its own, durable before any value of the block is given; a restore raises it with `Sequence::restore_end`, which never lowers it; values are reserved in blocks of 100, so each reopen, after a crash or not, skips at most 100 values of a name and never repeats one; the skips of reopens in a row add up, so a caller can see a larger gap; not transactional: an abort keeps its values; pack never touches it; a store without the table has no sequence yet; ADR 0024 |
| `meta` | fixed text keys | `format` (`ZOE format 1`), `last_tid`, `next_oid`, `pack_watermark`; numbers are 8-byte big-endian; not frames, so no checksum tag | one row each; pack writes `pack_watermark`, the tid of the last pack, with its first batch, and a store that was never packed has no such row |

`loadBefore(oid, tid)` is a reverse range scan from `(oid, tid - 1)`; `load(oid)` is `loadBefore(oid, MAX)`; `loadSerial` is a point read. This is the RelStorage history-preserving layout, which is what makes MVCC and `history()` free.

**Tids.** A tid is ZODB's 8-byte `TimeStamp`, strictly increasing. The core allocates it at `tpc_finish` under the commit sequencer: `max(now, last_tid + 1 tick)`. Clients may pass a tid only through `restore` (copy and replication paths), never through normal commits: `tpc_begin` with a tid starts a restore transaction, which takes both `store` and `restore`, and its `tpc_finish` commits under that tid. The tid must be after the last tid, both at `tpc_begin` and at `tpc_finish`, and it must not be `MAX`. Otherwise the call is refused, and a refusal at `tpc_finish` does not halt the store. A normal commit never gets `MAX`; when no tid below it is left, `tpc_finish` is refused. In Tier 3 the sequencer is one key updated inside the backend transaction (or an FDB versionstamp).

**Two-phase commit, per transaction**

```
Begin ──storea*──▶ Staged ──vote──▶ Locked ──tpc_finish──▶ Durable ──▶ Invalidated
   │                 │                │
   └── tpc_abort ────┴── tpc_abort ───┴── tpc_abort (locks released, staging dropped)
```

- `Staged`: pickles sit in a per-transaction buffer: in memory up to `staging-memory` bytes (default 16 MiB), and beyond it in a spill file of the transaction under `staging/` in the store directory, as byteserver does. A spill file is never durable data: nothing syncs it, the transaction removes it at finish or abort, and a writer open removes the spill files that a crash left. Nothing is visible.
- `Locked`: per-OID locks held for every written oid and every oid passed to `checkCurrentSerialInTransaction` (shared lock). Conflict detection and resolution happen here, so a failed vote releases locks before any disk write.
- `Durable`: one backend write transaction holds the records, their keys in the transaction index, the txn row with status `committed`, and the meta update; it is fsynced before the client is acknowledged. A crash before the fsync leaves nothing; a crash after it leaves a complete transaction. No repair step exists because there is nothing half-written to find.
- `Invalidated`: after acknowledgement, the oid list is pushed to every other client (server skin) or applied to the in-process invalidation queue.

**Lock manager.** A `HashMap<oid, LockState>` with a wait queue per oid, protected by one mutex (contention on the map is negligible against disk time; a sharded map is a later optimisation). Locks are acquired in ascending oid order, so two transactions cannot deadlock on each other; a transaction that must wait releases nothing and queues once. Shared locks (readCurrent) and exclusive locks (writes) follow the usual compatibility matrix. A lock-wait timeout (default 10 s) turns a wait into a `ConflictError`, so a stuck client cannot stall the store.

**Early write-intent locks (extension).** A client that negotiated the extension sends `lock_intent(oids)` as soon as a transaction registers a modified object. The lock manager grants exclusive locks early; a competing writer on the same oid waits until the first transaction finishes or aborts, then re-reads the object and proceeds without conflict. Deadlock avoidance: intent locks are also acquired in oid order, so a request for a lower oid while holding a higher one is rejected with `LockOrder`, and the client releases and re-acquires the whole set. In-process mode calls the same function directly. Section 9 explains why this is the one mechanism that avoids conflicts instead of resolving them.

**Conflict resolution dispatch**

1. Detect: for each staged `(oid, serial_seen)`, compare with the current serial; equal means no conflict.
2. Tier 1, structural, in Rust: BTree buckets, sets and `Length` whose keys Rust can order (ints, floats, strings, bytes and tuples of those) merge with the rules of `BTrees._base` and `Length._p_resolveConflict` and are re-encoded by `zoe-pickle`. Object keys that need Python ordering fall through.
3. Tier 2, field-level, in Rust: for any persistent object whose class the merge policy marks mergeable, decode the old, committed and new states, compute the two changed-key sets, merge when they are disjoint or agree, and apply the per-field strategies the policy names (`lww`, `sum`, `max`, `union`, `append`, or a named native merge such as an append-only `workflow_history`). Fields the policy marks coupled stay strict.
4. Tier 3, the long tail: classes with their own `_p_resolveConflict` and no policy. The core keeps the locks of the vote and returns the committed states (`vote_resolving`). In server mode, the reply of `vote` lists each such conflict as a ZEO 6 server with `client-conflict-resolution` does: `{oid, serials: [committed, read], data}`, where `data` is the pickle that the client stored. Every stock `ZEO.ClientStorage` accepts this reply, with no negotiation: it loads the read and the committed states with `loadSerial`, runs the method locally, where the class already lives, re-stores the resolved state over the committed serial with `storea` and votes again. The locks stay held between the two votes; a stock ZEO server releases its commit lock there. The second vote takes no lock, checks every serial again, and replies `[]` or fails. A class that does not resolve raises `ConflictError` in the client, which aborts; a client that disconnects between the two votes releases the locks. In-process mode calls the method directly through the `ConflictResolvingStorage` mixin, with the same vote: the storage re-stores the resolved states and votes again. A stale read-current serial is never resolved, nor is a new object, a deletion or a deleted object: the vote fails at once.
5. Unresolved: the vote fails with `ConflictError`, or the client raises it when the class does not resolve. No tier ever runs Python on the server.

The merge policy is a versioned object in the store, so it is transactional and the server reloads it on change. `zoe.plone` generates it from Dexterity schemas (a type whose schema declares no `@invariant` is mergeable field by field; fields named in an invariant are coupled) plus a short overrides file, and Loupe shows and edits it. Shadow mode runs tiers 1 and 2 on every conflict but still raises, recording what would have been merged by class, tier and field; merging is switched on per class once a production run shows it would have been right, judged by replaying the merged states through the differential suite. Every resolution, merged or raised, is logged with oid, class, tier, the keys merged and both transactions' user and description. Tier 1 and 2 merges are replayed through the Python resolvers in the test suite and must produce identical pickles.

**Pack.** Pack at tid T keeps what a reader at T or later needs; T is the lower of the requested tid and the last tid. It keeps every record after T and, for each object that such a reader can reach, the newest record at or before T, a deletion included; it removes every other record at or before T with its blob record, then its blob file. It removes rows and writes no deletion record. The roots of the mark are the root object and every object that a record after T refers to, so an object that a later record links again stays; from each root the mark follows the newest record at or before T. References come from `zoe-pickle`, which reads the persistent id of every BINPERSID of a record, opaque parts included, as ZODB's `referencesf` does, and never unpickles; weak references and references into another database keep nothing. A reachable record that does not decode fails the pack before it removes anything. Pack runs in batches, in key order, the oldest record of an object first. Each batch is one backend write transaction that removes records with their keys in the transaction index and their blob records, lowers the oid count of their transaction rows and removes a row that keeps no record, so `history` and the iterator stay consistent. Readers never wait. A finish waits for at most one batch: while a pack runs, each finish notes its oids, and before each batch the pack holds the commit sequencer, marks what the noted records refer to and commits the batch, so a record committed during the pack that links an object again keeps it. The first batch writes `pack_watermark`; nothing is undoable past it, and a pack at an earlier tid does nothing. Blob files are deleted after the removal of their blob records is durable: before the commit, an intent file in `blobs/tmp/` names them, so a crash leaves no blob file that the next open does not remove. A client that links an object from a reference read before T can still link an object that a batch removed, as with ZODB's own pack. A failed batch write halts the store as a failed finish does; a record that fails its checksum stops the pack, which never deletes the evidence of a corruption; a read-only store refuses a pack. ADR 0025 records these decisions. `pack` is exposed on both skins and as a CLI (`zoe pack --days 7`).

**Blobs.** The blob store is a trait with two implementations: a directory tree beside the store, and an S3-compatible store with a local LRU cache. The directory tree is `blobs/` in ZODB's `BushyLayout`, or the directory of the `blob-dir` option (ADR 0027), so existing tools understand it: `blobs/.layout` holds `bushy`, and the file of `(oid, tid)` is one directory per big-endian byte of the oid, then the tid as ZODB's `tid_repr` writes it, for example `blobs/0x00/0x00/0x00/0x00/0x00/0x00/0x12/0x34/0x03f6db4f24444444.blob`. A committed blob file is read-only and immutable per `(oid, tid)`. `storeBlob` writes into a temp file in `blobs/tmp/`, computes its BLAKE3 hash on the way, and syncs it. `tpc_finish` places the files before the backend commit: it writes and syncs an intent file in `blobs/tmp/` that names the tid and the oids, renames each file into place and syncs its directory. The backend write transaction then holds each blob record (hash and size) next to its record, and the intent file is removed. So a crash never leaves a blob record without its file. A crash can leave a file without its record: the next open reads only `blobs/tmp/`, never the whole tree, and removes the files that an intent file names and that have no blob record. Every read through the core checks the size and the hash at the end of the file; a mismatch is a distinct `CorruptBlob` error with the oid and tid. `loadBlob` returns a path (simple mode) or streams chunks (server mode). ADR 0023 records the blob record and the crash order.

**Undo.** Every `(oid, tid)` record stays until pack, so undoing a transaction T is a read and a write: for each oid in T, the undo transaction stores the state the oid had before T (`loadBefore(oid, T)`), or a delete record when T created it. When the oid's current record is T's, that is the whole job; when later transactions changed it, the write goes through the resolution tiers as a three-way merge, with T's state as the base, the current state as ours and the pre-T state as theirs, and fails with `UndoError` when no tier resolves it. A blob record points at the earlier blob file, immutable per `(oid, tid)`. `undoLog` and `undoInfo` read the transaction index; nothing is undoable past the last pack. In server mode `undoa` queues the undo into the running transaction and `vote` does the work, as ZEO's own server does.

**Record framing: checksums, compression, encryption.** Every record, blob record, transaction row, sequence row and intent file is a frame: a 32-byte little-endian header, then the typed-field slots, then the payload. The header holds the frame version (byte 0), the flags (1), the codec (2), a reserved byte (3), the branch id as a `u32` (4 to 7), the previous serial (8 to 15), the payload length as a `u32` (16 to 19), the length of the typed-field slots as a `u16` (20 and 21), two reserved bytes (22 and 23) and the checksum (24 to 31): xxh3-64 with seed 0 over a one-byte tag of the key space, then the key that the frame is stored under (the 16-byte big-endian `(oid, tid)` key of a record or a blob record, the 8-byte big-endian tid key of a transaction row or an intent file, the UTF-8 name of a sequence row), then bytes 0 to 23, the slots and the payload, so a frame under the wrong key or in the wrong key space fails it. The tags are 1 for `records`, 2 for `blobs`, 3 for `txns`, 4 for the intent files and 5 for `sequences`; 0 is never a tag, 6 is reserved for `vectors`, and a tag never changes and is never reused (ADR 0029). The tag is not stored: a reader checks with the tag of the table or file that it reads. The checksum (xxh3-64, CRC32C on backends that already keep one) is verified on every read and a mismatch surfaces as a distinct `CorruptRecord` error with the oid and tid (`CorruptBlob` with the oid and tid for a blob record, `CorruptTxn` with the tid for a transaction row, `CorruptSequence` with the name for a sequence row), never as a `POSKeyError`. The codec byte says how the payload holds the data: 0 is the data as it is, 1 is one zstd frame that states the data length in its own frame header, and a codec that a build does not know is refused as a newer format. The store compresses each record with zstd at a configured level, 3 by default, and keeps the result only when it is smaller than the data; otherwise it writes the data as it is with codec 0. Transaction rows, blob records and intent files use codec 0. A store reads every codec whatever its own setting, so the setting can change between opens. The payload length in the header and the checksum cover the stored bytes, so a read verifies them before it decompresses. Decompression is bounded: the zstd payload is exactly one standard zstd frame (its magic number is checked, so no skippable or legacy frame passes), the length that it states is at least 1 (empty data marks a deletion, so it is never compressed) and at most `u32::MAX` and at most 32,768 times the stored length (a zstd writer puts at most 128 KiB in a block of at least 4 bytes; libzstd accepts larger blocks, so the reader enforces this ratio itself), and it must decode to exactly that length; any other payload is a `CorruptRecord`. A later codec adds a dictionary trained on a sample of the store's own records, kept in the store header and versioned, so a pickle-heavy Plone store shrinks three to five times and the page cache holds proportionally more objects. With encryption on, the frame is sealed with AES-256-GCM under a per-store data key, itself wrapped by a key from a file, the environment or a KMS; the tid and oid are the nonce material, so no two records share a nonce. `zoe scrub` reads every frame at low priority, in the background or on a schedule, and reports mismatches to the metrics and the admin log; Loupe's integrity screen shows the last scrub.

**Forks.** A fork is an overlay store: reads fall through to the base at a fixed tid, writes land in the overlay, and the overlay has its own tids, pack and blobs. `zoe fork base sandbox --at <tid>` opens one without copying the base; discarding it is deleting a directory. It is ZODB's `DemoStorage` done natively, so `plone.app.testing` layers stack forks instead of DemoStorages, an upgrade is rehearsed on a fork of a production copy before it touches the real store, and Loupe opens a fork as it opens any store. A fork is never merged back; what is kept is reapplied as a normal transaction.

**Pack safety.** `zoe pack --dry-run` prints what a pack would remove, by class and by size, without touching the store. A pack never deletes at once: removed records move into a retention window (`pack-retention`, a number of days) during which `zoe pack --revert` restores them, and physical deletion happens when the window closes. Pack is incremental by default, a bounded slice of oids per step interleaved with normal traffic, so there is no pack weekend; a full pack remains available for migrations. Until the retention window and incremental pack ship in phase 6, a pack removes records at once and every pack is a full pack.

**Guardrails.** `max-record-size`, `max-transaction-size` and `max-blob-size` refuse a `store` with a clear error before anything is written; a lock held longer than `lock-warn` and a transaction open longer than `transaction-warn` are logged with their oid, client and duration, and counted in the metrics. The record size is the length of the pickle, the transaction size is the sum of the pickles of the transaction, and the blob size is the length of the blob file; blob bytes do not count toward the transaction size. A size equal to a limit passes, and no limit is set by default. A restore transaction checks no size limit, blobs included, so a copy such as `zodbconvert` never refuses what its source took. The error is `TooLarge`, which names the configuration key of the limit and holds no pickle bytes.

**The storage trait**

```rust
pub trait Backend: Send + Sync {
    fn load_before(&self, oid: Oid, tid: Tid) -> Result<Option<Record>>;
    fn load_serial(&self, oid: Oid, tid: Tid) -> Result<Option<Bytes>>;
    fn load_blob(&self, oid: Oid, tid: Tid) -> Result<Option<BlobRecord>>;
    fn last_tid(&self) -> Tid;
    fn begin_write(&self) -> Result<WriteTxn<'_>>;      // one per tpc_finish or pack batch
    fn iter_txns(&self, from: Tid, to: Tid) -> TxnIter<'_>;                                // reads the rows as it goes, from one snapshot
    fn history(&self, oid: Oid, limit: usize) -> Result<Vec<RecordMeta>>;
    fn load_before_next(&self, oid: Oid, tid: Tid) -> Result<(Option<Record>, Option<Tid>)>; // loadBefore and the next serial, one snapshot
    fn txn_oids(&self, from: Tid, to: Tid) -> Result<BTreeMap<Tid, Vec<Oid>>>;              // the oids of each transaction of a range
    fn indexed_tids(&self, from: Tid, to: Tid, limit: usize) -> Result<Vec<Tid>>;           // a slice of the tids of the index, for the iterator
    fn current_from(&self, from: Oid) -> Result<Option<(Oid, Record)>>;                     // the current record of the first oid ≥ from
    fn pack(&self, before: Tid, reachable: &dyn Fn(Oid, Tid) -> bool) -> Result<PackStats>; // not used: pack goes through the methods below
    fn pack_watermark(&self) -> Result<Tid>;                                                 // the tid of the last pack, 0 before the first
    fn record_keys(&self, from: Oid, limit: usize) -> Result<Vec<(Oid, Tid)>>;              // the record keys of whole oids from `from`, for the pack scan
    fn txn_records(&self, tid: Tid) -> Result<Option<TxnRecords>>;                          // a transaction row, its index keys and their records, one snapshot
    fn sync(&self) -> Result<()>;                         // fsync barrier used by group commit
}

pub trait WriteTxn {
    fn put_record(&mut self, oid: Oid, tid: Tid, prev: Tid, data: &[u8]) -> Result<()>;
    fn put_blob(&mut self, oid: Oid, tid: Tid, blob: &BlobRecord) -> Result<()>;
    fn put_txn(&mut self, meta: &TxnMeta) -> Result<()>;
    fn remove_record(&mut self, oid: Oid, tid: Tid) -> Result<()>;  // pack
    fn remove_blob(&mut self, oid: Oid, tid: Tid) -> Result<()>;    // pack
    fn replace_txn(&mut self, meta: &TxnMeta) -> Result<()>;        // pack: a lower oid count, the last tid unchanged
    fn remove_txn(&mut self, tid: Tid) -> Result<()>;               // pack: a row that keeps no record
    fn put_pack_watermark(&mut self, tid: Tid) -> Result<()>;       // pack
    fn commit(self) -> Result<()>;                        // durable on return
}
```

The core, not the backend, implements locking, 2PC, resolution, invalidations and blobs, so a backend is a few hundred lines.

**Backends**

| Backend | When | Durability | Trade-off |
| --- | --- | --- | --- |
| redb (embedded, pure Rust, MVCC, single writer) | first release, both modes | ACID per write transaction, fsync on commit | copy-on-write B-tree: write amplification; simplest to get right |
| append-only log + in-memory oid→offset index (filestorage2 design) | when `zodbshootout` shows redb's write path is the bottleneck | sequential appends, one fsync per group, marker flip | custom format: own backup, verify and index-rebuild tools |
| FoundationDB or TiKV | Tier 3 | replicated before acknowledgement | network hop per operation; FDB caps a transaction at 5 s and 10 MB, so large commits stage locally and write in one short transaction |

**Group commit.** `tpc_finish` calls join a commit batch; the batch leader writes all members under one backend transaction, fsyncs once, and wakes every member. With per-OID locks already held, batching changes nothing about isolation. On a 10 ms fsync device this is the difference between 100 and several thousand commits per second.

## In-process mode

`zoe.Storage` is a PyO3 extension class that implements ZODB's storage interfaces directly over the core, so one Zope instance runs without any server, the way it runs on `FileStorage` today.

**Python surface**

| Interface | Methods | Implementation |
| --- | --- | --- |
| `IStorage` | `load`, `loadBefore`, `loadSerial`, `store`, `tpc_begin`, `tpc_vote`, `tpc_finish`, `tpc_abort`, `lastTransaction`, `new_oid`, `getName`, `getSize`, `__len__`, `sortKey`, `isReadOnly`, `registerDB`, `close`, `history`, `pack`, `checkCurrentSerialInTransaction`, `supportsUndo` | thin wrappers calling the core; the GIL (or, on `t` builds, nothing) is released around every core call. `pack(t, referencesf)` turns `t` into a tid as FileStorage does, `TimeStamp(*time.gmtime(t)[:5] + (t % 60,))`, and runs the online pack of the store at that tid, with the GIL released for the whole pack; it ignores `referencesf`, because the core reads the references in the pickles. A record that fails its checksum or that the pack cannot read stops the pack with `StorageSystemError`, whose text starts with "pack stopped" and names the record. `__len__` returns 0: `IStorage` asks only for an approximate length, and the store keeps no object count. `getSize` is the size of the files in the store directory. `tpc_transaction` is not provided: it belongs to ZEO's `IServeable`, and the storage runs many transactions at once |
| `IBlobStorage` | `storeBlob`, `loadBlob`, `openCommittedBlobFile`, `temporaryDirectory` | paths into the blob store. `storeBlob` takes a file of `temporaryDirectory()` by rename, copies a file from elsewhere and refuses another file of the store; `loadBlob` raises `POSKeyError` when the blob file is missing |
| `IStorageIteration`, `IStorageCurrentRecordIteration`, `IStorageRestoreable` | `iterator`, `record_iternext`, `restore`, `restoreBlob` | backup, verification, `zodbconvert` in both directions. `tpc_begin(transaction, tid, status)` begins a restore transaction under `tid`; `status` takes `" "` and `"p"`, and the store keeps no status, so the iterator gives `" "`. The iterator reads one transaction at a time through the transaction index, and the rows and the index tids of its range a slice at a time, so its memory does not grow with the range; only a run of rows that do not read stays in one slice. Its records have `data_txn` `None`. `record_iternext` past the last object raises `ValueError`, as FileStorage does. `copyTransactionsFrom` copies another storage with `restore` and `restoreBlob` |
| `IStorageUndoable` | `undo`, `undoLog`, `undoInfo` | phase 3, on top of the merge tiers; `supportsUndo()` returns `False` until then |
| `IExternalGC` (the method only) | `deleteObject` | `deleteObject(oid, oldserial, transaction)` stages a deletion, a record with an empty pickle, through the core's `Txn::delete`. The vote checks `oldserial` as it does for `store`: a stale serial raises `ConflictError`, and the vote never resolves it. After the commit, `load` raises `POSKeyError`, and `loadBefore`, `loadSerial` and `loadBlob` still read the older records until a pack removes them. `deleteObject` of an oid that has no record, with an `oldserial` of zero, commits a deletion, where FileStorage raises `POSKeyError`. The class does not declare `IExternalGC`, so the interfaces of `zoe.Storage`, and the `interfaces` of `get_info` on the wire, do not list it |
| conflict resolution | `tryToResolveConflict` | tiers 1 and 2 run in the core; the class mixes in `ZODB.ConflictResolution.ConflictResolvingStorage`, so `tpc_vote` runs the long tail through the class's own `_p_resolveConflict` in the same process, with the locks held, and returns the resolved oids |
| extension | `lock_intent(oids)` | early write-intent locks, used by a small `Connection` subclass or hook (section 9) |

ZODB 5 wraps any storage that does not implement `IMVCCStorage` in `ZODB.mvccadapter.MVCCAdapter`, which builds per-connection snapshots from `loadBefore`. ZOE relies on that rather than implementing MVCC twice; `IMVCCStorage` is a later optimisation if profiling shows the adapter in the way.

**Opening.** `zoe.Storage(path, *, read_only=False, lock_timeout=None, blob_dir=None)` opens the store in the directory `path` and creates the directory when it is missing. `lock_timeout` is the lock-wait timeout in seconds; `None` keeps the default of the store. `blob_dir` is the blob directory, `path/blobs` when it is `None`; the core takes it as `Options::blob_dir` (ADR 0027). The store does not record its blob directory, so each open gives the same one: a store with blobs refuses a blob directory without a layout file with `StorageError`, because the option is missing or wrong or the directory is not mounted. A `blob_dir` that is not a directory raises `OSError` before anything is created. A blob directory without a layout file that holds other files, or that another open store holds, raises `StorageError` and keeps its files. A read-only storage refuses `tpc_begin`, `new_oid`, `store`, `deleteObject`, `pack`, `restore`, `storeBlob`, `restoreBlob` and `temporaryDirectory` with `ReadOnlyError`, and a missing directory, or a missing parent of `blob_dir`, with `FileNotFoundError`: it creates nothing. Until the core has a read-only open, it opens the store for writes and takes the exclusive lock. `close` aborts the open transactions, and the storage then refuses every call, a `tpc_begin` that races the close included; a call in progress keeps the store open until it returns. `zoe.Storage` is `zoe.storage.Storage`, a Python subclass of the extension class `zoe._zoe.Storage` and of ZODB's `ConflictResolvingStorage` that declares `IStorage`, `IBlobStorage`, `IStorageIteration`, `IStorageCurrentRecordIteration` and `IBlobStorageRestoreable`, and adds `tpc_vote` with the conflict resolution, `history`, `iterator`, `copyTransactionsFrom`, `pack`, `registerDB` and `supportsUndo`. `import zoe` does not import ZODB: `zoe.Storage` loads `zoe.storage` at first use.

**Thread model.** `Storage` is thread-safe and shared by all Connections of a process. Per-connection state lives in ZODB; per-transaction state is a core transaction in a table of the storage, keyed by the identity of the ZODB transaction object. A second `tpc_begin` of the same transaction, or a call with a transaction that the storage did not begin, raises `StorageTransactionError`; `tpc_abort` of an unknown transaction does nothing. The thread that calls `tpc_begin` drives the transaction: `store`, `deleteObject`, `restore`, `storeBlob`, `checkCurrentSerialInTransaction` and `tpc_vote` from another thread raise `zoe.WrongThread`, a subclass of `StorageTransactionError`, and change nothing. `tpc_finish` and `tpc_abort` may run on any thread, as ZODB's own `BasicStorage.test_tid_ordering_w_commit` does. On a free-threaded build, two worker threads can be inside `tpc_finish` at the same time; the core's lock manager and group commit sort them out. No mutex of the core or of the extension is held while Python code runs. The per-OID locks of a vote are not mutexes: they stay held while `_p_resolveConflict` runs, until the second vote ends, the transaction aborts or the storage closes. The module declares `Py_mod_gil_not_used` (PyO3 0.28 default), so loading it does not re-enable the GIL.

**Commit publication.** The core makes a commit durable and visible before `tpc_finish` calls its callback, which hands the invalidations to the other Connections. A commit is published when its callback has run, and `lastTransaction` gives only a published tid: it waits for the callback of the last commit, so a Connection never starts a snapshot after a commit whose invalidations it did not get. Callbacks run in commit order: the core gives each commit the tid of the commit before it, and a commit waits until that one is published. A callback that calls `lastTransaction` gets the tid before its own commit. A callback cannot commit in its own storage, because that commit would wait for the callback: its `tpc_finish` raises `StorageTransactionError` and commits nothing.

**Errors.** Core errors arrive as the exceptions of ZODB: `NotFound` as `POSKeyError(oid)`; `Conflict` as `ConflictError` and `ReadConflict` as `ReadConflictError`, both with `oid` and, for a stale serial, `serials == (current, read)` (a lock-wait timeout gives no serials); a `ConflictError` on a stored object also gets the stored pickle as `data`, from which ZODB reads the class name; a conflict that `_p_resolveConflict` does not resolve raises the `ConflictError` of `tryToResolveConflict`, which also gives `class_name`, and the failed vote releases its locks; `Protocol` and `LockOrder` as `StorageTransactionError`; `WrongThread` as `zoe.WrongThread`; `Halted`, `CorruptRecord`, `CorruptTxn` and `Backend` as `StorageSystemError`; `Locked`, `Closed`, `Format`, `TooLarge` and a call on a closed storage as `StorageError`; `Io` as `OSError`.

**Configuration**

```xml
%import zoe
<zodb_db main>
  <zoe>
    path  var/zoe
    # optional; defaults to <path>/blobs
    blob-dir  var/blobstorage
    # redb | log | fdb
    backend  redb
    lock-timeout  10s
    staging-memory  16MB
    # on comes with early locks
    early-locks  off
  </zoe>
  mount-point /
</zodb_db>
```

ZConfig keeps a `#` after a value as part of the value, so a comment takes a line of its own.

| Key | Environment twin | Value | Default |
| --- | --- | --- | --- |
| `path` | `ZOE_PATH` | the directory of the store, created when missing; a leading `~` is the home directory | none: the stanza or `ZOE_PATH` gives it |
| `blob-dir` | `ZOE_BLOB_DIR` | the directory of the blob files; a leading `~` is the home directory | `<path>/blobs` |
| `backend` | `ZOE_BACKEND` | `redb`; `log` and `fdb` when they are built; another value raises `ValueError` | `redb` |
| `lock-timeout` | `ZOE_LOCK_TIMEOUT` | whole seconds, as ZConfig's `time-interval`: `10`, `10s`, `2m` | the default of the store |
| `early-locks` | `ZOE_EARLY_LOCKS` | a boolean; `on` raises `ValueError` until early locks are built | `off` |
| `read-only` | `ZOE_READ_ONLY` | a boolean, as in `<filestorage>` | `false` |

A key of the stanza wins over its twin: the twin gives the value only when the stanza does not set the key, with the same syntax. An empty value of any key counts as not set, so the key takes its twin, then its default.

With the `uri` extra, `zoedb[uri]`, the same storage is reachable through `zodburi` as `zoe:///var/zoe?backend=redb` for scripts, `zodbverify` and `zodbconvert`. The path of the URI is `path`, and it is absolute. The query takes the other keys of the table, with the same names and values, each at most once, and the URI does not read the twins. The query also takes the database keys of zodburi, such as `database_name` and `connection_cache_size`; zodburi refuses any other key.

In a buildout, `plone.recipe.zope2instance` knows only filestorage, ZEO client and RelStorage, so an instance part points `zope-conf-template` at a template carrying the stanza above; the Plone extra ships that template, and a `zoe` option for the recipe is a later contribution upstream.

**Migration from `Data.fs`**

1. `zodbconvert` from `<filestorage>` to `<zoe>`: uses `iterator` on the source and `restore` on the target; tids, oids and pickles are copied unchanged, blobs are copied by `restoreBlob`.
2. Verify with `zodbverify` on the new store, then compare `lastTransaction()` and a random sample of `loadSerial` results between the two.
3. Swap the stanza, start the instance.
4. The reverse direction works the same way, which is the exit path if something goes wrong.

**What the instance gains without a server.** No wire, so a cache miss costs a redb lookup and an unpickle. With free-threaded Zope, commits on disjoint objects run in parallel inside one process; today a single instance on `FileStorage` serialises every commit behind one lock.

## Server mode

`zoe serve` runs a tokio server that speaks ZEO's msgpack protocol so that `ZEO.ClientStorage` connects unchanged; it replaces `runzeo`, not the client.

**Command line.** `zoe serve --path STORE --listen HOST:PORT` serves one store under the storage name `1`, the default of ZEO; the options have the twins `ZOE_PATH` and `ZOE_LISTEN`, and `--listen` defaults to `127.0.0.1:8100`. `--invalidation-queue-size` (`ZOE_INVALIDATION_QUEUE_SIZE`, 100,000 transactions by default) bounds the gap that `getInvalidations` covers, and `--client-queue-size` (`ZOE_CLIENT_QUEUE_SIZE`, 1,000 messages by default, at least 1) bounds the messages that wait for one client. A store that does not exist is created. stdout carries only the lines `admin <address>` and `listening <address>`; the server writes `listening` once it accepts connections, so a port of 0 gives a free port, and the logs go to stderr. The listening socket takes `SO_REUSEADDR`, so a restarted server binds the same port at once. The server announces `M5` and takes only `M5`. `tpc_finish` replies only after the commit is durable, so a SIGKILL at any point loses no acknowledged commit. Each connection runs its calls in order on one thread of the blocking pool, which drives its core transaction; the concurrent requests of "Connection handling" are not served yet. The extension of `tpc_begin` comes as a msgpack map: an empty map stores empty extension bytes, and another map stores its pickle in protocol 3, as `zoe.Storage` stores `extension_bytes`. The console script of the wheel has no server: `zoe serve` there fails with 1 and names the `zoe` binary. `zoe_server::serve::Server` runs the same server in the process of a Rust test, and `zoe.testing.ZoeServer` runs the binary for a Python test.

**Process shape.** The server is a single static binary with no Python inside and no Plone eggs beside it: conflict resolution runs in Rust (section 5) and the long tail is resolved by the client. `zoe serve` is the only entry point, and the same binary is the Tier 3 front.

**Named storages.** One process serves any number of stores, as ZEO does: `zoe serve --storage main=/var/zoe/main --storage catalog=/var/zoe/catalog`, or a `[storages]` table in the config, and the client's `register(storage_id, read_only)` selects one. Each store has its own path or backend, blob store, lock manager, pack schedule, read-only flag, quotas and a `storage` label on every metric and log line; commits on different stores never wait for each other. Plone mount points therefore work unchanged: one `<zeoclient>` per mount, same server. In-process mode is the same thing with one `<zoe>` stanza per mount.

**Modes.** `--read-only` serves reads and refuses `tpc_begin` with ZEO's `ReadOnlyError`; a follower being promoted serves read-only until it holds the lease, so reads continue through a failover (ZEO's `read_only_fallback` on the client side works as today). Maintenance mode (`zoe admin drain`) stops accepting new transactions, lets in-flight `tpc_finish` complete, sends every client a clean close and reports when the store is quiet, which is how a pack with a full stop, a format migration or a restore is run.

**Connection handling.** One tokio task per client; frames are length-prefixed msgpack as ZEO 5 defines them (`[message_id, async_flag, method, args]`). Requests on one connection are handled concurrently and replies carry the message id, so `prefetch` and pipelined loads return out of order. Loads run on a blocking pool (`spawn_blocking`) because the core is synchronous; the pool size is configurable and defaults to the core count. Writes from one client are serialised per transaction by the coordinator.

**Wire contract.** A frame is a 4-byte big-endian length and a payload. The server sends `M5` first, as raw bytes; the client answers `M5`, or `ruok` for a JSON status and a close. Every other payload is a msgpack array `[id, flags, method, args]`, encoded with bin types and decoded with str keys and arrays as tuples. A call that waits has an int id (or `[oid, tid]` for `loadBefore`) and flags false; an asynchronous call, in either direction, has id 0 and flags true and gets no reply. A reply echoes the id with `.reply` and flags 0, or flags 2 and `[class, __dict__ or args]` for an error. The server ignores id -1, the heartbeat of the client. An unknown method, a call before `register`, and an error in an asynchronous call close the connection. The method shapes are those of `docs/spikes/zeo-wire.md`, and the traces in `docs/spikes/zeo-wire/` are the reference for the protocol tests of `zoe serve`.

**Protocol compatibility matrix** (methods exposed by ZEO 6's `ZEOStorage`, from the source, checked against the traces of a stock server in `docs/spikes/zeo-wire/`):

| Group | Methods | Status | Notes |
| --- | --- | --- | --- |
| handshake | `register`, `get_info`, `set_client_label`, `ping`, `server_status`, `lastTransaction` | required | `register(storage_id, read_only)` selects one of the named storages; `get_info` advertises `supportsUndo` (false until phase 3) and `extensions: [lock_intent, client_resolve, trace_context]`; it gives `length` 0, as `len(zoe.Storage)`, because the store keeps no object count, `size` as the size of the files in the store directory, the store directory as `name`, `supports_record_iternext` true, and as `interfaces` the interfaces of `zoe.Storage` in `__iro__` order, so never `IStorageUndoable` or `IExternalGC`; `server_status` has the keys of ZEO 6's server in its order, with counters that all the connections of a storage share, and `conflicts_resolved` counts the conflicts that the clients resolved through the vote (a stock ZEO 6 server keeps it at 0); the store has no storage-wide commit lock, so `lock_time` is nil, `waiting` is 0 and `timeout-thread-is-alive` is `"stub"`, and `start` is in UTC; `set_client_label` is accepted and not kept; `ruok` is not a method but a protocol payload in place of `M5` (see "Wire contract"): the server answers it before any `register` with the JSON object `{storage id: server_status map}` and closes |
| reads | `loadBefore`, `loadSerial`, `getTid`, `getInvalidations`, `history`, `record_iternext` | required | `getInvalidations(tid)` replays from the transaction index of the store, so a restart keeps the answer: `[last published tid, oids]`, each oid once; `None` when more than `invalidation-queue-size` transactions that wrote records come after `tid` (default 100,000), which triggers client cache verification; `loadSerial` serves the conflicts that a client resolves; `history` gives the maps of FileStorage (`time`, `user_name`, `description`, `tid`, `size`), newest first, without the keys of the transaction extension; `record_iternext` past the last object fails with `ValueError`, as in FileStorage; `get_size_info` is not a method of a stock ZEO server, which closes the connection on it, but `zoe serve` answers it with `{length, size}`; sizes reach a stock client through the server's `info` call |
| commit | `tpc_begin`, `storea`, `vote`, `tpc_finish`, `tpc_abort`, `checkCurrentSerialInTransaction`, `deleteObject`, `new_oids` | required | `vote` returns the list of resolved and conflicting entries as ZEO 6 does: the oid of a conflict that the server resolved (tiers 1 and 2), and, for every client with no negotiation, a map `{oid, serials: [committed, read], data}` for each conflict that the client resolves, where `data` is the pickle that the client stored (tier 3); the client sends the resolved states and votes again, the locks stay held between the two votes, and the second vote checks every serial again and returns `[]` or fails; a stale read-current serial fails the vote with `ReadConflictError` |
| blobs | `storeBlobStart`, `storeBlobChunk`, `storeBlobEnd`, `storeBlobShared`, `sendBlob` | required | `storeBlobShared` is accepted only with a filesystem blob store mounted on both sides (`shared-blob-dir` on): the server checks that the path lies inside its blob directory and links the file at `tpc_finish`; refused with a clear error on an S3 blob store. Required by the ZEO 6 test matrix, whose second configuration is a shared blob directory |
| copy / backup | `restorea`, `iterator_start`, `iterator_next`, `iterator_record_start`, `iterator_record_next`, `iterator_gc` | required | needed by `zodbconvert`, `repozo`-style backup and `zodbverify` |
| maintenance | `pack` | required | online, see section 5 |
| undo | `undo`, `undoa`, `undoInfo`, `undoLog` | phase 3 | `supportsUndo` is false until phase 3, so clients never call them before; afterwards `undoa` queues the undo into the running transaction, `undoLog` and `undoInfo` read the transaction index, and `vote` runs the merge tiers for records that are no longer current |
| extension | `lock_intent(oids)`, `unlock_intent()`, `client_resolve`, `trace_context` | new | only after the client declared `extensions` in `register` |

**Invalidations.** After each `tpc_finish`, every other client of the storage gets one `invalidateTransaction(tid, [oids])`, with the oids from the transaction index; the client that committed gets `info {length, size}`, then its reply, and no invalidation, as from ZEO. A commit that wrote no oid sends `invalidateTransaction(tid, [])` to the other clients, as ZEO 6.2 does. Each client gets its own replies and the invalidations in commit order, because the cache of a stock client refuses a tid that goes back: the commits pass through one hub per storage, which orders them by the tid before each commit that `finish_after` gives. A commit waits in the hub, never on a thread, and a finish that fails publishes nothing. `register` and `lastTransaction` give the last published tid, and `register` joins the hub at that tid, so a client gets the invalidation of every later commit. Each connection has two queues: the replies of its session, and the frames of the hub, which holds at most `client-queue-size` messages (default 1,000). The session waits for room for its replies; the hub never waits: on overflow the server closes that connection, and the client's reconnect path replays through `getInvalidations`. A counter per connection places each frame when it goes into either queue, and the writer sends the frames in that order. A `loadBefore` reply shows only the commits that the hub published (the store makes a commit visible before that), and it takes its place before the invalidation of any later commit, because the stock client caches a record without a next serial as current. The replies of `register`, `lastTransaction` and `getInvalidations` come after the invalidations of the commits up to their tid; the invalidation of a later commit can come before them, and a stock client applies only the invalidations after that tid. This is the same contract ZEO keeps.

**Change feed.** The same broadcast, made public and resumable: `zoe feed --from <tid>` on the admin port (a gRPC stream, and a WebSocket for browsers) delivers one message per committed transaction with its tid, user, description, extension and the list of `(oid, class)` it touched, in commit order, exactly once from the requested tid, replayed from the transaction log for any tid still in the store. Consumers acknowledge by tid, so a restarted consumer resumes where it stopped. It is what an external indexer (OpenSearch, Solr), a cache purger (Varnish), a webhook bridge, Loupe's live tail and MCP notifications read; nothing in Plone has to be hooked for them. Class names come from `zoe-pickle` reading the class pickle, never from unpickling state.

**Blob streaming.** `sendBlob` streams the file in 64 KiB chunks straight from the blob store; `storeBlobChunk` appends to a temp file in the server's blob temp directory and `storeBlobEnd` links it to the staged transaction. S3-backed stores upload at `tpc_finish` and keep a local cache for `sendBlob`.

**Health, metrics, shutdown**

- `GET /healthz` on the admin port returns 200 when the store is open; `GET /readyz` when the last tid is loaded, the lock manager is empty of stale entries and (Tier 2) the replica is caught up.
- `GET /metrics` exposes Prometheus counters and histograms: `loads_total`, `load_latency_seconds`, `commits_total`, `vote_latency_seconds`, `finish_latency_seconds`, `fsync_batch_size`, `lock_wait_seconds`, `conflicts_resolved_total{path=native|python}`, `conflicts_raised_total`, `clients_connected`, `invalidation_queue_depth`.
- SIGTERM: stop accepting connections, let in-flight `tpc_finish` calls complete, abort transactions still in `Staged` or `Locked`, send a close to each client, exit. Bounded by `shutdown-timeout` (default 30 s).

**Traces.** Every request, lock wait, fsync and pack step is an OpenTelemetry span, exported over OTLP when `ZOE_OTLP_ENDPOINT` is set. A client that negotiated the `trace_context` extension sends its W3C traceparent with each request, so a slow Plone request and the server work it caused appear in one trace in Jaeger or Tempo; the `zoe.zeo` client sends it when `opentelemetry` is installed in the Zope process.

**Configuration** is flags or environment variables (`ZOE_PATH`, `ZOE_LISTEN`, `ZOE_ADMIN_LISTEN`, `ZOE_BACKEND`, `ZOE_BLOB_STORE`, `ZOE_LOCK_TIMEOUT`, `ZOE_EARLY_LOCKS`, `ZOE_TLS_CERT`/`_KEY`), plus a ZConfig `<zoe-server>` section for buildout users. TLS uses rustls; ZEO's `ssl` client option works against it.

**Extension negotiation.** A client declares `extensions: ["lock_intent"]` in `register`; the server echoes what it accepted in `get_info`. A stock ZEO client declares nothing and gets vote-time locking. The Plone-side piece is a `ZEO.ClientStorage` subclass (or a monkey patch shipped by `zoe.zeo`) that sends `lock_intent` from `Connection.register` hooks; section 9 has the mechanics.

## Async and free-threading

Async lives in the server and in the client's ability to overlap I/O; the persistent-object model stays synchronous because a ghost loads inside `__getattribute__`, where nothing can await. Free-threading is what makes one Zope process commit in parallel.

**What is asynchronous**

| Layer | Today | In this design |
| --- | --- | --- |
| ZEO client | asyncio loop on a helper thread, synchronous facade to ZODB | unchanged |
| ZEO server | asyncio in Python, one loop, GIL-bound | tokio, work-stealing runtime, blocking pool for storage calls, no GIL on the request path |
| Request pipelining | message ids, concurrent calls | handled concurrently; replies out of order; `prefetch` becomes a real batch |
| Storage core | — | synchronous by design; async wrappers around it, never inside |
| Object loading in Zope | synchronous on attribute access | unchanged |

**Async storage interface for new clients.** The server exposes the same methods as `async fn` in a Rust client crate (`zoe-client`) and, through PyO3, as awaitables (`await storage.aload_before(oid, tid)`, `await storage.aprefetch(oids)`, `await tm.acommit()`). This is what an async web layer such as collective.starlette uses for things that are not Plone page requests: streaming, WebSocket handlers, background jobs. A Plone request itself keeps running on a worker thread with a synchronous Connection.

**Prefetch-then-touch.** The one pattern that lets async code use persistent objects safely: `await conn.aprefetch(oids)` warms the cache, then synchronous traversal hits only the cache. The server supports it with batch loads; the ZEO client already issues `prefetch` as concurrent calls.

**Transaction managers.** Async code uses an explicit `TransactionManager` per task (`db.open(transaction_manager=tm)`), never the thread-local default. `zoe.asyncio.transaction()` is a small context manager that does this and runs commit on the pool.

**Free-threading in one process**

- The extension declares `Py_mod_gil_not_used`; on 3.14t the GIL stays off when `zoe` loads.
- ZODB's contract already is one Connection per thread, nothing shared but the DB. The core is `Send + Sync`; the lock manager, group commit, the pack state and the sequence counters are the only shared mutable state, and all are behind mutexes sized for disk latency, not instruction latency.
- Two worker threads in `tpc_finish` at once: both hold their per-OID locks, both join the same commit batch, one fsync, both acknowledged. Today `FileStorage` takes one storage-wide `_commit_lock` from `tpc_begin` to `tpc_finish`.
- `tpc_vote` runs conflict resolution on the caller's thread; with the GIL off, two threads can resolve at the same time, and native merges need no Python at all.

**Thread-safety invariants the core enforces**

1. The thread that calls `tpc_begin` drives the transaction: the core records the thread id and refuses `store`, `restore` (and `storeBlob`), `checkCurrentSerialInTransaction` and `vote` from another thread with `WrongThread`, which turns a class of silent corruption into an exception. A refused call changes nothing. `tpc_finish` and `tpc_abort` may come from any thread, as ZODB lets them.
2. Locks are acquired in oid order; a request out of order is refused, never granted.
3. A backend write transaction is opened only by the commit-batch leader, a pack batch, a sequence reservation and a restore of a sequence end (`Sequence::restore_end`). The leader and a pack batch open it under the commit sequencer; a sequence reservation and a restore of a sequence end open it outside the sequencer and write only the row of their sequence, never a record, a transaction row or a `meta` row. Readers use snapshots and never block them.
4. Every shared structure is covered by a ThreadSanitizer CI job and by `loom`-style interleaving tests on the lock manager.

**What stays on persistent and BTrees.** With `zoe` loaded but `persistent`, `BTrees` and `zodbpickle` undeclared, CPython re-enables the GIL at import and the parallel-commit benefit disappears. The first step of the roadmap is therefore upstream: `Py_mod_gil_not_used` plus a thread-safety audit in those three packages (zope.interface already did it in 8.3). The LRU ring in `cPersistence.c` changes on nearly every attribute access. On 3.14t the `persistent` fork takes the cache's critical section there too, because a sweep from another thread (`DB.cacheMinimize()`, `cacheGC()`) otherwise corrupts the ring; this costs about 6 ns per attribute access on 3.14t and nothing on the GIL builds. The one-Connection-per-thread rule still holds for the objects: a sweep from another thread of a Connection in use stays unsupported, because the thread that uses it can get spurious `AttributeError`s or read an instance dict that is being freed. With the stock `persistent` on 3.14t, a multi-thread stress of the pattern of ZODB's `test_race_loadopen_vs_local_invalidate` fails on every storage that was measured: on `zoe.Storage` and on `FileStorage` with a `SystemError` in `ZODB.serialize` or a spurious `AttributeError`, and on `MappingStorage` with a crash of the interpreter. The test itself passes most runs. ZOE's tests skip it on free-threaded builds until the fork is in use. A Rust port of `persistent` and `BTrees` is a separate project; it would share the native merge code from section 5.

## ConflictError strategy

ConflictError is optimistic concurrency at pickle granularity: two transactions write the same object and the second to vote loses. A storage cannot abolish that; it can resolve more conflicts, shrink the window, or serialise writers on the hot object so they wait instead of failing. The design does all three, in that order of cost.

**Step 0: measure before building.** Zope logs the oid and class on every ConflictError it retries or raises. Before the native-merge list is final, rank one week of production logs from a real site by class and by oid; the top ten classes decide which merges go native first. Expected suspects in Plone: catalog index buckets (`IOBTree`, `OOBTree`, `IITreeSet`), `BTrees.Length.Length` in indexes and BTreeFolder2, the ZCTextIndex lexicon (`_nextwid`), and annotations on shared objects such as the site root and the registry.

**Lever 1: resolve in Rust, never in Python on the server.** Today a conflict on a bucket costs three unpickles and one pickle in Python on the ZEO server, per object, per retry, and the server must carry every Plone egg to do it. Section 5 replaces that with three tiers: structural merges for buckets, sets and `Length`; field-level three-way merges for any object whose merge policy allows it, so two editors changing different fields of the same item no longer conflict; and, for classes with their own `_p_resolveConflict`, resolution on the client, where the class already lives. The policy is derived from Dexterity schemas by `zoe.plone`, strict by default, and loosened per class only after shadow mode has shown on production conflicts that the merge would have been right.

**Lever 2: shrink the window.** Faster vote and finish (group commit, no Python on the server path) shorten the interval during which a second writer can read a serial that is about to be stale. This reduces the probability of conflict, not its possibility.

**Lever 3: early write-intent locks.** This is the one mechanism that avoids conflicts. The client asks for the exclusive lock on an object when the transaction first marks it changed, not at vote. A second writer blocks on the lock, then re-reads the object after the first commit and writes a non-conflicting update. The wasted work of three retries becomes one wait.

Mechanics on the Plone side: ZODB's `Connection.register(obj)` is called when `_p_changed` is set. A `zoe.zodb.LockingConnection` (or a `DB` with `klass=` set to it through the stanza `early-locks on`) overrides `register` to call `storage.lock_intent([oid])` for objects that already have an oid. New objects need no lock. The call is batched per transaction and sent once per 50 ms or at vote, whichever comes first, so a transaction that touches 2,000 objects does not make 2,000 round trips.

Costs and controls:

- Long transactions hold locks longer: `lock-timeout` (default 10 s) turns a wait into a `ConflictError`, which Zope retries as today.
- Deadlocks are prevented by oid ordering; a request out of order is refused and the client re-requests the full set in order.
- Read consistency: `checkCurrentSerialInTransaction` (BTrees' `readCurrent`) takes a shared lock under the same rules, so a reader cannot be invalidated by a writer it did not wait for.
- Under a free-threaded Zope with many worker threads, a waiting thread costs nothing; under a four-thread instance a hot object can stall all four, so the default stays `vote` locking until the instance runs free-threaded.

**Lever 4: structural, in Plone.** Counters as `Length`, queued indexing (Plone's `CMFCore` indexing queue already defers catalog writes to the end of the transaction), shorter transactions in batch jobs, splitting hot BTrees by key range. These stay application work; the metrics from section 7 tell you where.

**What remains.** Two transactions writing the same non-mergeable object with early locks off still conflict, and that is correct: the alternative is lost updates. The target in the success criteria (an order of magnitude fewer ConflictErrors reaching Zope on a production replay) comes from levers 1 and 3 together; the harness in section 11 measures each separately.

## Catalog acceleration and search

The Plone catalog is where write amplification, ConflictErrors, cold starts and memory meet, so ZOE treats it as a first-class target in three steps: the storage-level relief already specified (native merges, early locks, a shared hot cache); a read-side accelerator, `zoe-catalog`, that mirrors ZCatalog's indexes in memory and answers queries in Rust behind the unchanged ZCatalog API; and a write-side projection that replaces bucket writes with commutative deltas. A native BM25 text index on tantivy ships with the accelerator as the first ZOE-native index.

&#91;embedded content: catalog query path · mirror or Python, same brains\]

The fallback is the correctness guarantee: anything the mirror does not cover takes the path Plone has always taken.

**Where the catalog hurts today**

| Path | What happens | Cost |
| --- | --- | --- |
| write | one content change reindexes 20 to 40 indexes: dozens of bucket pickles and `Length` counters per transaction | most of the pickle volume and most of the ConflictErrors in a Plone site |
| read | each query loads bucket pickles into the process cache, intersects `IISet`s in C, and sorting on `modified` loads the sort index's `_unindex` for every hit | slow until warm, minutes of crawl after every restart |
| memory | every Zope process holds its own copy of the same buckets | N processes, N catalogs |
| text | `ZCTextIndex` scores with Okapi BM25 in Python over word-id buckets, with a splitter that knows no language, no stemming, no accent folding, no snippets | sites bolt on Solr or Elasticsearch and inherit a second system and its consistency lag |

**Step 2: the mirror, `zoe-catalog`**

- Scope: `FieldIndex`, `KeywordIndex`, `DateIndex`, `DateRangeIndex`, `BooleanIndex`, `UUIDIndex`, `ExtendedPathIndex`, `GopipIndex` (sort), plus the `data` metadata table, `uids` and `paths`. `ZCTextIndex` is not mirrored; the native text index below replaces it.
- Representation: per index a map from key to a roaring bitmap of rids and the reverse map from rid to keys; sorted rid arrays for sort indexes; a trie for the path index; metadata rows in a columnar table so brains need no bucket at all.
- Discovery and build: catalog roots are found by class (`Products.ZCatalog.Catalog.Catalog`) at startup; each index's BTree roots are traversed once in a background job (readiness metric `zoe_catalog_mirror_ready`), then maintained incrementally: at `tpc_finish`, before invalidations go out, every committed bucket of a mirrored index is decoded by `zoe-pickle` and applied.
- MVCC: the mirror is versioned per tid with structurally shared persistent maps and copy-on-write bitmaps, kept for a window (default 10,000 tids or 10 minutes). A query carries the client's snapshot tid and is answered as of that tid, so a result never names an object the Connection cannot load. Outside the window the query falls back to the Python path.
- Protocol extension `catalog_query(catalog_oid, tid, plan)`: the plan is ZCatalog's query normalised (index, operator, values or range, `sort_on`, `sort_order`, `sort_limit`, `b_start`, `b_size`, requested metadata). The reply is the ordered rids, the metadata rows and the total count. In-process mode calls the same function through PyO3 with no wire.
- Plone side, `zoe.plone.catalog`: a hook on `Catalog.search`, the way `collective.solr` hooks in, routes a query to the mirror when every index and operator in it is supported and builds a `LazyMap` of brains from the returned rows; otherwise the whole query takes the Python path. Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other and arrives in the plan as today.
- Correctness is a differential test, not a claim: every query of the Plone test suites and a recorded production query log run through both paths, and rids, order and counts must be identical, with ties broken by rid as ZCatalog does.
- Memory: roaring bitmaps and shared structure put a one-million-object catalog with thirty indexes in the low hundreds of megabytes, once for the whole cluster instead of once per process.

**Native BM25 text index.** Plone already scores with BM25; what it lacks is everything around the formula, so the native index is tantivy with BM25 (`k1`, `b` tunable), Snowball stemmers for the site's languages, ASCII folding, stop words, phrase, prefix and fuzzy queries, per-field boosts (`Title^3`, `Description^1.5`, `SearchableText^1`) and snippet generation. One tantivy index per catalog, a field per text index, and a field per language when `plone.app.multilingual` is installed, chosen from the object's `Language`.

- Why it is the first native index: text-index buckets hold word ids, not text, so a BM25 index cannot be rebuilt from them. `zoe.plone` therefore provides `ZoeTextIndex`, a ZCatalog index class whose `index_object` sends `(rid, index, text)` to the core as a delta record inside the same transaction and whose `_apply_index` queries tantivy. This pilots the delta mechanism of step 3 on the one index with the largest quality gain.
- Consistency: tantivy commits in batches a few hundred milliseconds behind the tid; hits are filtered by the mirror's bitmap of rids existing at the query's snapshot tid; snapshots older than the window fall back to the Python index.
- Durability: the deltas are records in the ZOE transaction, so the tantivy directory beside the store is a cache that `zoe catalog rebuild-text` recreates without Plone.
- Compatibility: a translator maps ZCTextIndex query syntax (`AND`, `OR`, `NOT`, quoted phrases, trailing `*`) to tantivy's; `sort_on='relevance'` keeps meaning BM25; a compatibility switch reproduces the old ranking for sites whose tests pin it. Enhanced mode is the default; fuzzy matching, field boosts and snippets reach new interfaces through the `@search` restapi extension.

**Step 3: the catalog as a projection (phase 9, design only here).** Plone keeps computing index values in Python at commit, but instead of writing buckets it emits deltas, `(catalog, rid, index, old value, new value)`, as a record type in the same ZOE transaction. The core applies them to the mirror and appends them to a persisted delta log per catalog with periodic snapshots. Deltas on different rids commute, so two editors indexing different documents never conflict on a shared bucket again; the only true conflict left is the same rid and index in two concurrent transactions, which is raised as today. Catalog BTrees and `Length` counters leave the store, which removes most of the write amplification. ZCatalog objects stay as thin proxy index classes, so `plone.app.querystring`, collections and navigation never notice; migration is one rebuild. ACID holds because the deltas ride the transaction that changed the content, and in-process mode works the same way.

**Metrics.** `zoe_catalog_queries_total{path=native|python}`, `zoe_catalog_query_latency_seconds`, `zoe_catalog_mirror_lag_tids`, `zoe_catalog_mirror_build_seconds`, `zoe_text_commit_lag_seconds`, `zoe_catalog_fallbacks_total{reason}`.

**Testing, on top of section 11.** The differential suite above; `zoe-bench catalog` with a query mix replayed from production logs at concurrency, warm and cold; text relevance checked against a labelled set of queries from a real site, in that site's language. Acceptance: identical results on every differential run; a five-index intersection with a sort on a one-million-object catalog under 5 ms at p99 warm; mirror ready within 60 s of a cold start on that catalog; a text query under 20 ms at p99.

**Roadmap placement.** Phase 5: the mirror, the native text index and the `zoe.plone.catalog` hook. Operations and data tools are phase 6, AI-native retrieval phase 7; Tier 3 becomes phase 8. The projection is phase 9, started only after the mirror has answered a long enough run of production queries identically.

## AI-native: MCP server and retrieval

ZOE exposes its store to agents through an MCP server and gives Plone content native retrieval, built on pieces the catalog work already provides: the delta pipeline carries the text, the mirror holds every object's `allowedRolesAndUsers` as a bitmap, and `zoe-pickle` turns any object into JSON. The vector backend is a choice: ZOE's own index stored in the DB for simple deployments, or Qdrant for sites that want a dedicated vector service.

**MCP server, `zoe mcp`.** A Rust MCP server on the official `rmcp` SDK, running inside `zoe serve` or standalone against a server, with stdio for local agents (Claude Code on an ops laptop) and streamable HTTP for remote ones. Crate `zoe-mcp`.

| Family | Tools and resources | Ships |
| --- | --- | --- |
| ops and dev | `object_get(path or oid, as_of)` as decoded JSON, `history`, `diff(oid, tid_a, tid_b)`, `references(oid)`, `tail` as a resource with notifications, `space`, `verify`, `explain_conflict(tid)`; `pack` and `backup` behind the read-write role | phase 2, read-only; they are Loupe's functions with an agent face, and the builder agents use them on the store they are building |
| content | `search(query, mode, filters, as_of, k)` returning passages with path, title, modified, portal type and a citation; `get_document(path)`; `list_children(path)`; `similar(path)` | phase 7 |
| prompts | "explain this transaction", "why did this object conflict", "summarise what changed since tid" | phase 2 |

Security: the MCP server runs as a principal, and every content result is filtered by intersecting the caller's roles with the `allowedRolesAndUsers` bitmap before ranking; objects outside the catalog are visible to the ops role only. Transport security is the server's TLS with read-only or read-write certificates, as for Loupe; a Plone-authenticated gateway (`@mcp` in the restapi, so site users' agents inherit Plone's login and permissions) is a later addition on top, not a replacement.

**Retrieval, `zoe-vectors`**

1. Chunking: the `(rid, index, text)` delta that feeds tantivy is split into passages with offsets; passages are stored in the core, versioned by tid, so retrieval returns text and not pointers. One default policy (passage size, overlap, which text indexes) with overrides per `portal_type`, because minutes and decisions chunk differently from news items.
2. Embedding: asynchronous, like tantivy commits, with `zoe_vectors_lag_tids` as the metric and the as-of-tid bitmap filter so a deleted page never returns. Providers are pluggable: a local multilingual model through ONNX in the server process is the default, because a site's content should not leave the site owner's server by default; remote providers (Mistral, Voyage, OpenAI) are opt-in per site and logged as such.
3. Vector backend behind one trait, `VectorIndex` (`upsert`, `delete`, `search(vector, k, prefilter)`, `rebuild`), chosen in the stanza:

| Backend | What it is | When |
| --- | --- | --- |
| `builtin` | vectors stored in the ZOE store itself, in a `vectors` key space keyed `(catalog, rid, chunk, tid)`; brute-force SIMD search under a configurable size, an HNSW graph above it, the graph cached beside the store and rebuilt from the vectors when missing | the KISS deployment: no extra service, vectors in the backup, and `search(as_of=tid)` answers what the index knew at that tid because the vectors are versioned like everything else |
| `qdrant` | a Qdrant collection per catalog over gRPC; payload carries rid, portal type and roles for pre-filtering; the as-of bitmap is applied after an over-fetch | sites that already run Qdrant or want vector search scaled separately |
| others | the trait is small enough for pgvector or Milvus later | not planned |

4. Hybrid search: BM25 from tantivy and vectors from the backend fused by reciprocal rank, behind the same `catalog_query` extension, the `@search` restapi extension (`mode=bm25|vector|hybrid`, `return=passages`) and the MCP `search` tool. Security filtering happens before ranking in every mode.
5. Rebuild: `zoe catalog rebuild-vectors` re-embeds from the stored passages with no Plone involved; switching backend is a rebuild.

**Metrics.** `zoe_mcp_requests_total{tool}`, `zoe_mcp_denied_total{reason}`, `zoe_vectors_lag_tids`, `zoe_embed_latency_seconds{provider}`, `zoe_vector_search_latency_seconds{backend}`, `zoe_vectors_remote_bytes_total` (what left the server, by provider).

**Testing, on top of section 11.** MCP conformance with the reference client; a security suite that asserts an anonymous principal never receives a restricted passage across every mode and backend; backend equivalence (`builtin` and `qdrant` return the same top-k on a fixed corpus within a tolerance); retrieval quality on the same labelled query set against BM25 alone; lag and as-of correctness under a write load.

**Roadmap placement.** MCP ops tools and prompts in phase 2, a thin layer. Content tools, chunking, embeddings, both vector backends and hybrid search are phase 7, AI-native; Scale-out becomes phase 8 and the catalog projection phase 9.

## Deployment

The same wheel and binary cover a laptop, a single server and a Kubernetes cluster; what changes is who opens the store directory.

**Simple.** One Zope instance with `<zoe>` in `zope.conf`. No server, no extra process, one directory to back up. With free-threaded Zope this instance commits in parallel on its own.

**Clustered, bare metal or VMs.** `zoe serve` on the storage host, instances with `<zeoclient>` as today. Supervisor or systemd unit shipped in the package; the server is a single static binary, no Python and no Plone eggs on the storage host.

**Kubernetes**

| Tier | Shape | What you get | What you give up |
| --- | --- | --- | --- |
| 0 | one Plone pod, in-process storage, PVC, `Recreate` strategy | simplest possible site, no server | no horizontal scaling of Plone |
| 1 | StatefulSet ×1 for the server, one PVC (redb + blobs), Plone as a stateless Deployment | horizontal Plone scaling, seconds of downtime on a reschedule, full observability | a single point of failure for writes |
| 2 | leader + follower StatefulSet ×2, two PVCs, log shipping, leader election through the Lease API, a Service that follows the lease | automatic failover in seconds, read-only clients served by the follower at a lagged tid | more disk, a replication lag to watch |
| 3 | server Deployment ×N, FoundationDB or TiKV cluster as backend, S3 for blobs | stateless fronts, no PVC on the server, durability and failover owned by the KV cluster | an extra network hop per operation; the FDB transaction limits |

**Tier 1 manifest essentials**

- `StatefulSet` with `replicas: 1`, `podManagementPolicy: OrderedReady`, a `volumeClaimTemplate` on an SSD storage class; `fsync` latency of the class decides write throughput, so benchmark the class before choosing it.
- `readinessProbe` on `/readyz`, `livenessProbe` on `/healthz`, `terminationGracePeriodSeconds` above `shutdown-timeout`.
- A `Service` on the ZEO port and a second on the admin port; a `ServiceMonitor` for Prometheus.
- `PodDisruptionBudget` with `maxUnavailable: 0` so a node drain waits for a manual window until Tier 2 exists.
- Backups: a `CronJob` running `zoe backup --to s3://…` (incremental, by tid range, through the iterator) and `zoe verify`.

**Tier 2 mechanics.** The follower streams transactions from the leader's log (`iterator_start` from its own last tid, kept open) and applies them with `restore`; it is read-only to clients. Leader election: both pods renew a `coordination.k8s.io/Lease`; the holder serves writes. On lease loss the old leader closes client connections; clients reconnect through the Service, which selects by the `zoe/role=leader` label that a sidecar sets from the lease. Promotion requires the follower to be at the leader's last acknowledged tid; a leader acknowledges a commit only after the follower confirmed it when `replication acks=1` is set, which also replaces fsync on slow cloud disks.

**Backup and point-in-time recovery (continuous shipping is post-roadmap; dumps are the phase 2 answer).** The log shipping that feeds a follower feeds a bucket just as well: `backup.target = s3://bucket/zoe/main` makes the leader ship every committed transaction and every new blob to object storage as it commits, with a full snapshot on a schedule (`backup.snapshot = daily`) so a restore reads one snapshot plus the log after it. `zoe restore --from s3://... --to-tid <tid>` (or `--to-time`) rebuilds a store at any transaction still in the bucket, and `zoe backup verify` restores the latest snapshot plus log into a scratch directory and checks it against the live store's checksums; the Tier 1 manifest runs it as a CronJob and its result is a metric. The recovery point is the last shipped transaction, seconds behind the leader; repozo-style dumps through the iterator stay available for sites without a bucket. Encryption keys are never in the bucket: a restore needs the wrapping key from the file, environment or KMS it came from, which is the one secret an operator must keep outside the cluster.

**Tier 3 mechanics.** The backend trait maps onto an ordered KV: records under `(oid, tid)` keys, the tid sequencer under one key updated inside the same transaction, invalidations read from a tid-ordered change log that every front tails. Any front can take any client; clients see one Service. Blobs go to S3 with a per-front LRU cache. The FDB limits (5 s, 10 MB per transaction) are met by staging locally and committing in one short write; a commit larger than 10 MB is split by oid range inside one logical tid using FDB's atomic versionstamp ordering.

**Configuration and observability, all tiers.** Flags and `ZOE_*` variables, a `ConfigMap` for the server stanza, secrets for TLS and S3. Metrics as listed in section 7, with a Grafana dashboard shipped in the repository: commit latency, fsync batch size, lock wait, conflicts resolved by path, invalidation queue depth, replication lag (Tier 2), backend latency (Tier 3).

## Loupe: desktop inspector and editor

Loupe is a Tauri 2 desktop application for ops people and developers that opens a ZOE store or connects to a ZOE server, browses, edits, creates and deletes objects without a Python interpreter, travels in time, tails live commits and runs operations. It never opens a `Data.fs`; it converts one. Crate `zoe-loupe`, product name Loupe.

&#91;embedded content: Loupe · folder mode, server mode, reverse index and reindex hook\]

The tinted box is the app; everything it touches already exists in the design, except the reverse index it keeps for itself and the queue it leaves for Plone.

**Two ways in, same screens.** A store folder (exclusive, instance stopped, `zoe-core` in-process) or a running server (`zoe-client` over the ZEO protocol with TLS, read and write through the same two-phase commit as any client, so running Plone instances see Loupe's writes through invalidations). Because the wire protocol is ZEO's, Loupe also browses a classic ZEO server, read-only, which is how a site gets inspected before it is converted.

**Pickle layer, `zoe-pickle`.** A ZODB record is two pickles, class reference and state, written with one memo. The crate decodes both into a generic tree (class path, attributes, persistent references, blob references, primitives, containers). It reads the opcodes of protocols 0 to 3, so the records that Python 2 stores wrote in protocols 1 and 2 decode too. Each node keeps the opcode form that wrote it, and the encoder writes that form back: it never picks another one, and new values use protocol 3 with ZODB's `persistent_id` convention. Constructs it does not model (custom `__reduce__` callables, extension codes, Python 2 instances) become opaque nodes that re-emit their original bytes; editing inside one is refused rather than guessed. Fidelity is a tested property: decode then encode must reproduce every record of the conformance corpus and of a production copy byte for byte.

The decoder refuses what Python's unpickler refuses as malformed, for example an unknown protocol or a BINSTRING or LONG4 length of 2³¹ or more. It is stricter than Python in a few places: it refuses the opcodes of protocols 4 and 5, PROTO after the start of a pickle, values or a mark left on the stack at STOP, bytes after the state pickle, and text arguments that no pickler writes (for example a memo index with leading zeros, or an INT in octal or hexadecimal). No operation on the tree recurses once per nesting level, so a deep value cannot overflow the stack. Each record has a node budget, a parameter of each call: online readers such as Loupe use the default of 2²⁰ nodes, and pack and the converters pass the length of the record, which refuses nothing the input can hold. For pack and the converters a refusal is a hard error, never a record without references.

**Screens**

| Screen | For | What it shows and does |
| --- | --- | --- |
| Object browser | developers | root and reference traversal; Plone-aware paths through `BTreeFolder2._tree`, `__annotations__` and the catalog, so `/plone/meetings/2026` resolves; per object: class, oid, serial, size, decoded state, inbound and outbound references |
| Time travel | both | history of any object (tid, user, description), diff of decoded state between two versions, a global "as of tid" switch backed by `loadBefore` |
| Live tail | ops | when connected to a server, the invalidation feed as a scrolling commit log: user, description, oids, size, lock waits, conflicts with the class that conflicted; filters and pause |
| Space | ops | size per path and per class, blob sizes, growth from history, largest objects, what a pack would reclaim; treemap and timeline; replaces the `db-size` and `fields-usage-stats` scripts and their Streamlit dashboard |
| Integrity | ops | every record decodes, dangling references, orphaned blobs, classes with no module in the site's class list; background job with progress, exportable report |
| Operations | ops | pack, backup, verify with confirmation; connected clients; the lock table; replication lag in Tier 2 |
| Editor | developers | full CRUD on objects and attributes, below |

**Full CRUD, and what each operation really does**

| Operation | Mechanics | Guard |
| --- | --- | --- |
| Create | allocate an oid with `new_oids`; class by dotted name, picked from the classes seen in the store or typed; initial state built in a typed form; linked from a container (raw attribute, or a Plone-aware insert into `_tree`, `_count` and `_mt_index`) | an unlinked object is reported as garbage-to-be |
| Read | decoded state and both directions of references; inbound references come from a reverse-reference index Loupe builds on first open as a background job, stores beside the store as `loupe-index`, and keeps current from the live tail | — |
| Update | typed editors for primitives, dates, bytes, lists, dicts, tuples, references and blobs (replace the file); a pickle-level diff is shown before commit | opaque nodes are not editable |
| Delete | `deleteObject` after the reverse index confirms no inbound references, or unlink only; optional cascade to children that become unreferenced (pack removes them anyway) | refused while inbound references exist unless cascade is chosen |
| Move and copy | reference changes in the containers involved plus the object's `id`; Plone-aware moves update both containers' `_tree`, `_count` and `_mt_index` | — |
| Revert | restore an object to the state at an earlier tid, as a new transaction | — |

Every Loupe write is one ZODB transaction: user `loupe:<operating-system user>`, a description the operator must type, and extension data `{"tool": "loupe", "version": …}` so history and the live tail show which changes came from Loupe. It passes vote like any transaction, so it can lose a conflict and retry.

Undo rides on the same path once phase 3 lands: the time-travel view offers "undo this transaction", which calls the storage's `undo`, shows per object whether the record is current or goes through a merge, and commits with a `loupe:undo` note like any other edit.

**Application invariants.** Raw edits bypass Plone: the catalog, `ObjectModifiedEvent` handlers, workflow, `modified` dates, the UUID index. Loupe is a scalpel, not a content editor, and the design says so in four ways:

1. Writes need an explicit expert mode per session.
2. Objects that are catalogued carry a badge, and the commit dialog lists which indexes will be stale.
3. Later, not in V1: an optional Plone integration, `zoe.plone`, shipped as the zoedb\[plone\] extra of the wheel, provides a reindex queue: Loupe appends the touched oids to a persistent `IOTreeSet` at a known path in the same transaction; the add-on drains the queue on the next request (or a clock tick), reindexes those objects and optionally fires `ObjectModifiedEvent`. With it installed, a Loupe edit is reindexed within seconds; without it, Loupe lists the touched oids and the operator reindexes by hand, which is the V1 answer.
4. Every commit is preceded by a dry run that shows the pickle diff and the list of transactions it would conflict with if any client has them in flight.

**Converters, `zoe convert` and a Loupe screen.** The goal is to replace `Data.fs`, so conversion is first-class and runs in Rust without a Python environment:

| Direction | Reader or writer | Notes |
| --- | --- | --- |
| `Data.fs` → ZOE | streaming `FileStorage` reader: transaction records, data records, back pointers, `BushyLayout` and `LawnLayout` blob directories | the migration path for every existing site; resumable by tid |
| RelStorage → ZOE | PostgreSQL via `sqlx`, history-preserving and history-free schemas, blobs from the blob directory or from `blob_chunk` | the second migration path |
| ZOE → `Data.fs` | `FileStorage` writer | the exit path, so nobody is locked in |
| ZOE → ZOE | iterator to a different backend | backend change, Tier 1 to Tier 3 |

Every conversion verifies transaction counts, last tid and a sample of records byte for byte, and writes a report. The Python route, `zodbconvert` with the `zoe` storage, stays available for anyone who prefers it.

**`zoe verify`.** `zoe verify --path STORE` checks a store without Python and opens it read-only, so it changes no byte. It walks every record key: each record frame, deletions too, and each blob record passes its checksum, and each blob file has the size and the BLAKE3 hash of its blob record. It reads each transaction row, one at a time, and each sequence row (`Backend::sequence_names` lists the names); a sequence name that is not UTF-8 is one finding in `checksums`, and then no sequence row is read. Of each oid, the newest record is the current record: its class comes from the class pickle, and its persistent references from `zoe-pickle`'s `persistent_ids` with the node budget of the record length, never from an unpickler. A reference to an oid without a current record is dangling, as `referencesf` would find it. `--classes FILE` (`ZOE_CLASSES`) names the classes that the application provides, one `module.name` per line; each other class of the store is a finding, as the classes that `zodbverify` cannot import. A record that does not decode is a finding, not a crash. `--format json` (`ZOE_FORMAT`) prints one JSON document with oids and tids as integers, and these keys: `checksums`, the frames that do not read, each `{oid, tid, error}`, where a transaction row has `null` as its oid and a sequence row has `null` as its oid and its tid and names the sequence in `error`; `blobs`, the blob files that do not match their blob records, each `{oid, tid, error}`; `references`, the dangling references, each `{oid, tid, missing}`; `pickles`, the current records whose references zoe-pickle cannot read, or that hold a reference whose oid is not 8 bytes, each `{oid, tid, error}`; `classes`, the class of each current record with its count, `{"module.name": count}`; `unknown_classes`, the classes that `--classes` does not name. The command exits with 0 when it finds nothing, 1 when it finds something, and 2 on a usage error. The walk keeps 8 bytes for each oid with a current record, and about 24 bytes for each reference to a later oid until it ends; ZODB gives a new object its oid when it pickles the parent, so nearly every reference in a store that ZODB builds points to a later oid. The library is `zoe_convert::verify`. Loupe and `zoe mcp` do not call it yet: neither depends on `zoe-convert`. A blob record without its record is not checked yet: no read of the store lists the blob records.

**`zoe backup` and `zoe restore`: dumps (N15).** `zoe backup --path STORE --to DIR` (`ZOE_PATH`, `ZOE_TO`) opens the store read-only and writes one dump file into the backup directory `DIR`: the full dump the first time, then a dump of the transactions after the last tid of the dumps, read through the iterator. A dump holds each transaction with its tid, user, description, extension and records, deletions included, the blob of each blob record with its size and BLAKE3 hash, and the end of each sequence, read after the last tid of the dump. It ends with the BLAKE3 hash of its bytes. ADR 0030 gives the layout. The backup writes the file under a temporary name, syncs it and renames it, so it never changes a dump that `DIR` holds. When the dumps end at the last tid of the store, it writes nothing and exits with 0. It takes a `flock` on `DIR`, so two backups never write into it together. Dumps whose last tid is not a tid of the store fail it, unless a pack removed that transaction. `zoe restore --from DIR --path STORE` (`ZOE_FROM`) refuses a target that holds a store or other files, checks every dump of the chain before it writes anything, and rebuilds the store in the work directory `STORE.restore`: each transaction under its own tid, each blob after a check of its hash, and each sequence raised to its end in the dumps with `Sequence::restore_end`, so no value comes twice. Then it renames the work directory to `STORE`. A damaged or missing dump fails the restore with 1, and a failed restore leaves nothing at the target. `zoe verify --path STORE --backup DIR` (`ZOE_BACKUP`) restores the dumps into a scratch directory in `TMPDIR`, compares them with the store up to the last tid of the dumps, and removes the scratch directory. Each transaction, record and blob record must be the same, but at or before the pack watermark of the store, a record or a transaction that a pack removed is no difference. Each sequence of the store must end at or above its end in the dumps. A difference is a finding, and the command exits with 1; so does a damaged dump. With `--format json`, the report is one JSON document with the keys `dumps`, `last_tid`, `transactions`, `records`, `blobs`, `sequences` and `differences`. A restore commits each transaction with a sync, and reads each dump twice: once to check it, once to restore it. The dumps keep the history that a later pack removes; a new full dump takes a new directory. The library is `zoe_convert::backup`, `restore` and `verify_backup`.

**Operations tools in Loupe.** Each operations feature of the core and the server has a screen: open a fork of the current store at any tid and work in it; the last scrub, its findings and a button to run one; a pack dry-run as a treemap of what would go, the retention window with what is still revertible; `verify` findings with a jump to each broken reference; `rename-class` with a preview of affected objects; the change feed as the live tail, with filters by class and user; a Parquet export dialog that picks classes and attributes; the backup state (last shipped tid, last verified snapshot) and a point-in-time restore into a fork once continuous backup ships after the roadmap, which is the safe way to look at what the store held at a given moment.

**Stack.** Tauri 2; the Rust side links `zoe-core`, `zoe-client`, `zoe-pickle` and a `zoe-convert` crate, exposed to the window as IPC commands; frontend React and TypeScript with Vite, since that is what the Plone frontend world already writes; d3 for the treemap and timelines, a virtualised tree for the browser; UI strings in a translation catalog from the first release, English first. Packages for Linux first, macOS and Windows in a later phase, with Tauri's updater and code signing.

**Security.** Loupe carries production credentials, so the server distinguishes read-only from read-write client certificates and Loupe shows which it holds; read-only mode hides the editor and operations. Credentials live in the operating-system keychain through Tauri's store plugin, never in a config file.

**Testing, on top of section 11.** Pickle round-trip property tests over the corpus; CRUD invariants against a Plone 6 site (edit in Loupe, assert Plone shows the change after invalidation and the `zoe.plone` reindex); reverse-index correctness against a brute-force scan; converter equivalence (`Data.fs` → ZOE → `Data.fs` byte-identical pickles and tids); end-to-end Tauri tests with `tauri-driver`.

**Roadmap placement.** Loupe v0 (folder mode, browser, time travel) and the `Data.fs` converter land in phase 1, because an inspector is the debugger for the storage itself; live tail, space and operations in phase 2; full CRUD and `the optional zoe.plone integration` in phase 3; the RelStorage converter in phase 4.

## Performance and testing harness

Speed and durability are the product, so the harness is part of the first milestone, not the last; every requirement in section 3 maps to one job below, and the numbers go into a tracked dashboard from the first commit that compiles.

**A. Conformance (correctness against ZODB's own definition)**

| Suite | What it proves | Runs against |
| --- | --- | --- |
| `ZODB.tests`: `BasicStorage`, `MTStorage`, `Synchronization`, `ConflictResolution`, `PackableStorage`, `RevisionStorage`, `HistoryStorage`, `IteratorStorage`, `PersistentStorage`, `ReadOnlyStorage`, blob tests | `IStorage` semantics, MVCC, pack, iteration, blobs | in-process and server |
| full `ZODB` test suite through a `zoe.testing` layer | Connection, DB pool, invalidations, `MVCCAdapter` behaviour | in-process |
| `ZEO.tests` with the forker launching `zoe serve` | protocol, reconnect, cache verification, invalidation delivery, blob transfer, `getInvalidations` replay | server |
| differential conflict tests: property-generated bucket, set and `Length` conflicts resolved natively and by `ZODB.ConflictResolution`, pickles compared byte for byte | native merges equal Python merges | core |
| Plone 6 suites (`Products.CMFPlone`, `plone.app.contenttypes`, `plone.restapi`, `plone.app.upgrade` on a copied production store) through a `plone.testing` layer | drop-in claim | both modes |
| `zodbconvert` round trip `Data.fs` → ZOE → `Data.fs` on a production copy, pickles and blobs compared | migration and tooling | in-process |

Corpora: public CI runs every suite above on a synthetic Plone 6 site generated by `zoe-bench` (configurable size, content types, catalog indexes, blobs). The production copies named in this document come from an adopting site and run only on that site's private runner; their results reach the dashboard as numbers and the data never leaves the site.

**B. Performance**

1. `zodbshootout` (RelStorage's harness) is the public scoreboard. Scenarios: add, update, read cold, warm and hot, object sizes 1 KB and 100 KB, concurrency 1, 4, 16, 64 with threads and with processes, against `FileStorage`, ZEO 6, RelStorage on PostgreSQL 17, ZOE in-process and ZOE server, on one documented reference machine (CPU, NVMe, kernel, Python build, free-threaded or not). Results as p50, p99 and operations per second.
2. `zoe-bench`, a Rust load generator that speaks the ZEO protocol: N clients, pipelining depth, read/write mix, Zipf-skewed hot set, transaction size. It measures the two things `zodbshootout` cannot: contention (ConflictErrors per 1,000 commits, lock wait, retries) with vote-time versus early locks, and server saturation (loads per second at a p99 ceiling).
3. `criterion` microbenchmarks in the core: `load_before`, `vote`, `finish`, group-commit batching, lock manager acquire/release, native merge per bucket size, msgpack frame decode.
4. Fsync-class benchmark: the same write workload on NVMe, on a cloud PD or EBS class, and with Tier 2 replication acknowledgement instead of fsync; this is the number that sizes Kubernetes storage classes.
5. Plone end-to-end: a copied production site behind k6 or locust with a write-heavy script (edit, workflow transition, bulk upload, catalog reindex) and a read-heavy script; requests per second, p99, ConflictErrors per 1,000 requests, ZEO versus ZOE, same Plone, same hardware.
6. Transaction-log replay: replay N days of production transactions from the iterator at 1×, 4× and 16× speed with their original concurrency grouping, over a read load, and count conflicts raised, resolved natively and resolved in Python. This is the measurement behind the success criterion on ConflictErrors.

**C. Durability and crash consistency**

1. Simulated filesystem in the core tests: a `Vfs` trait with an in-memory implementation that records every write and fsync, then replays prefixes, reorders unsynced writes and truncates the tail; after every replay the store must open, `verify` must pass, and every acknowledged tid must load. Deterministic, thousands of cases per minute, runs on every PR.
2. Process kill loop: writers against a real store, `SIGKILL` at random points including inside `tpc_finish`, restart, verify, repeat 10,000 times nightly.
3. Power-loss emulation: the same loop on a `dm-flakey` device or a QEMU guest that is force-stopped, so lost page-cache writes are real, weekly.
4. Blob atomicity: kill between blob rename and record commit, verify neither a dangling blob nor a record without its blob.
5. Pack under load: pack during the write benchmark; readers must see consistent snapshots, no `POSKeyError`, and the store size must drop.

**D. Concurrency and free-threading**

- ThreadSanitizer build of the core and the extension on every PR; `loom` interleaving tests on the lock manager and the commit batch.
- Free-threaded stress: on 3.14t, 32 threads committing on overlapping and disjoint oid sets through the in-process storage, with invariants checked after every round (serial monotonicity, no lost update, lock table empty).
- Lock-order and deadlock tests: adversarial clients requesting oids out of order; every run must end with `LockOrder` or timeout, never a hang.

**E. Protocol and input fuzzing**

- `cargo-fuzz` targets: msgpack frame decoder, `register`/`get_info` handshake, pickle header parser, native merge inputs. One hour per nightly, corpus kept in the repository.
- Fault injection on the wire: client disconnect at each 2PC state, server restart during vote, invalidation queue overflow, interrupted blob upload; the client must end in `ClientDisconnected` or a clean retry, the server must hold no stale locks.

**F. Replication and Kubernetes (Tier 2 and 3)**

- A `kind` cluster in CI: leader kill during commit, follower promotion, split-brain attempt with two lease holders, rolling restart; acknowledged commits are never lost, clients reconnect within 10 s.
- Tier 3 against a single-node FoundationDB in CI, the same conformance suites.

**G. Operations**

- Bit-flip injection: flip random bits in records, blobs and the dictionary on a copy of the corpus; every read must raise `CorruptRecord`, never return data; `zoe scrub` must list every injected fault and nothing else.
- Backup round trip: continuous shipping to a local S3-compatible bucket under the write benchmark, then `restore --to-tid` at ten random tids compared record by record against `loadBefore` on the live store; `backup verify` as a scheduled job.
- Feed exactness: an external consumer counts commits through the kill loop and reconnects from its last acknowledged tid; the count must equal the server's, with no duplicate.
- Forks: a fork of the corpus runs the Plone upgrade suite; the base store's checksums are unchanged afterwards; `plone.app.testing` layers stacked on forks run the Plone suites.
- Encrypted store: the whole conformance suite on an encrypted store; a scan of the files finds no plaintext record or blob; a wrong key fails to open with a clear error.
- Named storages: a two-mount Plone site on one server, the ZEO multi-storage tests, commits on one store under load while the other is packed.
- Pack safety: dry-run report compared to the actual removal set; revert inside the window restores every record; incremental pack under the read benchmark keeps p99 inside budget.

**CI matrix**

| Job | When | Platforms |
| --- | --- | --- |
| unit + conformance (A) | every PR | Linux x86-64; CPython 3.12, 3.13, 3.14 and 3.14t |
| Vfs crash simulation (C1), TSan (D), fuzz smoke (E, 5 min) | every PR | Linux x86-64 |
| `zodbshootout` quick (B1, concurrency 1 and 16) with regression gate | every PR | reference runner |
| full `zodbshootout`, `zoe-bench`, kill loop (C2), fuzz (E, 1 h), Plone suites (A), `kind` cluster (F) | nightly | reference runner |
| power-loss emulation (C3), Plone end-to-end (B5), log replay (B6) | weekly | reference runner |
| wheels and binaries (N11) | tag | Linux x86-64; macOS and Windows once they join |

Every row of the matrix is one function of the Dagger pipeline, called by a thin GitHub Actions workflow; the everyday loop on a developer machine is `just test` and `just bench` running natively inside `devenv`, and `just ci` runs the same Dagger function locally when a CI result needs reproducing. The tooling itself is described in the companion document.

**Acceptance thresholds** (first release; each number is a gate, not a wish)

| Requirement | Metric | Threshold |
| --- | --- | --- |
| N1 durability | acknowledged tids lost in C1, C2, C3 | 0 |
| N3 latency | p99 `loadBefore`, cache miss, 10 GB store, in-cluster | ≤ 0.3 ms |
| N4 throughput | `zodbshootout` ops/s versus ZEO 6, every scenario | ≥ 3× |
| N4 throughput | `zodbshootout` ops/s versus RelStorage/Postgres, every scenario | > 1× |
| N5 commit | commits/s, 50 writers, 10 ms fsync device | ≥ 1,000 |
| N7 startup | ready on a 100 GB store | ≤ 2 s |
| F7 conflicts | ConflictErrors on a hot `Length` with early locks on, 64 writers | 0 |
| success criterion | ConflictErrors reaching Zope in B6 versus ZEO | ≤ 10% |
| regression gate | p99 or ops/s change on B1 quick scenarios, per PR | within 5% |

**Regression tracking.** Every benchmark job writes JSON results to a `bench-results` branch; a dashboard (`github-action-benchmark` or Grafana over the JSON) plots each metric per commit, and the PR gate reads the last ten nightly runs to compute the baseline, so noise does not fail PRs and real regressions do.

## Risks and open questions

The largest risks are not in Rust; they are in matching ZEO's exact semantics and in the Plone-side hook for early locks.

| Risk | Impact | Mitigation |
| --- | --- | --- |
| ZEO protocol edge cases (cache verification after a long disconnect, `getInvalidations` gaps, blob shared-dir modes) differ from the source reading | a Plone cluster sees stale objects | run the full `ZEO.tests` suite from phase 2 week 1; keep the invalidation queue large; treat every ZEO test skip as a bug |
| redb write amplification caps commit throughput below RelStorage on write-heavy loads | N4 fails on update scenarios | the append-only log backend is designed in from day one behind the trait; the decision is made on phase 2 numbers |
| Early locks stall a four-thread Zope instance on a hot object | worse latency than ConflictError retries | default stays vote-time locking; early locks are opt-in per instance and documented for free-threaded Zope first |
| The `Connection.register` hook for early locks touches ZODB internals that change between releases | breakage on ZODB upgrades | keep the hook in one small module with a test per supported ZODB version; propose an upstream `IStorage` extension hook once it is proven |
| Native merges drift from `BTrees` semantics after a BTrees release | silent data differences | differential tests pin the BTrees version; CI runs against the latest BTrees weekly |
| `persistent`, `BTrees`, `zodbpickle` do not declare `Py_mod_gil_not_used` upstream | free-threaded parallelism in Zope stays off | phase 0 opens the PRs; a vendored build with the declaration is the fallback for the first production site |
| Tier 3 on FoundationDB: the 5 s / 10 MB transaction limits and the extra hop | large commits need splitting; p99 rises | stage locally, commit in one short write; Tier 3 is for clusters that need HA more than the last 100 µs |
| Single maintainer | bus factor | the spec, the harness and the conformance suites are the handover; keep the core small |
| Pickle-level assumptions (class paths, `zodbpickle.binary`) break on py2-migrated stores | migration failures | the round-trip test runs on a production copy before any release |
| Loupe edits bypass Plone invariants (catalog, events, workflow) | stale catalog, surprised editors | expert mode, catalogued-object badges, pickle diff before commit, the optional zoe.plone reindex queue; Loupe is documented as a scalpel, not a content editor |
| zoe-pickle cannot round-trip an exotic construct byte for byte | an edit rewrites more than the operator changed | opaque nodes re-emit original bytes and are not editable; the property test runs on a production copy before every release |
| RelStorage schema changes between versions | the converter misreads a site | pin the supported RelStorage versions, detect the schema version at start, refuse unknown ones |
| Production data or credentials leak into a public repository or a CI log | a data breach, and the adopting site's trust | synthetic corpora in public CI; private runner for production copies; secret scanning on every push; builders have no access to production data at all |
| The catalog mirror answers a query differently from ZCatalog | wrong search results, silently | differential suite on every PR; production query log replay nightly; any index or operator not covered by the suite takes the Python path by construction |
| Text index lags the tid or diverges from ZCTextIndex ranking | a just-saved page missing from search; tests that pin ranking break | hits filtered by the snapshot bitmap; commit lag as a metric with an alert; the compatibility switch for pinned rankings |
| A remote embedding provider sends content off the site owner's server | GDPR exposure without anyone deciding it | local ONNX provider by default; remote providers opt-in per site, logged, and counted by zoe\_vectors\_remote\_bytes\_total |
| An agent reaches content through MCP that Plone would have hidden | a permissions bypass | roles bitmap intersected before ranking in every mode; non-catalogued objects ops-only; the security suite runs on every PR touching zoe-mcp or zoe-vectors |
| A field-level merge combines two valid saves into an invalid object because an invariant was never declared | silent data inconsistency | policy derived from schemas with strict as the default; merging enabled per class only after shadow mode on production conflicts; every merge logged with the keys it merged and reversible from history |
| An encryption key is lost | the store and every backup are unreadable | encryption is off by default; the wrapping key lives outside the cluster (file, environment or KMS) and is the one secret the runbook makes an operator keep; `zoe backup verify` fails loudly as soon as a snapshot cannot be opened |
| The operations features dilute the core | phase 6 grows, the storage core gets less review | phase 6 is a parallel track behind the server gate; each tool is its own card with its own acceptance test; none of them is in the first release and none of them touches the commit path |

**Open questions**

- [x] Project name: ZOE (section Naming).
- [ ] Which conflict classes dominate the production logs of a real site? Needed before phase 3; a one-week log sample decides the native-merge list.
- [ ] Is the reference machine a developer workstation, an on-premise server or a cloud VM? The benchmark numbers only mean something on one fixed box.
- [ ] Does the first release target ZODB 6 and ZEO 6 only, or also the 5.x lines still common in Plone 6.0 deployments?
- [ ] Should the async Python client (`aload_before`, `aprefetch`, `acommit`) ship with the server in phase 2 for collective.starlette, or later?

## References

Pages opened during the analysis; repository figures come from clones made at the same time.

- [PEP 779 – Criteria for supported status for free-threaded Python](https://peps.python.org/pep-0779/)
- [PEP 803 – "abi3t": Stable ABI for free-threaded builds](https://peps.python.org/pep-0803)
- [PyO3 guide: Supporting free-threaded CPython](https://pyo3.rs/latest/free-threading.html)
- [persistent on PyPI, changelog and cp314t wheels](https://pypi.org/project/persistent/)
- [zope.interface 8.x changelog](https://simple-repository.app.cern.ch/project/zope-interface) and [issue #384, VerifyingBase cache races](https://github.com/zopefoundation/zope.interface/issues/384)
- [ZODB.interfaces – Connection thread model](https://zodb-docs.readthedocs.io/en/latest/_modules/ZODB/interfaces.html)
- [zopefoundation/ZODB](https://github.com/zopefoundation/ZODB), [zopefoundation/ZEO](https://github.com/zopefoundation/ZEO), [zopefoundation/persistent](https://github.com/zopefoundation/persistent), [zopefoundation/BTrees](https://github.com/zopefoundation/BTrees), [zopefoundation/zodbpickle](https://github.com/zopefoundation/zodbpickle)
- [jimfulton/byteserver](https://github.com/jimfulton/byteserver) and [jimfulton/filestorage2](https://github.com/jimfulton/filestorage2)
- [Byteserver project status thread, 2019](https://groups.google.com/g/zodb/c/u_QWRSRR14A)
- [Jim Fulton – The ZODB, PloneConf 2016 notes](https://2016.ploneconf.org/maurits.vanrees.org/weblog/archive/2016/10/jim-fulton-the-zodb.html)
- [Free-threaded Python for backend developers, 2026 overview](https://www.refontelearning.com/blog/free-threaded-python-backend-developers)
