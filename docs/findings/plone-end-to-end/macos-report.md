# Plone end-to-end benchmark: ZEO and ZOE

- Runs: 14; commit c50c71d38f1e
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

## browse

rendered pages: folders, documents, news, files, breadcrumbs; anonymous and logged-in members (mix {'browse_anon': 1, 'browse_member': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 26.4 (26.4–26.4) | 41 | 43 | 45 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 35.7 | 0.02 | 0.1 | 1.7% | pass (n=1) |
| zeo | 4 | 44.6 (44.6–44.6) | 90 | 116 | 120 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.7 | 0.01 | 0.1 | 3.3% | pass (n=1) |
| zeo | 16 | 45.3 (45.3–45.3) | 352 | 450 | 478 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.3 | 0.01 | 0.1 | 3.4% | pass (n=1) |
| zoe-native | 1 | 26.0 (26.0–26.0) | 41 | 45 | 47 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 36.2 | 0.03 | 0.0 | 1.8% | pass (n=1) |
| zoe-native | 4 | 44.7 (44.7–44.7) | 90 | 117 | 120 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.6 | 0.02 | 0.0 | 3.2% | pass (n=1) |
| zoe-native | 16 | 45.4 (45.4–45.4) | 350 | 454 | 475 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.2 | 0.01 | 0.0 | 3.4% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## catalog

@@search with text, filters, sorting and pages; collections; REST @search with metadata (mix {'search': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 22.0 (22.0–22.0) | 49 | 53 | 54 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 43.0 | 0.01 | 0.2 | 1.5% | pass (n=1) |
| zeo | 4 | 43.0 (43.0–43.0) | 101 | 119 | 147 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 47.2 | 0.00 | 0.1 | 3.0% | pass (n=1) |
| zeo | 16 | 44.4 (44.4–44.4) | 390 | 443 | 463 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 46.0 | 0.00 | 0.1 | 3.2% | pass (n=1) |
| zoe-native | 1 | 21.4 (21.4–21.4) | 51 | 55 | 57 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 44.1 | 0.00 | 0.0 | 1.6% | pass (n=1) |
| zoe-native | 4 | 42.8 (42.8–42.8) | 102 | 116 | 145 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 47.4 | 0.00 | 0.0 | 3.0% | pass (n=1) |
| zoe-native | 16 | 44.1 (44.1–44.1) | 398 | 440 | 458 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 46.4 | 0.00 | 0.0 | 3.2% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## create-shared

the add form of Plone, every user in the one folder bench/inbox (mix {'create_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 4.0 (4.0–4.0) | 255 | 314 | 322~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 229.3 | 9.48 | 724.4 | 4.0% | pass (n=1) |
| zeo | 4 | 3.9 (3.9–3.9) | 677 | 2218 | 3521~ | 1.9% | 75.9% | 3.35 | 392.4 | 3 | 475.6 | 18.05 | 933.3 | 7.1% | pass (n=1) |
| zeo | 16 | 3.5 (3.5–3.5) | 4736 | 5821 | 7887~ | 0.7% | 81.4% | 3.26 | 278.6 | 1 | 593.2 | 15.02 | 853.8 | 8.2% | pass (n=1) |
| zoe-native | 1 | 4.1 (4.1–4.1) | 250 | 309 | 315~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 216.8 | 13.19 | 4051.1 | 4.1% | pass (n=1) |
| zoe-native | 4 | 4.1 (4.1–4.1) | 734 | 1984 | 3492~ | 0.6% | 71.1% | 3.39 | 397.6 | 1 | 447.3 | 25.74 | 5329.8 | 7.6% | pass (n=1) |
| zoe-native | 16 | 3.6 (3.6–3.6) | 4504 | 5537 | 5861~ | 0.0% | 86.1% | 3.17 | 166.7 | 0 | 585.8 | 19.42 | 5140.9 | 9.1% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 101; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: <read conflict> 42, IOBucket 35, OOBTree 12, OIBTree 1; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## create-spread

the add form of Plone, each user in folders of its own section (mix {'create_spread': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 4.7 (4.7–4.7) | 218 | 245 | 255~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 194.7 | 9.22 | 594.9 | 3.7% | pass (n=1) |
| zeo | 4 | 5.5 (5.5–5.5) | 263 | 2767 | 4016 | 3.1% | 74.1% | 3.41 | 473.7 | 7 | 260.9 | 16.14 | 888.9 | 4.5% | pass (n=1) |
| zeo | 16 | 6.0 (6.0–6.0) | 2188 | 4682 | 6338 | 3.6% | 78.5% | 3.35 | 422.3 | 9 | 263.3 | 15.04 | 894.4 | 5.4% | pass (n=1) |
| zoe-native | 1 | 4.8 (4.8–4.8) | 211 | 241 | 248~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 183.5 | 13.01 | 4039.2 | 3.8% | pass (n=1) |
| zoe-native | 4 | 6.3 (6.3–6.3) | 245 | 1411 | 4230 | 2.3% | 78.0% | 3.32 | 366.8 | 6 | 227.1 | 23.07 | 5027.2 | 5.1% | pass (n=1) |
| zoe-native | 16 | 6.0 (6.0–6.0) | 2198 | 5173 | 6999 | 2.4% | 75.9% | 3.35 | 395.9 | 6 | 233.9 | 21.72 | 4894.2 | 5.3% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 214; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: <read conflict> 115, IOBucket 67, OOBTree 9, OIBTree 1; server resolved [2], ZEO server resolver [None], mean fsync batch ['1.00']

## edit

the edit form of Plone on Documents, each user its own Documents (mix {'edit': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 5.1 (5.1–5.1) | 202 | 212 | 218 | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 176.5 | 4.88 | 380.6 | 3.7% | pass (n=1) |
| zeo | 4 | 7.4 (7.4–7.4) | 233 | 1344 | 3879 | 1.0% | 81.3% | 3.27 | 293.3 | 3 | 204.2 | 7.93 | 442.8 | 5.7% | pass (n=1) |
| zeo | 16 | 7.0 (7.0–7.0) | 2044 | 3545 | 5991 | 1.4% | 74.1% | 3.37 | 395.1 | 4 | 213.6 | 8.92 | 477.0 | 5.4% | pass (n=1) |
| zoe-native | 1 | 5.2 (5.2–5.2) | 201 | 211 | 215 | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 166.6 | 7.17 | 2695.9 | 3.8% | pass (n=1) |
| zoe-native | 4 | 8.1 (8.1–8.1) | 227 | 1505 | 2797 | 1.5% | 80.1% | 3.28 | 311.9 | 5 | 198.2 | 12.62 | 3151.3 | 6.2% | pass (n=1) |
| zoe-native | 16 | 7.5 (7.5–7.5) | 1888 | 3898 | 5563 | 0.0% | 76.3% | 3.36 | 357.9 | 0 | 195.0 | 12.97 | 3352.9 | 5.8% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 201; server resolved [0], ZEO server resolver [3549], mean fsync batch ['–']
- zoe-native: IOBucket 193, <read conflict> 16; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## mixed

a mix of interactive traffic, weights in the mix (mix {'browse_anon': 8, 'browse_member': 3, 'search': 4, 'edit': 2, 'create_spread': 1, 'workflow': 1, 'create_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 26.4 (26.4–26.4) | 41 | 43 | 45 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 35.7 | 0.02 | 0.1 | 1.8% | pass (n=1) |
| zeo | 4 | 45.4 (45.4–45.4) | 92 | 138 | 144 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.1 | 0.01 | 0.1 | 3.4% | pass (n=1) |
| zeo | 16 | 32.6 (32.6–32.6) | 393 | 1329 | 2021 | 0.2% | 97.9% | 1.22 | 33.7 | 2 | 62.0 | 1.24 | 48.7 | 4.7% | pass (n=1) |
| zoe-native | 1 | 26.1 (26.1–26.1) | 41 | 44 | 46 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 36.1 | 0.03 | 0.0 | 1.8% | pass (n=1) |
| zoe-native | 4 | 45.2 (45.2–45.2) | 92 | 139 | 144 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 45.2 | 0.02 | 0.0 | 3.4% | pass (n=1) |
| zoe-native | 16 | 33.7 (33.7–33.7) | 383 | 1257 | 1877 | 0.2% | 98.2% | 1.22 | 27.4 | 3 | 59.5 | 1.66 | 280.2 | 4.9% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 35, UniqueIdGeneratorTool 8, OOBTree 1; server resolved [0], ZEO server resolver [804], mean fsync batch ['–']
- zoe-native: IOBucket 32, <read conflict> 3, OOBTree 2; server resolved [120], ZEO server resolver [None], mean fsync batch ['1.00']

## workflow

publish and retract through the workflow menu, with an anonymous visibility check (mix {'workflow': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 9.3 (9.3–9.3) | 141 | 164 | 169 | 0.0% | 100.0% | 2.67 | 0.0 | 0 | 95.7 | 0.89 | 46.3 | 4.2% | pass (n=1) |
| zeo | 4 | 16.7 (16.7–16.7) | 307 | 381 | 409 | 0.0% | 99.9% | 2.70 | 1.5 | 0 | 120.2 | 1.58 | 49.3 | 8.3% | pass (n=1) |
| zeo | 16 | 16.9 (16.9–16.9) | 1073 | 1319 | 1396 | 0.0% | 99.7% | 2.69 | 3.0 | 0 | 121.3 | 1.59 | 49.2 | 8.5% | pass (n=1) |
| zoe-native | 1 | 9.3 (9.3–9.3) | 141 | 165 | 168 | 0.0% | 100.0% | 2.67 | 0.0 | 0 | 94.9 | 1.41 | 480.6 | 4.3% | pass (n=1) |
| zoe-native | 4 | 17.0 (17.0–17.0) | 297 | 376 | 408 | 0.0% | 100.0% | 2.70 | 0.0 | 0 | 117.8 | 2.30 | 523.2 | 8.5% | pass (n=1) |
| zoe-native | 16 | 17.1 (17.1–17.1) | 1048 | 1310 | 1378 | 0.0% | 100.0% | 2.69 | 0.0 | 0 | 120.4 | 2.28 | 554.6 | 8.7% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IITreeSet 3; server resolved [0], ZEO server resolver [4874], mean fsync batch ['–']
- zoe-native: none; server resolved [3909], ZEO server resolver [None], mean fsync batch ['1.00']

