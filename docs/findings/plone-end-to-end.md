# Finding: where ZOE helps a Plone site, and where it does not yet (interim)

The Plone end-to-end benchmark (B5, `just bench plone`, see `bench/README.md`). **Interim**: one repetition of each primary scenario on `zeo` and `zoe-native`, small fixture, in two series: a Linux VM (items 1–8) and an Apple M3 Pro (section "The macOS series"). Never compare numbers across the two series. The contention, background, cold-start, probe-overhead and sustained-write campaigns did not run yet. `HANDOFF.md` at the root of the repository gives the state and the next steps.

- Tables: [`plone-end-to-end/interim-report.md`](plone-end-to-end/interim-report.md). Machine-readable: [`plone-end-to-end/interim-summary.json`](plone-end-to-end/interim-summary.json).
- Commit under test: `0f03dfb` (the harness changed after it, the stores and the servers did not).
- Label: a **synthetic realistic-site benchmark**, not a production-site benchmark. Fixture `small`: 685 objects, the same logical site in a `Data.fs` and in its `zoe convert` copy (digest equal).
- Machine: 4 vCPU (Xeon 2.1 GHz, shared VM), 16 GiB, ext4 on a virtual disk. Server on CPU 0, two Zope instances of two threads on CPUs 1 and 2, Locust on CPU 3 (under 5 % of its CPU in every run). CPython 3.13, GIL on.
- Closed workload, no think time: 1, 4 and 16 users, 45 s each, first 5 s of each step left out.
- Every run passed its verification: no lost acknowledged write, no phantom, no refused write committed, versions, workflow history, permissions, catalog entries and search all as the ledger says.

## Measured

1. **Pages and searches: parity.** `browse`, `catalog`: ZEO and ZOE within 5 % on rate and p95 at every load (18–20 pages/s at saturation). Both Zope processes are CPU-bound (about 90–110 ms of Zope CPU per operation); the storage server uses under 1 % of a CPU. A faster storage cannot move these numbers on a warm, small site.
2. **Write requests: the storage is a small slice.** On both stacks a save takes 400–650 ms of request time. The vote and the finish take about 10 ms of it, the whole commit phase (pickling, catalog, storage) 60–90 ms. The native client loads faster: 25–31 ms of storage reads per write request against 38–48 ms on ZEO, about 3–4 % of the request.
3. **Edits fail under concurrency on both stacks, for the same reason.** `edit` at 16 users: 16.9 % failed saves on ZEO, 15.3 % on ZOE, after Zope's three retries. Most of these were an artifact of the harness, not conflicts (item 9). Then: 60–67 % of saves succeed at their first attempt at 4 and 16 users. The objects that conflict are catalog structures. A map of the conflicting oids (`inzope/oidmap.py`) puts most of them in the `_wordinfo` buckets of the ZCTextIndex indexes `SearchableText` and `Description`, then `sortable_title`, `UID`, `cmf_uid`, `path`, `id`. The `modified` DateIndex holds very few.
4. **ZOE removes the `portal_uidgenerator` conflict, and the next object takes its place.** On ZEO, every conflict of `create-spread` and `create-shared` is on `portal_uidgenerator` (135 in `create-spread`). On ZOE (`zoe.plone` sequence), none is there; about as many land on catalog buckets and read conflicts instead. The failure rate of creations barely moves (2–5 % on both).
5. **Workflow transitions do not conflict.** `workflow`: no failure on either stack at any load, and every anonymous visibility check after a transition passed. The ZOE clients resolved about 640 bucket conflicts per step, all with success.
6. **Resolution work is thrown away.** The clients of ZOE resolved 800–1,750 bucket conflicts per 45 s step in `edit` and `create-*`. The ZEO server resolved 2,588 in the `edit` run and failed on 160. A vote that resolves many buckets and then meets one that does not resolve fails whole, and the publisher retries the request from the start.
7. **ZOE writes far more to disk per operation.** Bytes that the server process wrote, per successful operation: `edit` about 2.2–2.7 MB on ZOE against 0.43–0.46 MB on ZEO; `workflow` 0.86–0.94 MB against 0.04 MB; `create-*` 1.7–2.6 MB against 0.85–1.05 MB. The mean fsync batch of `zoe serve` is 1.0 in every run: no group commit at these rates. This VM has fast enough storage that it does not show in latency; a site with a slow or metered disk would see it.
8. **The harness itself:** a hex token in each title made each concurrent save add a new word to the text index lexicons, and two such saves always conflicted on the new word id. That inflated the conflicts of the first `edit`, `create-*` and `mixed` runs (the runs in the interim report). Titles now carry four known words (`corpus.op_code`). A sanity run of `edit` with the codes at 4 users on ZOE still had 63 conflicts in 30 s, in the same `_wordinfo` buckets: the text index contention is real, the lexicon part was the artifact. These four scenarios must run again.
9. **The harness, again:** the warmup opened edit forms as `editor-000` and never saved or cancelled them. Plone locks an item when its edit form opens, so these Documents stayed locked. At 16 users, other editors own some of them, and their edit form showed the locked item: the ledger counts that as `rejected`. In the macOS series, all 27 rejected Documents were Documents that the warmup opened, and only the editors of `section-00` met them. The warmup now cancels each edit form that it opens. A rerun of `edit` on macOS: failed and rejected saves at 16 users fell from 25 % to 1.4 % on ZEO and to 0 % on ZOE. The Linux failure share of item 3 holds the same artifact; the conflict counts and classes do not.

## The macOS series

- Tables: [`plone-end-to-end/macos-report.md`](plone-end-to-end/macos-report.md). Machine-readable: [`plone-end-to-end/macos-summary.json`](plone-end-to-end/macos-summary.json).
- Commit under test: `c50c71d`; the three `edit` runs ran with the warmup fix of item 9 on top of it.
- Machine: Apple M3 Pro, 6 performance and 6 efficiency cores, 36 GiB, APFS on the internal SSD. No CPU pinning: macOS has none. CPython 3.13, GIL on. Both servers sync with `F_FULLFSYNC` (the harness makes the ZEO server use it, as `zoe serve` does).
- The same fixture (`small-ba01ff255313`), steps, warmup and trim as the Linux series. Every run passed its verification. Locust used under 10 % of a CPU in every step.

Measured:

10. **Reads: parity.** `browse` and `catalog`: ZEO and ZOE within 2 % on rate and p95 at every load; 44–45 pages/s at saturation from 4 users on. Zope CPU per page is 36–47 ms, against 90–110 ms on the Linux VM.
11. **Writes: no loss, small gains not yet proven.** `edit`, `create-spread`, `create-shared`, `workflow` and `mixed`: ZOE is equal to ZEO or ahead in rate at every load, by up to 15 % (`create-spread` at 4 users: 6.3 against 5.5 ok/s; `edit` at 4 users: 8.1 against 7.4). With one repetition, this is not significant. `edit` at 16 users: 7.0 ok/s on ZEO, 7.5 on ZOE; failed 1.4 % against 0 %.
12. **Conflicts.** `edit`: about 300–400 conflicts per 1,000 operations at 4 and 16 users on both stacks, nearly all on `IOBucket` (the `_wordinfo` buckets of item 3). Creations: on ZEO every conflict is on `UniqueIdGeneratorTool` (101 in `create-shared`, 214 in `create-spread`); on ZOE none is, and read conflicts take most of their place (42 and 115). Workflow transitions: 3 conflicts on ZEO, none on ZOE; the ZOE clients resolved 3,909 bucket conflicts, the ZEO server 4,874.
13. **Full durability costs ZEO about 4 ms per commit.** A second ZEO series with stock `fsync` (`--zeo-fsync plain`; the drive keeps the data in its cache): `tpc_finish` takes 1.6–3.5 ms with `fsync` and 5.4–7.5 ms with `F_FULLFSYNC` in `edit`, up to 27 ms against 18 ms in `create-shared` at 16 users. Rate and p50 change by at most 3 %.
14. **Why ZOE writes more bytes: two layers, measured.** The server process wrote 2.7–3.4 MB per `edit` operation on ZOE against 0.38–0.48 MB on ZEO. A replay of commits on a copy of the fixture store through a counting `Vfs` (100 commits each; the store holds 31,126 objects, median pickle 245 bytes) gives:
    - redb rewrites each changed page and its branch pages copy-on-write, 4 KiB at a time. A commit costs about 53 KB for one object, and about 630 KB for 80 objects at random places: 16 times their pickle bytes. 80 adjacent objects cost 150 KB, 4 times. A Plone save writes about 80 objects, most of them catalog buckets far apart.
    - On this machine the OS counts each scattered 4 KiB write as 16 KiB of disk write, with `F_FULLFSYNC` or with `fsync`; contiguous writes count about 1×. A plain file test gives the same factor without redb. So the 630 KB of redb become 2.3 MB on disk: the 2.7–3.4 MB per operation above. The Linux VM has 4 KiB pages; its own factor is not measured.
    - FileStorage appends each record once, so ZEO writes about the pickle bytes plus their headers.
    - The mean fsync batch of `zoe serve` stays 1.0: one sync per commit, as on Linux.

## Not measured yet

- Field-level merges, structural merges in Rust, shadow mode and early write-intent locks: phase 4, not built. No stack has them; nothing here measures them.
- Compression off: `zoe serve` has no option for it.
- Free-threaded Python: Plone 6.1 supports CPython up to 3.13; the instances run 3.13 with the GIL on (recorded in each result).
- Cold filesystem pages: the container cannot drop the page cache.
- A larger fixture (`medium`, `large`): not built; the site fits in every cache.

## Ranked optimization opportunities

| # | Observed bottleneck | Evidence | Affected workloads | Headroom (estimate) | Candidate change | Complexity and correctness risk | Next experiment |
| --- | --- | --- | --- | --- | --- | --- | --- |
| 1 | Unresolvable conflicts in ZCTextIndex `_wordinfo` buckets and other catalog BTrees | 3, 6, 8; oid map | every concurrent save and creation | up to the 15 % failed saves and 35–40 % retried saves at 16 users; retries cost most of the p95 | tier 1 native merges of buckets with value policies (a posting dict as a set union per key), or a catalog that defers text indexing; field-level merge on the content object does not help alone | high: a wrong merge loses postings; needs the differential tests of F6 and shadow mode first | rerun `edit` with codes, then classify each conflicting key (same key, split, delete) |
| 2 | Wasted resolution before a failing vote | 6 | the same | the client CPU of the resolutions of failed votes | try the classes without `_p_resolveConflict`, and the ones most likely to fail, first, so a vote fails before it resolves | low in the client; no format change | count resolutions per failed vote with the codes |
| 3 | Disk writes per commit and no group commit | 7, 14 | every write | none on either machine in latency; bytes written: 16× the pickles in redb pages, 4× more on macOS | fewer pages per commit: a key order that puts the objects of one commit near each other, or a log in front of the B-tree; batch more commits per sync under real concurrency | high: a key order or a log is a format change (red zone); durability (N1) must hold | the sustained-write campaign with disk counters; the replay of item 14 with the oids of real Plone commits |
| 4 | Zope CPU per page (rendering) | 1, 2 | every read and every write | the large one for users, but not in the storage | outside ZOE: caching, rendering | – | a larger fixture and cold caches, where the storage reads weigh more |
| 5 | `portal_uidgenerator` | 4 | concurrent creation | done: 0 conflicts on ZOE | already built (F29) | – | none |
| 6 | Load time of the client | 2 | every request | about 3–4 % of a write request today | the native client already wins; more matters on a larger site with cold caches | low | medium fixture, cold start |

Measured statements are 1–14. The headroom figures and the candidate changes are hypotheses. With one repetition, differences of a few percent between ZEO and ZOE are within run-to-run noise.
