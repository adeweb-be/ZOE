# ZOE — working methods and roadmap

@Antoine Duchêne

How ZOE gets built: one human, a team of AI agents, cards gated by tests, worked in interactive sessions. The technical design lives in Doc.

## Way of working: one human, a team of agents

ZOE is built by AI agents with one human as product owner and architect, so the process is designed around what agents are good at (fast, parallel, tireless, literal) and what they are bad at (judgement about scope, invariants nobody wrote down, and knowing when to stop). The rule that follows from this: nothing is a task until it has a test that decides it, and nothing merges on an agent's word.

&#91;embedded content: card lifecycle · two agent approvals, two human gates\]

The tinted boxes are the only two places the human is in the loop; a card that touches no red zone never waits on a person after it is approved.

**Roles**

| Role | Who | Does | Never does |
| --- | --- | --- | --- |
| Product owner and architect | the human | owns the technical specification and the ADRs, approves cards, reviews red zones, runs the batch review and the retro | writes code by hand except spikes and acceptance tests |
| Planner | agent, fresh session per planning run | slices a roadmap phase into cards, orders them by dependency and risk, writes the acceptance test skeleton for each, keeps the board current | starts implementation |
| Builder | agent, one or more Claude Code sessions per card, in the card's own git worktree | implements one card until its acceptance test is green, writes unit tests, opens the PR with a claims-and-evidence section | edits acceptance tests, CI configuration, or files outside the card's scope fence |
| Reviewer | agent, fresh context, never the builder's session | reviews the diff against the card, the specification and the crate's invariants file; checks the claims against the evidence; requests changes or approves | merges |
| Verifier | CI, plus an agent that reads the results | runs the harness tier for the PR, the benchmark quick set and the regression gate; posts numbers on the PR | approves on green alone when a red zone is touched |
| Documenter | agent, after every merge | diffs code against the specification and the ADRs, proposes updates and new ADRs, maintains CONTEXT.md | changes a decision |
| Release manager | agent, on tag | changelog from merged cards, version bump across crates and wheel, artifacts, signed binaries | tags without the human's go |

**Cadence**

| Rhythm | What happens | Human involvement |
| --- | --- | --- |
| continuous | a session builds a Ready card, a fresh reviewer and CI run on every PR, the human merges | the merge |
| per batch | the planner's next batch of cards is approved, open questions answered, order re-ranked; batches are as frequent as the human wants them, not calendar-bound | the main touchpoint |
| per red-zone PR | the human reviews the diff with the reviewer agent's report beside it | minutes per PR |
| per phase gate | gate check against the testing harness; one `just tsan` run on `main`, where a reported race becomes a card; the documenter reconciles specification and code; ADRs for every decision taken in the phase | an hour or two |
| retro, on the metrics | when rejection rate, cost per card or flaky tests cross a threshold, a retro happens; its output is changes to CLAUDE.md files, invariants files, skills and card templates: the retro improves the agents, not the people | the human reads the proposal |

**Repository conventions that make agents reliable**

- Root `CLAUDE.md`: how to build, test and bench, as just recipes; the red-zone list; the stop-and-ask rules; the claims-and-evidence PR template.
- One `CLAUDE.md` per crate with its invariants in plain sentences ("locks are acquired in ascending oid order", "the backend never allocates tids"); the reviewer checks diffs against this file.
- `docs/adr/`: one Architecture Decision Record per decision in the specification, numbered, with context, decision, consequences; new decisions go through a card of their own.
- `CONTEXT.md`: the domain glossary (oid, tid, serial, ghost, vote, pack, intent lock) so agents and humans use one vocabulary.
- Skills for the recurring procedures: run the conformance suite, run the benchmark set and compare to baseline, cut a release, write a handoff when a session ends unfinished.
- Trunk-based: short-lived branch per card, one agent per worktree, squash merge, no force pushes, CI configuration changed only by a human.
- Stacked pull requests: when several PRs are open at once, they form one merge train. Each branch merges the tail of the train before it is marked ready, PRs merge in train order, and the next branch merges `main` after each squash merge. The root `CLAUDE.md` gives the steps.
- `CODEOWNERS` marks the red zones: the on-disk format, pack, and the process files (`.github/` and `loops/`). A red-zone PR needs the human's approval; everything else merges on the reviewer agent's approval (ADR 0022).

**Tooling**

Two loops. The inner loop, on a laptop or in an agent session, is `devenv shell` plus `just`, running `cargo`, `uv`, `pytest` and `buildout` natively. CI is Dagger, triggered by GitHub Actions. They meet in one recipe, `just ci`, which runs the Dagger pipeline locally to reproduce a CI result before pushing.

| Tool | Role | Rule |
| --- | --- | --- |
| devenv | `devenv.nix` at the root pins the Rust toolchain, CPython 3.12, 3.13, 3.14 and 3.14t, Node for Loupe and every CLI below; `devenv shell` gives a laptop, a runner and an agent session the same environment | a tool that is not in `devenv.nix` does not exist for the project; the file changes through cards, reviewed like code |
| just | the `justfile` is the single entry point: `just check` (fmt, clippy, lints), `just test`, `just conformance`, `just plone` (builds the Plone instances), `just crash` (the process kill loop, then the kill loop under the simulator), `just bench`, `just loupe`, `just ci`, `just release`; recipes wrap `cargo`, `uv`, `pytest`, `buildout` and `dagger` | `CLAUDE.md` files name recipes, never raw commands; a command an agent needs and cannot find becomes a recipe in the same PR |
| uv | packaging for what the project ships, under `python/`: a uv workspace holding `zoedb` and `zoe.plone`, a committed `uv.lock`, `uv python install 3.14t` for the free-threaded interpreter, the wheel built with maturin through `uv build` | no pip, no hand-made virtualenvs; a dependency change ships with its lock file in the same commit |
| zc.buildout | the Plone instances the suites and benchmarks run against, under `plone/`: one `buildout.cfg` per scenario (in-process `<zoe>`, `<zeoclient>` against `zoe serve`, `FileStorage` and ZEO as baselines), `versions.cfg` pinned to a Plone release, `zoedb` from the wheel `uv build` just produced, installed into `target/plone/zoedb` and linked as a develop egg so that each build takes the current code, `plone.recipe.zope2instance` writing `zope.conf` and `bin/test`; no mxdev | buildout owns Plone, uv owns the project's own packages, nothing crosses that line; the Plone suites run from `bin/test` (zope.testrunner); the eggs and download caches are Dagger cache volumes |
| pytest | the runner for the project's own Python tests: `zoedb`, `zoe.plone` and the ZODB and ZEO conformance suites (unittest-based, collected unchanged); `pytest-xdist` where the suites allow; run on 3.12, 3.13, 3.14 and 3.14t, Linux x86-64 | new Python tests are plain pytest; a card's Python-side acceptance tests are pytest files named on the card; the Plone suites are the one exception and stay on `bin/test` |
| Dagger | CI as code in `ci/`: a Dagger module written in Python with the functions `check`, `test`, `conformance`, `plone`, `bench`, `crash`, `zoe-binary`, `server-image` and `release`, each running in containers built from the same `devenv` definition; one cache shared by the runners | CI logic lives here and nowhere else; reviewed like code; `just ci` runs it locally when a CI failure needs reproducing, never for the edit-test loop |
| GitHub Actions | thin workflows, one per event (pull request, merge queue, nightly, release), each a few lines that call the Dagger module; the merge queue; the nightly bench on the self-hosted runner | a workflow holds no logic; one exception: a first step frees the disk of a hosted runner before Dagger runs; a workflow change is a human-only change |
| GitHub | the repository, Issues as cards, the Projects board, pull requests, Releases, Pages for the dashboard, `CODEOWNERS`, branch protection, secret scanning, Dependabot | the board holds the state of every card |

Inner loop: `devenv shell`, then `just <recipe>`, which runs `cargo`, `uv run pytest` or `bin/buildout` natively, incremental, seconds per run (minutes for a fresh Plone instance, cached after). CI: a workflow calls `dagger call <function>`, containerised, reproducible, cached. An agent session lives in the inner loop and runs recipes only; it reaches Dagger through `just ci` and `just release` and nothing else.

## Units of work

The unit is the card: one card, one pull request, and a card carries everything a builder needs without the conversation that produced it. A card holds one whole mechanism: the core, the server, the client, `zoe.plone`, the tests, the benchmarks and the change of the specification or of an ADR that the mechanism needs. So the mechanism is tested end to end and reviewed once, and a phase needs few merges. Cut a mechanism into two cards only when each part ships and is measured on its own.

- **Steps.** The card lists its steps in order. A step is one commit or more. The description of the pull request gives the evidence of each step, so a reviewer can read the commits in order.
- **Sessions.** A card takes as many builder sessions as its budget gives. Each session ends with `docs/handoffs/<card>.md` up to date: the step reached, what failed, the next step.
- **Checkpoints.** When a step changes the on-disk format, the wire protocol, a public Python API, a trait signature that another crate uses, or a behaviour that the specification states, the card names the change. The first commit of that step is the ADR. The builder stops and asks for the human's approval of the ADR on the pull request, then builds on it. A checkpoint is a planned stop-and-ask, not a card of its own.
- **Unknowns.** An unknown that the card must answer is the first step of the card, and its finding goes into `docs/spikes/` in the same pull request.

**The task card**

```markdown
# ZOE-142 — group commit: coalesce concurrent tpc_finish into one fsync
Phase 2 · crate zoe-core · red zone: no

Requirement: N5
Goal: 50 concurrent writers on a 10 ms fsync device reach ≥ 1,000 commits/s.
Steps: 1. the batch and its leader; 2. one fsync for each batch, and a failed fsync fails every member; 3. the metrics of the batches and the contention benchmark
Acceptance tests (do not edit): tests/commit/group_commit.rs, bench/contention.rs@n5
Scope fence: crates/zoe-core/src/commit/**, crates/zoe-core/tests/commit/**
Out of scope: lock manager, backend trait
Invariants: locks held by members stay held until the batch is durable; a failed fsync fails every member
Checkpoints: none
Spike or build: build
Budget: three sessions; a spent budget means stop and ask, not try harder
Done when: acceptance green, bench posted, reviewer approved
```

Cards come from the roadmap phase and the requirements table of the specification, so every card cites a requirement id, and every requirement is closed by the set of cards that cite it.

**Definition of Ready.** A card has a requirement id, an acceptance test that fails today, its steps and its checkpoints, a scope fence, written invariants, a budget, and no dependency on an unmerged card.

**Definition of Done.** Acceptance test green; unit tests added; the PR harness tier green; benchmark numbers posted and within the regression gate; the claims-and-evidence section lists what was tested and what was not; reviewer approval; human approval when a red zone is touched; the specification or an ADR updated when behaviour changed.

**Stop-and-ask rules for builders.** A builder stops and writes a question on the card instead of guessing when a change would alter the on-disk format, the wire protocol, a public Python API, a trait signature used by another crate, or an acceptance test; when the budget is spent; and when the only way to make the test green is to weaken it.

**Spikes.** A spike card ends in a written finding in `docs/spikes/` and no merged code. A spike is a card of its own only when its answer decides whether a build card exists; otherwise it is the first step of the build card (units of work, above). The big unknowns are spikes before they are builds: redb versus the append-only log, pickle round-trip on a production copy, the early-lock hook in ZODB.

**Order of work inside a phase.** Walking skeleton first: the thinnest end-to-end slice (open store, store one object, finish, reload, kill, reopen) with the crash simulation attached, then widen. Hardest unknowns next, as spikes.

**Process metrics**, on the same dashboard as the benchmarks: cards merged per batch, first-review rejection rate, red-zone PRs waiting on the human, flaky-test count, benchmark drift, cost in tokens per merged card. When rejection rate rises, the retro fixes the card template or the invariants file; when a card spends its budget before its last step, the retro cuts the card at a step or gives it a larger budget.

## Sessions

The work goes through interactive sessions: the human and an agent, with no automated loop and nothing running unattended. A session takes the roles of the table above in turn. As builder, it works on one card in its own worktree on the branch `card/<id>` and opens the pull request. For review, it starts a fresh reviewer session that never shares the builder's context. The human approves red zones, answers the questions on cards and merges. The briefs for each role are in `loops/prompts/<role>.md`.

**Guardrails.** `.github/` (the workflows and `CODEOWNERS`) and `loops/` are human-only. Agents never hold production credentials or data (project organisation, below).

**Handoffs.** A session that ends before its card is done (the end of the session, the budget, a crash, a stop-and-ask, a checkpoint) writes `docs/handoffs/<card>.md`: state, what was tried, what failed, the next step. The next session starts from that file and the branch, not from a conversation. Sessions are disposable; files are the memory.

## Roadmap

Eleven phases, each closed by a measurable gate from the specification's testing harness; the in-process storage ships first because it needs no server and already proves the core. There are no durations: the work is done by agents, and a phase starts as soon as the gates it needs are green, so phases 3 to 7 run in parallel once the server gate is passed.

&#91;embedded content: roadmap · eleven phases and their gates\]

Each diamond is a gate from the testing harness; nothing moves to the next band until its gate is green.

| Phase | Scope | Gate | Needs |
| --- | --- | --- | --- |
| 0 Groundwork | `Py_mod_gil_not_used` and thread-safety audit upstream in `persistent`, `BTrees`, `zodbpickle`; ConflictError ranking from production logs; reference machine and `zodbshootout` baseline for `FileStorage`, ZEO 6 and RelStorage; repository scaffold for agentic work (devenv, justfile, uv workspace, buildout for the Plone instances, Dagger pipeline, Actions workflows, CLAUDE.md files, ADRs, CONTEXT.md, the board) and the walking-skeleton card | baseline published; upstream PRs opened; top-ten conflict classes known | nothing; starts first |
| 1 Core and in-process | `zoe-core` with redb backend, 2PC, per-OID locks, pack, blobs, `Vfs` crash simulation; PyO3 `Storage`; `zope.conf` stanza; `zodbconvert` round trip; conformance suites; Loupe v0 (folder mode, browser, time travel); `Data.fs` converter in Rust; record and blob checksums and zstd compression in the on-disk format, size guardrails, pack dry-run; the sequences primitive; clock and I/O traits for deterministic simulation; a codec byte, a branch id and typed field slots reserved per record | `ZODB.tests` green; crash simulation 0 losses; every injected bit flip detected; in-process beats `FileStorage` on `zodbshootout` | phase 0 baseline |
| 2 Server | tokio server, full protocol matrix, invalidations, blob streaming and shared blob directories, health and metrics, graceful shutdown, `zoe-bench`, ZEO and Plone suites, Tier 1 manifests, named storages for mount points, read-only mode, repozo-style backup and zoe verify, OpenTelemetry traces; the Loupe space screen in folder mode; MCP ops tools and prompts, read-only, on a store folder over stdio; sequences over the protocol with zoe.plone wiring for the history ids of `portal_uidgenerator`; the deterministic simulator over clock and network, with the kill loop run under it | a Plone 6 site runs on `<zeoclient>`; ZEO tests green; at least parity with ZEO on `zodbshootout` in every cell that reaches the server (all but `read_hot`), both through `ZEO.ClientStorage`; a site with a mount point runs on one server; kill loop 0 losses; 0 conflicts on id allocation with 50 concurrent creators | phase 1 gate |
| 3 Native client | the native Rust client with a persistent cache as a `ZEO.ClientStorage` drop-in, `zoe.zeo.ClientStorage` on `zoe-client`; one shared host cache per node for that client | `zoe serve` through the native client faster than ZEO through `ZEO.ClientStorage` in every cell of the native gate (ADR 0039); ZEO client tests green against the native client; the native client beats `ZEO.ClientStorage` on the client benchmark | phase 2 gate; parallel with 4, 5, 6 and 7 |
| 4 Conflicts | structural and field-level merges, the merge policy derived from Dexterity schemas, shadow mode, early write-intent locks with the client hook, log-replay harness, undo on top of the merge tiers; append-only fields, CRDT sets and registers, the ephemeral TTL namespace and the registry as durable KV, each with a zoe.plone behaviour; the writes-on-GET report, then incidental writes | differential tests green; ZODB and ZEO undo suites green; ≤ 10% of ZEO's ConflictErrors in B5 (`edit` and `create-spread`, 4 and 16 users, `zoe-native-qi` against `zeo`, in each scenario); 0 on a hot `Length` with early locks; 0 conflicts on workflow\_history and session writes on replay | phase 2 gate; parallel with 3, 5, 6 and 7 |
| 5 High availability | Tier 2: log shipping, Lease election, follower reads, `kind` cluster tests, Helm chart; read-only fallback during failover; RelStorage converter | failover under 10 s with no acknowledged loss | phase 2 gate; parallel with 3, 4, 6 and 7 |
| 6 Catalog | `zoe-catalog` mirror (bitmaps, sort arrays, path trie, metadata table, per-tid versions), `catalog_query` protocol extension, `zoe.plone.catalog` hook on `Catalog.search`, native BM25 text index on tantivy with `ZoeTextIndex` and the delta record type, query translator and compatibility switch, `zoe-bench` catalog scenarios; the incremental security index, traversal prefetch from the path trie, the reference index, the order index for ordered folders and shadow rebuilds | differential suite identical on the Plone test suites and a production query log; 5-index intersection with sort on 1M objects under 5 ms p99 warm; mirror ready within 60 s cold; text query under 20 ms p99; security bitmaps identical to reindexObjectSecurity on the corpus; a shadow rebuild with no query downtime | phase 2 gate; the field-store spike verdict; parallel with 3, 4, 5 and 7 |
| 7 Operations and data tools | copy-on-write forks (zoe fork: a native DemoStorage for test layers and upgrade rehearsals), the change feed (a resumable stream of commits from any tid on the admin port), encryption at rest with keys from file, environment or KMS, scheduled scrub, pack retention window and incremental pack, zoe rename-class, Parquet export, maintenance mode and per-storage quotas; the transactional outbox feeding the feed, derivations keyed (oid, tid, kind) with plone.scale as first consumer, zoe.testing layers on forks, zoe replay with sampled read sets, the versioning behaviour backed by history over the retention window; Loupe server mode, the live tail, the operations screen with a Loupe screen for each feature, full CRUD and undo-a-transaction, the optional `zoe.plone` reindex hook, and the writes-on-GET report in Loupe; `zoe mcp` inside `zoe serve` and against a server, with the `tail` resource and streamable HTTP | a rehearsal upgrade runs on a fork of a production copy; an external indexer driven by the feed misses no commit through the kill loop; scrub finds every injected fault; conformance green on an encrypted store; plone.scale writes nothing on GET once on derivations; a production conflict replays on a fork | phase 2 gate; Loupe's undo-a-transaction and its writes-on-GET report build on the undo and the report of phase 4; parallel with 3, 4, 5 and 6 |
| 8 AI-native | MCP content tools, chunking with per-type policies, local ONNX embedding provider behind a build flag plus remote opt-ins, `VectorIndex` trait with the builtin in-store backend and the Qdrant backend, hybrid search through `catalog_query`, `@search` and MCP, security pre-filtering by the roles bitmap, rebuild-vectors; embeddings as a derivation; the tid-stamped answer cache invalidated by the feed | security suite green on every mode and backend; builtin and qdrant agree on top-k; hybrid beats BM25 alone on the labelled query set; lag under 30 s at p99 under write load | phase 6 gate; the change feed and the derivations of phase 7 |
| 9 Scale-out and fast paths | Tier 3 backend on FoundationDB or TiKV, S3 blob store, append-only log backend if phase 2 numbers demand it; signed blob URLs and blob dedup by hash on the S3 store | conformance green on FDB; stateless fronts serve a cluster; log backend wins its benchmark or is dropped | phase 5 gate |
| 10 Catalog projection | delta records for every index type, persisted delta log with snapshots, proxy index classes in `zoe.plone`, one-time rebuild migration, catalog BTrees and `Length` counters leave the store | a production site runs on the projection with zero bucket conflicts and identical query results over a full release cycle; started only after the mirror has answered production queries identically for a full release cycle | phase 6 gate plus production use of the mirror |
| Later | `IMVCCStorage` fast path, async Python client, Rust `persistent` and `BTrees` sharing the merge code; macOS and Windows for the wheels, the server binary and Loupe | — | — |

First usable release: the server gate; the in-process storage is usable from the core gate.

## Feature triage

58 candidate features were triaged against the phase 2 gate: 1 Must, 1 spike, 24 Should, 16 Could, 16 Won't. Must = needed for the phase 2 gate. Should = inside the eleven phases, in the phase named. Could = after the roadmap. Won't = out of scope. `zoe.plone` may ship opt-in behaviours that change how Plone stores things; stock Plone keeps working unchanged. The on-disk format reserves room from phase 1 for Should features not yet built: typed field slots, a branch id, a codec byte and a blob hash per record.

| Feature | Verdict | Lands in | Note |
| --- | --- | --- | --- |
| Server-side sequences | Must | phase 1 primitive, phase 2 protocol | replaces the shared id counter `portal_uidgenerator`; `zoe.plone` wires it |
| Field-addressable objects | Spike | spike before phase 6 design | conflicts and write amplification, throughput, ergonomics; weighting decided when the numbers exist |
| Deterministic simulation testing | Should | phase 1 traits, phase 2 simulator | clock and I/O traits in `zoe-core` from the walking skeleton |
| Compression, zstd now, deltas and dictionaries later | Should | phase 1 zstd; deltas after the bench | codec byte per record |
| Blob content hashes, dedup later | Should | phase 1 hash; phase 9 dedup | BLAKE3 per blob record |
| Append-only fields | Should | phase 4 | `workflow_history` first; the tier 2 strategy `append` (ADR 0046); a stored primitive in the protocol only if B5 shows that rewriting the history costs too much |
| CRDT sets and registers | Should | phase 4 | shipped with append-only fields as the tier 2 strategies `union` and `lww`; counters are sequences |
| Ephemeral namespace with TTL | Should | phase 4 | sessions, CSRF keys, locks; `zoe.plone` session data manager and locking backend |
| Registry as durable KV | Should | phase 4 | same primitive, durable flavour; `plone.app.registry` storage swap |
| Incidental writes | Should | phase 4 report; async path after the outbox | writes-on-GET report first |
| Incremental security index | Should | phase 6 | inherited bitmap pointers; equality with `reindexObjectSecurity` is the test |
| Traversal pushdown | Should | phase 6 | the client prefetches the chain from the path trie |
| Reference index | Should | phase 6 | back-references from the mirror's decode pass; link integrity and relations |
| Ordered folders | Should | phase 6 | order index in the mirror; `zoe.plone` ordering adapter |
| Shadow indexes | Should | phase 6 | background rebuild, atomic swap |
| Branches (forks) | Should | phase 7 | branch id reserved in the key layout from phase 1 |
| Snapshot test fixtures | Should | phase 7 | `zoe.testing` layer on forks; first fork consumer |
| Commit forensics | Should | phase 7 | `zoe replay`; read sets behind a sampling flag |
| Transactional outbox | Should | phase 7 | at-least-once post-commit delivery; the feed becomes a consumer |
| Derivations | Should | phase 7 | keyed (oid, tid, kind); `plone.scale` first, embeddings in phase 8 |
| Retention rules | Should | phase 7 | `pack --keep`; versioning's prerequisite |
| Versioning from history | Should | phase 7 | `zoe.plone` behaviour replacing CMFEditions storage; CMFEditions stays available |
| RAG answer cache | Should | phase 8 | tid-stamped, cited oids, invalidated by the feed |
| In-process embeddings | Should | phase 8 | the ONNX provider behind a build flag; default build calls an endpoint |
| Shared host cache | Should | phase 3 | one mmap per node for the native client |
| Signed blob URLs | Should | phase 9 | `blob_url(oid, ttl)`; `zoe.plone` redirects downloads on the S3 store |
| Time travel: open at a tid, object diff | Could |  | retention stays in phase 7 |
| Live queries | Could |  | incremental view maintenance; simple queries first |
| Write-stream anomaly rules | Could |  | thresholds on the feed, auto-fork, alert |
| Zope-less anonymous read path | Could |  | needs derivations and the security index proven |
| Learned co-access prefetch | Could |  |  |
| Admission control and QoS | Could |  | interactive and batch classes |
| Hot-set snapshots | Could |  | warm a cold client from the server's working set |
| Containment-clustered layout | Could |  | only with the log backend |
| Generational pack | Could |  | incremental pack in phase 7 stays single-generation |
| Continuous backup and point-in-time restore | Could |  | removed from phase 5; repozo-style backup from phase 2 is the roadmap answer |
| Hash-chained log, TSA anchoring, legal hold | Could |  |  |
| Crypto-shredding | Could |  | encryption at rest in phase 7 stays |
| Public anchoring | Could |  | inherits the chain |
| Inclusion proofs | Could |  | inherits anchoring |
| Signed commits | Could |  | optional per-connection key |
| SQL façade | Could |  | waits on the field-store spike; Parquet export in phase 7 stays |
| Native multi-tenancy | Won't |  | isolation by process and named storages; per-storage quotas stay |
| Anonymising clone | Won't |  |  |
| Agent branches with merge-back | Won't |  | agents commit to main |
| Permission explainer in Loupe | Won't |  |  |
| Read replicas with session tokens | Won't |  | plain follower reads with bounded staleness stay in phase 5 |
| Online backend swap | Won't |  | `zoe convert` with a maintenance window |
| EUDI wallet attestations | Won't |  |  |
| Actor-tagged transactions | Won't |  |  |
| Semantic transaction notes | Won't |  |  |
| Conflict-resolver synthesis | Won't |  | merge policies are hand-written |
| Index advisor | Won't |  |  |
| WASM plugins | Won't |  | embedded Python and Rust are the two extension paths |
| Server-side subtree clone and move | Won't |  |  |
| Class rename maps on read | Won't |  | `zoe rename-class` stays a one-shot conversion tool |
| Portable subtree bundles | Won't |  |  |
| Blocks as CRDT documents | Won't |  |  |

## Project organisation

ZOE is adeweb's project, built on its owner's own time, hardware and accounts; the first production site is a user, not an owner.

| Thing | Decision | Rule |
| --- | --- | --- |
| owner | adeweb; the GitHub organisation is `adeweb-be` | nothing from an adopting site (data, credentials, code) enters the repositories; contributions from adopters or anyone else arrive as pull requests under the repository licence |
| repositories | `adeweb-be/ZOE`, the monorepo: `crates/`, `python/zoedb/`, `apps/loupe/`, `deploy/`, `docs/`, `bench/`, `loops/`; `adeweb-be/zoe-bench-results` for the nightly JSON and the dashboard on GitHub Pages | Loupe stays in the monorepo through phase 7, which holds its last planned screens, and splits after it only if frontend contributors need their own cadence; the phase 0 patches to `persistent`, `BTrees` and `zodbpickle` go upstream to zopefoundation from forks in the `adeweb-be` organisation |
| tags and versions | `v0.3.0` for the Cargo workspace and the `zoedb` wheel together; `loupe-v0.1.0` for Loupe | semantic versioning; the on-disk format version is independent of the release version |
| licences | Apache-2.0 for everything, the Plone extra included | the Rust ecosystem's norm; keeps the door open to the Plone Foundation and to any adopter |
| documentation site | GitHub Pages of `adeweb-be/ZOE`, built with mdBook from `docs/` by the Dagger `docs` function on every merge to trunk; the bench dashboard is the Pages site of `adeweb-be/zoe-bench-results` | `docs/` in git is the source of truth: the technical specification, this document and the ADRs are exported there once and maintained in git from then on, kept current by the documenter; nothing is hosted on a private domain |
| data hygiene | test corpora in public CI come from a synthetic Plone site generator; production copies from an adopting site run only on that site's private runner, whose results are posted as numbers, never as data; secret scanning on every push; builders never hold production data | the one rule that survives every retro |

## Process risks and open questions

| Risk | Impact | Mitigation |
| --- | --- | --- |
| The human becomes the bottleneck: batches and red-zone reviews wait | the project slows to the approver's pace | red zones are narrow and listed; the planner batches by dependency so one approval unlocks many cards |
| Agents optimise for the test, not the goal | green acceptance tests on code that misses the point | acceptance tests written before the build and never editable by the builder; the reviewer checks claims against evidence; shadow runs and differential suites judge behaviour, not coverage |
| Invariants nobody wrote down | reviewers approve changes that break an unstated rule | per-crate invariants files, grown by every retro and every incident |
| Cost drift | token spend grows faster than merged work | cost per merged card on the dashboard; rising cost triggers the retro |
| Production data or credentials leak into a public repository or a CI log | a data breach, and the adopting site's trust | synthetic corpora in public CI; private runner for production copies; secret scanning; builders have no access to production data at all |
| Single owner | bus factor | the specification, the harness, the ADRs and the role briefs are the handover; anyone with the repository can continue the work |

**Open questions**

- [x] Which conflict classes dominate the production logs of a real site? The synthetic logs of B5 and B6 replace the one-week log sample (step 0 of the ConflictError strategy in the specification). B5 ranks the posting dicts of ZCTextIndex first, then the shared `BTrees.Length` counters.
- [ ] Is the reference machine a developer workstation, an on-premise server or a cloud VM? The benchmark numbers only mean something on one fixed box.
- [ ] Should the async Python client ship with the server in phase 2 for collective.starlette, or later?
- [ ] How are the three field-store spike criteria weighted (conflicts and write amplification, throughput, ergonomics)? Decided when the numbers exist, before phase 6 design.
