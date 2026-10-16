# Handoff: the Plone end-to-end benchmark (B5), ZEO against ZOE

| | |
| --- | --- |
| Branch | `claude/plone-zeo-zoe-locust-benchmark-pvkgea`, pushed. No pull request yet. |
| Base | `main` at `c0db22b` (ZOE-192), merged in. The 14 runs of section 3 measured the servers and clients of `0f03dfb` (ZOE-186), on Linux. |
| Commits | `40597a7` the harness; `fc435a5` the timeline of the report; `2ef5560` the word codes in titles and `inzope/oidmap.py`; `9b66b29` the interim findings and this file; then the merge of `main`; `c50c71d` the port of the harness to macOS (section 4.4); `ee42ff8` the macOS series and the warmup fix; then steps 2–6 on macOS (section 3.2). |
| Card | None: the work came as a direct request. When a card exists, move this file to `docs/handoffs/<card>.md`, as `CLAUDE.md` asks, and name the card in the commits of the follow-up. |
| Status | Harness complete and tested, on Linux and on macOS. One repetition of the primary scenarios on `zeo` and `zoe-native` in two series: Linux (section 3) and macOS (section 3.1), all `verification pass`. The macOS series uses the word codes and the warmup fix (section 4.5), so steps 1–6 of section 5 are done there (sections 3.1 and 3.2). The source of the 5–13× disk bytes is measured (section 4.6). Nothing runs now. |
| Read first | `bench/README.md`, section "The Plone end-to-end benchmark" (the user manual); `docs/findings/plone-end-to-end.md` (the interim results); then this file. |

---

## 1. The brief

The human asked for a reproducible Locust benchmark of a realistic Plone site, stock ZEO against ZOE, that measures application performance and the practical benefit of ZOE's conflict handling. The aim is to find where optimization work helps real Plone users most, without forcing a winner and without any "3× faster Plone" claim from one scenario. The rules that shape the code:

- Compare at least stock ZEO with `ZEO.ClientStorage` and `zoe serve` with its native client. Add ZOE with optional features off, with single strategies, and the production combination, only where these exist. Never invent a "disabled" mode.
- Keep everything constant but the storage: Plone, add-ons, Python, process and thread counts, CPUs, caches, accounts, request mix, data, durability. Record compression; treat compression on and off as a separate experiment.
- A deterministic synthetic site when no sanitized snapshot exists (none was given), configurable in size, labelled synthetic. Equivalent ZEO and ZOE stores through the supported converter, checked for logical equivalence. A fresh copy before each write experiment, and a fresh warmup.
- Locust scenarios for browsing, catalog, editing, creation, workflow, mixed with background work, and controlled contention. Real UI endpoints for UI journeys, REST kept apart and labelled. One session and one account per user. Semantic success, not HTTP 200. Stable request names.
- Separate the kinds of failure: application checks, storage conflicts, conflicts resolved by stock resolvers, conflicts resolved by ZOE, publisher retries, client retries, final failures. No uncontrolled client retries; a retry counts inside its operation's latency.
- Equivalent server-side observability on both stacks. Never report an unavailable counter as 0. Correlate each Locust operation with its server requests through retries. Measure the overhead of the instrumentation.
- A runner that validates, restores, starts, warms up, runs, collects, verifies and cleans up. Closed workload stated as such. Load generator off the server CPUs. Interleave the stacks. Cold, warm and sustained-write cases defined. Real durable storage, normal durability. Jobs of about 30 minutes; no Cartesian explosion.
- A ledger of intended and acknowledged operations, ambiguous timeouts handled, read-only verification before any repair.
- A report per scenario and configuration, and a ranked optimization table that separates measurements from hypotheses.
- Do not provision paid infrastructure, touch a production site, publish private data, merge, or change the phase gates.

During the session the human asked for **one repetition only** ("3 reps is useless here"), then to **stop, push, and write this handoff**.

## 2. What exists on the branch

### 2.1 Files

```
bench/plone/                      uv project "zoe-plonebench" (Python 3.13), not in the python/ workspace
  pyproject.toml, uv.lock         locust 2.46.7, ZEO>=6, msgpack; dev: pytest
  src/plonebench/
    __main__.py      CLI: prepare, run, campaign, report (--timeline), serve, fixtures
    corpus.py        SiteSpec, presets small/medium/large, plan_site(): the deterministic plan of the site;
                     generated words (Zipf), search_terms(), code_words()/op_code(), file_bytes(), user_ids()
    workload.py      ROLES, SCENARIOS, assign_roles() (smooth weighted round robin), Targets (disjoint targets)
    locustfile.py    PloneUser (one class, its role decided by its index), the Steps load shape, operation(),
                     Attempt (request headers, pending records), every role_* journey
    forms.py         stdlib HTML form reader: sends every field as a browser does; CSRF token, canonical, messages
    ledger.py        Record, Ledger (JSON lines), read() (pending without final -> ambiguous)
    stack.py         STACKS, Settings, the zope.conf/wsgi.ini/zeo.conf templates, Stack (start/stop/run_script)
    fixture.py       prepare() (build on ZEO, zoe convert, digests, equivalence), restore()
    runner.py        RunOptions, run(): the whole run; _counters, _server_result, _verify, _analyse
    analysis.py      percentile, latency, probe join, op_summary, summarize_steps, zope_busy
    sampler.py       /proc sampler thread and summarize()
    servers.py       server_status through a stock client, Prometheus parser, deltas, histogram quantiles
    environment.py   machine, software and commit record (no host name, user name or path)
    verification.py  request() and judge(): the checks of the ledger against the observed site
    report.py        summarize() per scenario/stack/step, build() markdown, timeline()
    inzope/          scripts that run inside Zope through the interpreter of a buildout scenario
      run.py         configure_wsgi(conf), app = makerequest(Zope2.app()), exec(script)
      build_site.py  the site of a SiteSpec, and the manifest
      digest.py      the logical digest of the site (for equivalence)
      observe.py     the read-only observation for the verification
      oidmap.py      names the structure that holds each oid (catalog index, tool)
    probe/
      plonebench_probe.py        WSGI filter in each Zope process (on PYTHONPATH, wsgi.ini "call:" filter)
      zeoserver/launch.py        starts runzeo through the interpreter of the zeo scenario, after the probe
      zeoserver/plonebench_zeoprobe.py   counts the resolutions of the ZEO server
  tests/             30 tests: corpus and workload, forms, verification rules, accounting
bench/README.md      section "The Plone end-to-end benchmark", and a row in the "Run it" table
justfile             `just bench plone ...` at the top of the bench recipe
docs/spec/technical-specification.md   B5 item: what is built
docs/findings/plone-end-to-end.md      interim findings and the ranked table
docs/findings/plone-end-to-end/interim-report.md, interim-summary.json   the 14 runs
docs/SUMMARY.md      links the finding
```

### 2.2 The stacks (`stack.STACKS`)

| Name | Server | Client section | Eggs (buildout scenario) | Purpose |
| --- | --- | --- | --- | --- |
| `zeo` | ZEO 6.2 `runzeo`, FileStorage, pickle protocol Z5, `invalidation-queue-size 100` (the recipe default) | `<zeoclient>` | `zeo` | baseline; the ZEO server resolves conflicts in its process |
| `zoe-native` | `zoe serve --path ... --listen ... --admin-listen ...`, no other option (defaults: zstd 3, lock timeout 10 s, redb cache 1 GiB) | `<zoeclient>` with `native true` | `zoeclient` (`zoedb[plone]`) | the production combination: native client, extensions `sequences` (+ `trace_context` only if opentelemetry were installed: it is not), `zoe.plone` uidgenerator wiring; tier 3 client resolution |
| `zoe-asyncio` | same | `<zoeclient>` with `native false` | `zoeclient` | sequence wiring without the native client |
| `zoe-stock` | same | `<zeoclient>` (stock client over M5) | `zeoclient` (msgpack, no zoedb) | the server alone, no client extension |

Not built in ZOE, so no stack has them and no switch exists: tier 1 native merges, tier 2 field-level merges and their policy, shadow mode, the resolution log, early write-intent locks (`early-locks on` raises), append/CRDT fields, undo. Compression has no option in `zoe serve` (always zstd level 3). These facts come from `crates/zoe-core/CLAUDE.md`, `crates/zoe-server/CLAUDE.md`, `python/zoedb/src/zoe/component.xml` and `config.py`.

### 2.3 The constants (`stack.Settings`, recorded in each result)

2 Zope instances × 2 waitress threads; ZODB cache 30,000 objects per connection; pool 7; storage client cache 128 MB in memory for each instance (no `client` key, so no persistent cache file for either client); a blob cache per instance, `shared-blob-dir off`; `debug-mode off`; translogger on (as the recipe writes it). CPUs: server `0`, instance 0 on `1`, instance 1 on `2`, Locust on `3` (`taskset`). Ports: Zope 18080 and 18081, server 18100, admin 18199. Both servers fsync at commit: FileStorage in `_finish_finish`, `zoe serve` in its group commit before `tpc_finish` replies. The instances run CPython 3.13.16 (the system interpreter in this container) with the GIL on; Plone 6.1.5, Zope 5.14.2, ZODB 6.2, ZEO 6.2, plone.restapi 9.15.6 (the full list is in each result).

The runner writes `zope.conf` and `wsgi.ini` of every instance from one template, so the storage section is the only difference between stacks. The instances run `Zope2/Startup/serve.py wsgi.ini` with the `bin/interpreter` of the scenario (the eggs), not `bin/instance`.

### 2.4 The fixture

`just bench plone prepare` (default preset `small`) → `target/bench/plone/fixtures/small-ba01ff255313/`:

- 685 content objects: 57 Folders (4 sections × depth 2 × fanout 3, plus `bench/` and its 4 folders), 468 Documents, 104 News Items, 52 Files (16 KB, 128 KB, 1 MB; 21 MB of blobs), 4 Collections; 48 editors (one section each, through groups with Contributor, Editor, Reviewer), 32 members, an importer, a maintainer (Manager). Profiles: `plone.app.contenttypes`, `plonetheme.barceloneta`, `plone.restapi` (needed for JWT and REST).
- States: workflow pool half published half private; others about 70 % published, 20 % private, 10 % pending. Deterministic UIDs and dates from the seed.
- Pools: every 5th Document is in the workflow pool (browsing never reads it, its visibility changes), 2 of 5 in the edit pool. `bench/hot` (4 Documents), `bench/shared` (48), `bench/inbox`, `bench/import` serve the contention scenarios.
- Build on ZEO 80 s, `zoe convert` 0 problems (23 transactions, 54,593 records, 104 blobs), digests equal through both stores, 686 catalog entries (the site root has one). Store sizes: Data.fs 31 MB + 21 MB blobs; `zoe.redb` 65 MB apparent, 31 MB allocated, + blobs.
- `credentials.json` (mode 0600) holds the generated password of all bench accounts. It never goes on a command line or into the repository.

### 2.5 The scenarios (`workload.SCENARIOS`)

| Scenario | Roles (weights) | Interface |
| --- | --- | --- |
| `browse` | browse_anon 1, browse_member 1 | UI |
| `catalog` | search 1 (`@@search` text 35 %, filtered 20 %, subject 10 %, page 2 10 %, collection 15 %, REST `@search` 10 %) | UI (+ REST rows named `REST ...`) |
| `edit` | edit: `@@edit` form, own Documents of the edit pool | UI |
| `create-spread` | create in own section's folders, `++add++Document` | UI |
| `create-shared` | create in `bench/inbox` | UI |
| `workflow` | publish/retract from the workflow menu links, then 50 % an anonymous visibility check | UI |
| `mixed` | browse_anon 8, browse_member 3, search 4, edit 2, create_spread 1, workflow 1, create_shared 1 | UI |
| `mixed-background` | `mixed` + 1 maintainer (`manage_reindexIndex` of SearchableText, every 5 s) + 1 importer (REST POST into `bench/import`) | UI + REST |
| `contention-folder` | edit form, own Document each, all in `bench/shared` | UI |
| `contention-fields` | REST PATCH; users 2k and 2k+1 share `bench/shared/doc-k`, one writes title, the other description | REST |
| `contention-same` | REST PATCH title of `bench/hot/doc-(k mod 4)` | REST |
| `warmup` | browse_anon 2, browse_member 1, search 1, warm 1 (GET the edit form and cancel it, GET the add form; no save) | UI |

Roles go to users by a smooth weighted round robin over the largest step (`assign_roles`), so any prefix follows the weights. Editors own disjoint Documents (`Targets.own_docs`), so only the contention scenarios collide on purpose. Each user stays on one instance (sticky, like a load balancer with sessions). Think time 0 by default: a closed workload at maximum pressure.

### 2.6 One run (`runner.run`)

1. Check the fixture and the prerequisites (interpreters of the scenarios, the `zoe` binary in `target/release`).
2. Copy the store of the stack from the fixture (`cp -a --reflink=auto`) into `target/bench/plone/runs/<UTC>-<scenario>-<stack>-r<n>/store`.
3. Start the server (pinned), then the instances (pinned), and wait until each serves `/Plone`.
4. Warm up: the `warmup` scenario, 8 users, `--warmup` seconds, its own ledger. It saves no content; the edit form writes a lock and its cancel removes it.
5. Read `server_status` (and `/metrics` for zoe), run Locust with the steps, read them again.
6. Stop the instances (SIGTERM). Run `inzope/observe.py` in a client process of its own through the still-running server: read-only, never commits. Judge the ledger against it.
7. Stop the server. Analyse. Write `result.json`. Remove the store copy (unless `--keep-store`; kept when the run failed).

`campaign` loops repetition → scenario → stack, and reverses the order of the stacks on odd repetitions.

### 2.7 Measurement and accounting

- **Ledger** (`ledger/run.jsonl`): one record per logical operation: op id, kind, user, role, account, instance, step, start, end, outcome (`ok` / `failed` / `ambiguous` / `rejected`), client attempts, requests, HTTP statuses, target, values written. A write writes a `pending` record before its request; a pending record without its final one reads as `ambiguous`. Locust gets `--stop-timeout 120` so users finish the operation in flight.
- **Semantic checks in Locust:** view → status 200 and the canonical link of the path, not a login form; file download → the exact size; search → the results container; edit → redirect to the item and "Changes saved"; create → redirect under the folder and "Item created"; transition → "Item state changed"; visibility → anonymous sees it exactly when published; PATCH → 204; import → 201 and the new `@id`; reindex → a plone.protect token from a Plone page and the ZMI "Reindexing Performed" redirect.
- **Probe** (`probe/instance<N>.jsonl`): per HTTP request, the op id and attempt from the headers `X-Bench-Op`, `X-Bench-Attempt`, `X-Bench-Name` (never the URL, query or body); `attempts` (`PubStart` events), `retries` (`PubFailure` with retry), each error with conflict kind (write or read), class and oid and whether retried; `commit_seconds` (PubBeforeCommit → PubSuccess/PubFailure); `loadBefore` count/bytes/time, `store` count/bytes, `tpc_vote`, `tpc_finish` time (measured also when they raise), `tpc_abort`; `tryToResolveConflict` successes, failures and time; the CPU time of the thread. A snapshot every 5 s: requests in flight, objects in the ZODB caches, the client cache size.
- **ZEO server probe** (`probe/zeoserver.jsonl`): resolutions in the ZEO server process (it resolves conflicts itself), every 2 s and at exit.
- **Server counters:** `server_status` deltas for both servers (loads, stores, commits, aborts, conflicts, conflicts_resolved). For zoe also the Prometheus deltas and histograms: `zoe_vote_latency_seconds`, `zoe_finish_latency_seconds`, `zoe_lock_wait_seconds`, `zoe_load_latency_seconds`, `zoe_fsync_batch_size`.
- **Sampler** (`samples.jsonl`, 1 s): per process CPU (user, system), RSS, `/proc/<pid>/io` read/write bytes, context switches, run-queue delay summed over the threads (schedstat); machine CPU (steal, iowait); disk counters of the store device (flushes included).
- **Per step** (`analysis.summarize_steps`): an operation belongs to the step it started in; the first `--trim` seconds (5) of each step stay out. For each kind and for all: ok/s, outcomes, failure rate, first-attempt ok (client attempt 1 and every probe request with attempts 1), eventual ok, requests per op, server attempts per op, latency of ok ops (p50, p95, p99, `p99_reliable` from 200 ops), server totals and conflict classes; `zope_busy` (Little's law); resources.
- **Report:** median over repetitions with min–max; latency percentiles pooled from the ledgers; `~` marks a p99 of fewer than 200 operations; Zope and server CPU ms per ok op; server KB written per ok op; Locust CPU share; the verdicts; conflict classes.

### 2.8 Verification (`verification.judge`)

Checks, each with counts and examples in `result.json` → `verification.checks`:

| Check | Rule |
| --- | --- |
| `created` | each `ok` create exists at its target with its title |
| `failed_writes` | a `failed` write (HTTP error) did not commit |
| `phantoms` | every new object maps to a create of the ledger by the code in its title |
| `unique_ids` | no UID and no CMFEditions history id on two objects |
| `edits` | one writer: the last `ok` value, or an `ambiguous` value that started after it; several writers: an `ok` value that no `ok` write started after the end of, or an `ambiguous` value |
| `versions` | versions added = `ok` edits + k, 0 ≤ k ≤ unknown (ambiguous or rejected) |
| `version_titles` | for `bench/hot` and `bench/shared` targets: each `ok` title is in the version history |
| `workflow` | final state = the last `ok` transition (or a later unknown one); workflow history grew by the `ok` transitions + k |
| `permissions` | anonymous may View exactly when published |
| `catalog` | one entry per object; Title, review_state, modified, Anonymous in allowedRolesAndUsers match; no orphans, no uncatalogued objects, no duplicate paths |
| `search` | the code of the current title finds the object in the Title index; a superseded code does not |

## 3. Results so far (one repetition, small fixture)

Runs (under `target/bench/plone/runs/`, not in git): `20261015T202216Z-browse-zeo-r0` … `20261015T210625Z-mixed-zoe-native-r0`, 14 runs, all `verification pass`. Steps 1, 4, 16 users × 45 s, warmup 45 s, trim 5 s, think time 0, no client retry. Locust used under 5 % of its CPU in every step. Summary (ok ops/s; latency of ok ops; failed share; first-attempt share; conflicts per 1,000 ops; KB the server process wrote per ok op; Zope CPU ms per ok op):

| Scenario | Stack | Users | ok/s | p50 ms | p95 ms | failed | 1st try | confl/1k | KB/op | Zope ms/op |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| browse | zeo | 1 / 4 / 16 | 10.6 / 18.6 / 19.5 | 95 / 220 / 826 | 129 / 314 / 1135 | 0 | 100 % | 0 | ~0 | 89 / 104 / 99 |
| browse | zoe-native | 1 / 4 / 16 | 10.4 / 18.4 / 20.3 | 96 / 217 / 801 | 128 / 312 / 1074 | 0 | 100 % | 0 | ~0 | 90 / 104 / 95 |
| catalog | zeo | 1 / 4 / 16 | 8.9 / 17.6 / 18.1 | 118 / 240 / 916 | 148 / 312 / 1101 | 0 | 100 % | 0 | ~0 | 106 / 107 / 107 |
| catalog | zoe-native | 1 / 4 / 16 | 8.5 / 17.5 / 17.9 | 119 / 236 / 927 | 173 / 318 / 1110 | 0 | 100 % | 0 | ~0 | 112 / 110 / 107 |
| edit* | zeo | 1 / 4 / 16 | 2.2 / 3.3 / 3.5 | 449 / 718 / 4017 | 559 / 2062 / 5843 | 0 / 3.6 / 16.9 % | 100 / 65 / 60 % | 0 / 522 / 361 | 333 / 461 / 427 | 420 / 512 / 532 |
| edit* | zoe-native | 1 / 4 / 16 | 2.2 / 3.4 / 3.8 | 455 / 746 / 3719 | 566 / 3357 / 5977 | 0 / 1.4 / 15.3 % | 100 / 67 / 64 % | 0 / 478 / 322 | 4300 / 2661 / 2234 | 418 / 497 / 473 |
| create-spread* | zeo | 1 / 4 / 16 | 2.1 / 2.5 / 3.3 | 473 / 722 / 4458 | 576 / 4470 / 6924 | 0 / 4.7 / 3.0 % | 100 / 64 / 77 % | 0 / 689 / 463 | 641 / 1053 / 851 | 435 / 655 / 554 |
| create-spread* | zoe-native | 1 / 4 / 16 | 2.0 / 2.8 / 3.3 | 485 / 633 / 4397 | 610 / 3230 / 7881 | 0 / 5.1 / 2.3 % | 100 / 66 / 77 % | 0 / 633 / 391 | 5219 / 2572 / 1730 | 447 / 580 / 538 |
| create-shared* | zeo | 1 / 4 / 16 | 2.0 / 2.2 / 2.1 | 497 / 1222 / 7588 | 639 / 3740 / 9373 | 0 / 3.3 / 1.2 % | 100 / 63 / 74 % | 0 / 589 / 410 | 672 / 915 / 842 | 456 / 808 / 920 |
| create-shared* | zoe-native | 1 / 4 / 16 | 2.0 / 2.4 / 2.1 | 523 / 1399 / 7468 | 644 / 2538 / 9242 | 0 / 2.1 / 0 % | 100 / 67 / 83 % | 0 / 500 / 241 | 5122 / 2313 / 1910 | 457 / 763 / 903 |
| workflow | zeo | 1 / 4 / 16 | 3.8 / 7.1 / 7.4 | 328 / 685 / 2462 | 450 / 904 / 2988 | 0 | ~100 % | 0 / 0 / 3 | 37 / 40 / 42 | 242 / 269 / 260 |
| workflow | zoe-native | 1 / 4 / 16 | 3.9 / 7.3 / 7.3 | 313 / 682 / 2484 | 437 / 921 / 3055 | 0 | ~100 % | 0 / 3 / 0 | 1079 / 862 / 936 | 235 / 265 / 264 |
| mixed* | zeo | 1 / 4 / 16 | 10.9 / 19.7 / 13.1 | 93 / 212 / 1010 | 122 / 281 / 3226 | 0 | 100 / 100 / 98.5 % | 0 / 0 / 19 | ~0 / ~0 / 52 | 86 / 98 / 147 |
| mixed* | zoe-native | 1 / 4 / 16 | 10.2 / 19.3 / 14.3 | 96 / 212 / 931 | 142 / 302 / 3102 | 0 | 100 / 100 / 98 % | 0 / 0 / 19 | ~0 / ~0 / 545 | 92 / 100 / 135 |

\* These scenarios used the hex token in titles (section 4.1): their conflict numbers are inflated and must be rerun. `mixed` has writes only from 16 users on (the weighted roles of 1 and 4 users are readers).

What the data says, measured:

1. **Reads are at parity.** Zope CPU (90–110 ms per page) limits `browse` and `catalog`; the storage server stays under 1 % CPU. Saturation is reached at 4 users (2 instances × 2 threads on 2 CPUs); 16 users only queue.
2. **In a write request, the storage is small.** Request 400–650 ms; vote + finish about 10 ms; whole commit phase 60–90 ms; storage reads 25–31 ms per write request on zoe-native against 38–48 ms on zeo (the native client is faster, about 3–4 % of a request).
3. **Edits fail at about 15–17 % at 16 users on both stacks**, with only 60–67 % first-attempt success at 4 and 16 users. Retried requests take about 56–70 s per 45 s step on both stacks.
4. **Conflict sources** (oid map, section 4.2): ZCTextIndex `_wordinfo` buckets of `SearchableText` and `Description` dominate, then `sortable_title`, `UID`, `cmf_uid`, `path`, `id` BTrees, CMFEditions' shadow storage, catalog metadata. The `modified` DateIndex is minor (9 of 878). My first hypothesis (`modified`) was wrong.
5. **`portal_uidgenerator`:** every conflict of the creation scenarios on zeo is on it (135 in `create-spread`); none on zoe-native (F29 works). The creations then conflict on catalog structures instead, and the failure share of creations barely moves.
6. **Workflow transitions do not conflict** on either stack, and every anonymous visibility check passed. The zoe-native clients resolved about 640 bucket conflicts per step there, all successfully.
7. **Wasted resolution:** zoe-native clients resolved 800–1,750 bucket conflicts per step in `edit`/`create-*`; the ZEO server resolved 2,588 in the `edit` run and failed on 160. A vote that meets one unresolvable object fails whole after resolving the others, and the publisher redoes the request.
8. **Disk writes per operation:** the `zoe serve` process wrote 5–13× more bytes per write operation than the ZEO server at 1 user (edit 4.3 MB vs 0.33 MB; create 5.2 MB vs 0.64–0.67 MB; workflow 1.1 MB vs 0.04 MB), 2–6× at 4 and 16 users. The mean `zoe_fsync_batch_size` is 1.0 in every run: no batching at these commit rates. Latency did not suffer on this VM.

### 3.1 The macOS series

Runs: the lists `target/bench/plone/campaigns/macos-baseline-final.txt` (14 runs) and `macos-zeo-plain-final.txt` (4 runs of ZEO with `--zeo-fsync plain`), not in git. Same steps, warmup and trim as section 3. Commit `c50c71d`; the three `edit` runs ran again with the warmup fix (section 4.5) before it was committed. The report is `docs/findings/plone-end-to-end/macos-report.md`, and the finding (items 9–14) says what it shows. In short:

- Reads at parity, within 2 %. Pages cost 36–47 ms of Zope CPU here, against 90–110 ms on the Linux VM.
- Writes: ZOE equal or up to 15 % ahead in rate; with one repetition, not significant. `edit` at 16 users fails 1.4 % on ZEO and 0 % on ZOE once the warmup fix is in.
- `UniqueIdGeneratorTool` conflicts on ZEO only; read conflicts on ZOE in the creation scenarios (42 and 115).
- Full durability (`F_FULLFSYNC`) costs ZEO about 4 ms per `tpc_finish`, and at most 3 % of rate and p50. The table is in `target/bench/plone/reports/fsync-table.md`.

### 3.2 The macOS series, steps 2–6

Runs: the lists `macos-contention.txt` (16), `macos-background.txt`, `macos-cold.txt`, `macos-probe-on.txt`, `macos-probe-off.txt` and `macos-sustained.txt` (2 each) under `target/bench/plone/campaigns/`, not in git; all `verification pass`. Commit `ee42ff8`, `--zeo-fsync full`. The report is `docs/findings/plone-end-to-end/macos-next-report.md`; the finding says what it shows (items 15–20). In short:

- The native client is what makes ZOE faster: `zoe-native` is 5–14 % ahead of ZEO in the contention scenarios and uses 4–5 ms less Zope CPU per REST write; `zoe-stock` and `zoe-asyncio` (stock ZEO client against `zoe serve`) are about 10 % behind ZEO in the REST scenarios and level in the form scenarios. Stock and asyncio are equal.
- `contention-fields`: 190–210 `IOBucket` conflicts against 16–37 on the `Document` per stack. The expectation of step 2 holds: a field-level merge of the Document alone would not remove the conflicts. The starving of the title writer was not checked per role.
- `UniqueIdGeneratorTool` conflicts appear on `zoe-stock` (55), as on ZEO (139): the stock wiring has no `zoe.plone` sequence.
- `mixed-background`, cold start, probe on and off: no difference that one repetition can show. The probe costs under 1 %. The cold start costs about 10 % of rate in the first 10 s only, on both stacks.
- Sustained `create-spread`, 8 users × 600 s: ZOE 5.6 against 5.1 ok/s; both drift down 15–25 % as the site grows. `zoe serve` wrote 19 GB against 2.7 GB, fsync batch still 1.0, and its RSS grew to 1.07 GB at about 100 MB a minute (ZEO server: 82 MB). The redb cache defaults to 1 GiB and `zoe serve` does not set it: probable cause, not proven. The native client instances hold about 150 MB more (their 128 MB cache is in memory).

## 4. Things learned the hard way

### 4.1 The hex token bias (fixed in `2ef5560`, data not yet redone)

Titles carried `Edit <8 hex>` / `New <12 hex>`: a new word per save. ZCTextIndex's Lexicon gives a new word the next word id from a `Length`; two concurrent saves read the same length, take the same wid, and insert the same key with different words into `_words`/`_wids`, and the same wid into `_wordinfo`: unresolvable. So every pair of concurrent saves conflicted. Titles now start with a prefix (`Edit`, `New`, `Patch`, `Import`) and four words of `corpus.code_words` (vocabulary ranks 100–1100, which the site already holds). The verification parses the code with `verification._TOKEN` and searches it in the Title index (an AND of the four words). A sanity run of `edit` with codes (zoe-native, 4 users, 30 s; run dir `20261015T211021Z-edit-zoe-native-sanity`, not in the reports) still had 63 conflicts, in `SearchableText._wordinfo` (38), `sortable_title._index` (7), `Description._wordinfo` (10), others: the text index contention is real Plone behaviour; the lexicon conflicts were the artifact. The prefix words are new to the lexicons at their first use; that costs a few early conflicts only.

### 4.2 Mapping conflict oids

```sh
# oids from the probe of a run
python3 - <<'EOF'
import json, glob, collections
c = collections.Counter()
for f in glob.glob("target/bench/plone/runs/<run>/probe/instance*.jsonl"):
    for line in open(f):
        r = json.loads(line)
        if r.get("kind") == "request":
            for e in r["errors"]:
                if e.get("oid") and e.get("conflict"):
                    c[e["oid"]] += 1
json.dump(sorted(c), open("oids.json", "w"))
EOF
```

Then run `inzope/oidmap.py` against a read-only FileStorage copy of the fixture, with no server: a `zope.conf` with `<filestorage> path <copy>/Data.fs; blob-dir <copy>/blobs; read-only true </filestorage>` and `INSTANCEHOME` of the zeo scenario, then `target/plone/zeo/parts/instance/bin/interpreter bench/plone/src/plonebench/inzope/run.py <zope.conf> oidmap.py oids.json out.json`. It walks the catalog (metadata, uids, paths, every index down to buckets and tree sets), the lexicon and three tools. An oid that a run created (a bucket split) is not in the fixture: run oidmap on the store of that run (`--keep-store`) to name it.

### 4.3 Environment and tooling

- No `devenv` and no `just` in this container. The session installed `just` with `nix profile install nixpkgs#just` and the pinned Rust 1.99.0 with `rustup toolchain install 1.99.0` (+ `rustup override set 1.99.0` in the repo; the image had 1.97, and the crates require 1.99). `zoe` was built with `cargo build --release --locked -p zoe-server --bin zoe -p zoe-bench --bin zoe-bench`. The Plone instances came from `just plone zeo build`, `just plone zoeclient build`, `just plone zeoclient build` (network needed the first time: dist.plone.org and PyPI). In a normal setup use `devenv shell`.
- `uv` warns that `UV_NATIVE_TLS` is deprecated: harmless noise from the container.
- The `runzeo` shell script of `plone.recipe.zeoserver` sets its own PYTHONPATH of eggs and cannot import `zc.lockfile` (the `zc` namespace) here. The harness runs `probe/zeoserver/launch.py` with the scenario's `bin/interpreter` instead.
- Debian's CPython has `/usr/lib/python3.13/sitecustomize.py`, so a `sitecustomize` probe is shadowed: the ZEO probe is `plonebench_zeoprobe.py`, imported by `launch.py`.
- plone.protect aborts a write without a valid token and logs "Error checking for CSRF", but the ZMI still redirects with its success message. ZMI pages carry no token: take it from a Plone page (`@@search`). Logged-in Plone pages give `_authenticator` in forms and workflow links.
- A File's own URL downloads it for anonymous users; its page is `<path>/view`.
- `plone.restapi` must be installed as a profile for `@login` (JWT); the fixture installs it.
- An empty catalog query returns nothing in Plone 6: query with `path=/Plone`.
- `zoe_conflicts_resolved_total{path="python"}` and `server_status["conflicts_resolved"]` count resolutions of votes that end up passing; resolutions inside a vote that then fails are not counted. A minimal two-client `Length` conflict confirms the counter works. The probe of the instances counts every resolution.
- Process control: never `pkill -f` a pattern that the command line of the calling shell contains (it killed the shell once). Campaigns were started with `setsid nohup script.sh` and stopped with `kill -TERM -- -<pgid>`.
- `ruff check` / `ruff format --check` of `bench/plone` are clean with `uvx ruff`; `just check` does not cover `bench/` (the shootout harness is not covered either).

### 4.4 The port to macOS

The human made macOS a target, and the harness now runs on an Apple M3 Pro (6 performance and 6 efficiency cores, APFS). `bench/README.md`, paragraph "On macOS", is the user manual. What changed and why:

- **No CPU pinning.** macOS has no CPU affinity API. `stack.pin()` returns no prefix there, and each result records `cpu_pinning: "none: macOS has no CPU affinity"` and `core_kinds`. The `--*-cpus` options stay, for Linux.
- **Durability.** On macOS `os.fsync` returns in 0.04 ms and `F_FULLFSYNC` in 3 ms (measured on this machine): `fsync(2)` leaves the data in the cache of the drive. The Rust std of `zoe serve` uses `F_FULLFSYNC`. So `probe/zeoserver/launch.py` makes `os.fsync` of the ZEO server use `F_FULLFSYNC` before ZODB is imported (FileStorage binds `os.fsync` at import). `--zeo-fsync plain` keeps stock ZEO. Measured with `edit`, 4 users: ZEO `tpc_finish` 6.5 ms per commit with `full`, 3.3 ms with `plain`; `zoe serve` 7.3 ms. Each result records `durability`, and the report prints it in its header. The runs of one stack pool in the report, so never mix the two modes in one report.
- **The sampler.** `sampler._Darwin` reads `proc_pid_rusage` (`rusage_info_v4`) and `proc_pidinfo` through ctypes: CPU, RSS, bytes read and written to disk, threads, context switches. The run delay is the runnable time minus the CPU time, because `ri_runnable_time` counts the time on a CPU too. Checked against psutil, and under 3× oversubscription. The machine CPU and the disk counters come from psutil (now a direct dependency). The disk is the physical store under the APFS volume of the run (`diskutil`). Absent on macOS: steal, iowait, disk flushes, and the split of voluntary and involuntary switches (one `switches` key).
- **The rest.** `environment` reads `sysctl` and psutil instead of `/proc` and `/sys`. Store copies use `/bin/cp -c` (APFS clones): the devenv shell puts GNU `cp` first on PATH, and GNU `cp` has no `-c`. Buildout puts the eggs in `target/plone/eggs/cp313/` here, so `stack.egg_paths()` looks one level down too.
- **Environment.** `devenv shell` works; `just plone zeo|zoeclient|zeoclient build` and `cargo build --release` passed without change. `just bench plone prepare` built the same fixture as on Linux (`small-ba01ff255313`, 685 objects, equivalent) in 49 s.

Three smoke runs of `edit` (4 users × 20 s) passed the verification before the campaign. Their directories are deleted.

**Open question: the Python of the instances.** The instances run CPython 3.13 with the GIL, because the Plone build of the repository pins 3.13 (`justfile`, recipe `plone`; `plone/README.md`: "the newest version that Plone 6.1 supports"). So the benchmark cannot show what free-threaded 3.14t would give Plone. Moving the instances to 3.14 or 3.14t changes `plone/` and the `justfile` for the whole repository, and needs a Plone that supports it. The human decides.

### 4.5 The warmup left edit locks (fixed)

In the first macOS series, `edit` at 16 users had 25 % of its operations `rejected` on both stacks: `GET @@edit` gave 200 without the save form. The rejections came only from `editor-004`, `-008` and `-012`, the other editors of `section-00`, and all 27 rejected Documents were Documents that the warmup had opened. The `warm` role of the warmup runs as `editor-000` and owns every Document of `section-00`. Plone locks an item when its edit form opens, and only a save, a cancel or the timeout of the lock releases it. So the measured editors met items locked by `editor-000`. `role_warm` now posts the cancel button of each edit form that it opens and checks "Edit cancelled". A rerun gave no rejected operation. The Linux `edit` runs of section 3 have the same artifact in their failure share.

### 4.6 Why `zoe serve` writes more bytes (measured)

A throwaway program replays commits on a copy of the fixture store through a `Vfs` that counts the writes to `zoe.redb`. It rewrites k existing objects with their own pickles, 100 commits per case. Its source is in `target/bench/plone/wamp/` (not in git: a crate outside the workspace that depends on `zoe-core` by path; build it with `cargo build --release` in that directory). Its output is `target/bench/plone/reports/wamp-result.md`:

| k objects | placement | pickle KB/commit | redb KB/commit | redb ÷ pickles | OS disk KB/commit | page writes/commit |
| ---: | --- | ---: | ---: | ---: | ---: | ---: |
| 1 | – | 0.4 | 53 | 122 | 196 | 14 |
| 10 | random | 4.3 | 137 | 32 | 522 | 35 |
| 10 | adjacent | 4.4 | 63 | 14 | 236 | 17 |
| 80 | random | 39 | 632 | 16 | 2,309 | 158 |
| 80 | adjacent | 38 | 149 | 4 | 487 | 38 |

- redb writes whole 4 KiB pages copy-on-write: each changed leaf and its branch pages, the system tables (allocated and freed pages) and the header. About 53 KB per commit is fixed, and each object at a separate place adds about 7 KB. Nearly every write is one 4 KiB page.
- macOS counts a scattered 4 KiB write as 16 KiB of disk write, with `F_FULLFSYNC` or `fsync`. A plain file test, without redb, gives the same 4×; contiguous writes count 1×. So 632 KB of redb pages show as 2.3 MB: the 2.7–3.4 MB per `edit` operation of the server.
- The key of a record is the oid, then the tid (`format::record_key`). So the objects of one commit lie at the places of their oids, and a Plone save touches about 80 objects spread over the catalog.
- Anything that changes this (a key order, a log in front of the B-tree) is a format change: `crates/zoe-core/src/format/` is a red zone. This is a finding, not a card.

## 5. Next steps, in order

The time estimates are for the 4-vCPU Linux VM, one repetition (the human's choice). The macOS machine has 12 cores and no pinning: its results are a new series. Never pool them or compare them with the Linux runs of section 3. On macOS, steps 1–6 are done (sections 3.1 and 3.2). Step 8 has not run on either machine. Run it on macOS with `--zeo-fsync full` (the default), so it joins the macOS series.

1. **Rerun the write scenarios with the word codes** (≈ 35 min, 8 runs):
   ```sh
   just bench plone campaign --stacks zeo,zoe-native --scenarios edit,create-spread,create-shared,mixed \
     --reps 1 --steps 1:45,4:45,16:45 --warmup 45 --label codes --list target/bench/plone/campaigns/codes.txt
   ```
2. **Contention on all four stacks** (≈ 55 min, 16 runs). `zoe-stock` vs `zoe-asyncio` isolates the sequence wiring; `zoe-asyncio` vs `zoe-native` isolates the native client; the field and same-field scenarios show what tier 2 merges would have to solve:
   ```sh
   just bench plone campaign --stacks zeo,zoe-native,zoe-stock,zoe-asyncio \
     --scenarios contention-fields,contention-same,contention-folder,create-shared \
     --reps 1 --steps 4:45,16:45 --warmup 45 --label contention --list target/bench/plone/campaigns/contention.txt
   ```
   Expectation from the smoke tests (not yet measured in a campaign): in `contention-fields` the title writer starves (its transaction is longer) and both fields conflict on the same catalog metadata key, so a field-level merge of the Document alone would not remove the conflict.
3. **Background work** (≈ 10 min): `--scenarios mixed-background --steps 4:60,8:60`. The reindex of an unchanged SearchableText writes almost nothing: it is a long read transaction, CPU-heavy. Consider a reindex that writes (after edits) if the result is flat.
4. **Cold start** (≈ 5 min): `--scenarios browse --steps 4:60 --warmup 0 --trim 0`, then `just bench plone report --timeline 10 <runs>`. Cold = new server and Zope processes, empty ZODB and client caches; the page cache stays warm (cannot drop it here): say so.
5. **Probe overhead** (≈ 12 min): the same `browse --steps 4:45` with and without `--no-probe`, on both stacks; report the difference of ok/s and p95.
6. **Sustained writes** (≈ 25 min): `--scenarios create-spread --steps 8:600 --warmup 30`, then `--timeline 60`: watch latency drift, server RSS, bytes written, fsync batch, store growth.
7. **Investigations** (each a short experiment):
   - classify the `_wordinfo` conflicts (same key, split, deletion) on a kept store;
   - the bytes per commit are explained on macOS (section 4.6). Left: replay the oids of real Plone commits (from a kept store's `txn_oids`) instead of random ones; measure the factor of the Linux file system; see why fsync batches stay at 1.0 at 16 users (and at 8 users in the sustained run);
   - the RSS of `zoe serve` in the sustained run: rerun it with `--keep-store` and a small redb cache, and see whether the RSS levels off at the cache size;
   - the `ReadConflictError`s that only zoe-native shows (about 10–25 per step in writes): BTrees `readCurrent` under per-oid locks;
   - resolution order: count resolutions spent in failed votes; try unresolvable classes first.
8. **A medium fixture** (`just bench plone prepare --preset medium`: ≈ 4,900 objects, a build of perhaps 15–20 min), then `browse`, `catalog`, `edit` and the cold start on it: the storage weighs more when the site exceeds the caches.
9. **Write the final findings**: replace `docs/findings/plone-end-to-end.md` with all of the above (`just bench plone report ... --out ... --json ...` for the tables), keep measured statements apart from hypotheses, update the ranked table, list what did not run and why.
10. **Open a pull request** only when the human asks. The PR template (`.github/pull_request_template.md`) ends with a claims-and-evidence section. Nothing here touches a red zone: `bench/`, `docs/`, the `justfile` and the B5 item of the specification.

## 6. Done means

- Every scenario of section 2.5 run on its stacks with the current harness (codes), verification `pass` everywhere, or each failure explained.
- Cold, sustained and probe-overhead runs reported.
- The final finding with the ranked table, built from `report` output, with the commands, revisions, settings and limits.
- `just bench plone test` green; `uvx ruff check bench/plone` and `uvx ruff format --check bench/plone` clean.

## 7. Limits to keep in the report

- Synthetic site, small fixture: everything fits in every cache. Not a production benchmark.
- One machine per series: a shared Linux VM with 4 vCPUs, or an Apple M3 Pro with no CPU pinning and two kinds of core. One repetition by the human's choice, so differences of a few percent are noise.
- On macOS, both servers sync with `F_FULLFSYNC` by default, and stock ZEO does not (section 4.4).
- Closed workload, no think time; the offered rate falls when the server slows.
- CPython 3.13 with the GIL, 2 instances × 2 threads; free-threaded Plone is out of reach (Plone 6.1 stops at 3.13).
- The page cache cannot be dropped: no cold-disk claim.
- Compression is always on in ZOE; ZEO's FileStorage has none. Store sizes and bytes written reflect that.
- The probe adds a WSGI filter and wrappers on the storage methods; on macOS it costs under 1 % of rate in `browse` (section 3.2).
