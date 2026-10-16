# The macOS series, handoff steps 2–6

One repetition each, commit `ee42ff8`. Built with `just bench plone report` from the run lists `macos-contention.txt`, `macos-background.txt`, `macos-cold.txt`, `macos-probe-on.txt`, `macos-probe-off.txt` and `macos-sustained.txt`. Each part keeps the header of its report.

## Contention, four stacks

- Runs: 16; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

### contention-fields

REST PATCH: two users on each Document, one writes the title, the other the description (mix {'patch_fields': 1}; interface rest)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 37.1 (37.1–37.1) | 27 | 42 | 236 | 1.5% | 97.5% | 1.06 | 78.4 | 22 | 23.8 | 3.76 | 522.9 | 2.9% | pass (n=1) |
| zeo | 16 | 37.5 (37.5–37.5) | 187 | 1637 | 4204 | 1.2% | 96.4% | 1.07 | 87.4 | 19 | 24.2 | 3.93 | 509.6 | 3.0% | pass (n=1) |
| zoe-native | 4 | 40.3 (40.3–40.3) | 26 | 43 | 872 | 1.2% | 96.4% | 1.07 | 80.3 | 19 | 20.3 | 6.23 | 1928.4 | 3.1% | pass (n=1) |
| zoe-native | 16 | 39.4 (39.4–39.4) | 179 | 1130 | 4899 | 0.9% | 96.5% | 1.07 | 77.3 | 15 | 19.7 | 6.30 | 2218.3 | 3.2% | pass (n=1) |
| zoe-asyncio | 4 | 34.2 (34.2–34.2) | 30 | 48 | 181 | 2.0% | 96.9% | 1.07 | 91.6 | 28 | 25.7 | 6.10 | 1960.1 | 2.8% | pass (n=1) |
| zoe-asyncio | 16 | 33.8 (33.8–33.8) | 212 | 1567 | 3746 | 1.0% | 95.3% | 1.09 | 99.6 | 14 | 26.0 | 6.38 | 2207.8 | 2.8% | pass (n=1) |
| zoe-stock | 4 | 33.9 (33.9–33.9) | 30 | 48 | 629 | 1.5% | 96.9% | 1.07 | 84.2 | 21 | 25.2 | 5.95 | 1933.5 | 2.7% | pass (n=1) |
| zoe-stock | 16 | 33.7 (33.7–33.7) | 216 | 1550 | 3506 | 1.2% | 95.2% | 1.09 | 101.0 | 17 | 26.4 | 6.49 | 2196.2 | 2.8% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 190, Document 29, IIBTree 21, IIBucket 11; server resolved [0], ZEO server resolver [6939], mean fsync batch ['–']
- zoe-native: IOBucket 209, IIBucket 19, Document 16, IIBTree 7, <read conflict> 3; server resolved [7403], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-asyncio: IOBucket 210, Document 31, IIBTree 14, IIBucket 6, <read conflict> 3; server resolved [4862], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-stock: IOBucket 196, Document 37, IIBucket 9, IIBTree 8, <read conflict> 4; server resolved [5407], ZEO server resolver [None], mean fsync batch ['1.00']

### contention-folder

the edit form, each user its own Document, all Documents in one folder (shared folder and catalog structures) (mix {'edit_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 7.0 (7.0–7.0) | 239 | 1629 | 4417 | 1.7% | 81.9% | 3.29 | 320.6 | 5 | 210.1 | 8.10 | 673.0 | 5.8% | pass (n=1) |
| zeo | 16 | 7.8 (7.8–7.8) | 1608 | 3265 | 5521 | 1.9% | 82.4% | 3.26 | 298.7 | 6 | 212.2 | 7.86 | 660.7 | 6.6% | pass (n=1) |
| zoe-native | 4 | 8.0 (8.0–8.0) | 220 | 1365 | 2837 | 1.5% | 81.4% | 3.26 | 287.9 | 5 | 195.8 | 12.08 | 2723.3 | 6.6% | pass (n=1) |
| zoe-native | 16 | 8.7 (8.7–8.7) | 1549 | 3998 | 4912 | 0.3% | 85.6% | 3.22 | 227.7 | 1 | 196.4 | 11.53 | 3106.6 | 7.2% | pass (n=1) |
| zoe-asyncio | 4 | 6.8 (6.8–6.8) | 233 | 1538 | 3137 | 3.2% | 82.3% | 3.28 | 342.8 | 9 | 220.5 | 12.23 | 2819.7 | 5.9% | pass (n=1) |
| zoe-asyncio | 16 | 7.8 (7.8–7.8) | 1686 | 3843 | 5159 | 1.0% | 80.8% | 3.26 | 281.2 | 3 | 212.0 | 11.44 | 3080.7 | 6.6% | pass (n=1) |
| zoe-stock | 4 | 7.0 (7.0–7.0) | 238 | 1918 | 3727 | 2.1% | 83.1% | 3.27 | 316.9 | 6 | 211.8 | 11.52 | 2687.2 | 5.8% | pass (n=1) |
| zoe-stock | 16 | 7.4 (7.4–7.4) | 1825 | 3760 | 6045 | 0.7% | 77.6% | 3.31 | 324.4 | 2 | 214.1 | 11.83 | 3076.1 | 6.2% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 187; server resolved [0], ZEO server resolver [3263], mean fsync batch ['–']
- zoe-native: IOBucket 170, IOBTree 2; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-asyncio: IOBucket 185; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-stock: IOBucket 184, IOBTree 2, <read conflict> 1; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

### contention-same

REST PATCH: every user writes the title of one of the hot Documents (incompatible edits of one field) (mix {'patch_same': 1}; interface rest)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 36.6 (36.6–36.6) | 27 | 31 | 44 | 1.9% | 98.0% | 1.06 | 79.0 | 29 | 23.8 | 3.47 | 521.1 | 2.8% | pass (n=1) |
| zeo | 16 | 38.0 (38.0–38.0) | 183 | 211 | 248 | 1.4% | 98.4% | 1.05 | 62.2 | 22 | 23.7 | 3.43 | 517.5 | 3.0% | pass (n=1) |
| zoe-native | 4 | 38.5 (38.5–38.5) | 26 | 32 | 46 | 1.8% | 97.8% | 1.06 | 77.2 | 28 | 18.8 | 5.63 | 1879.6 | 3.0% | pass (n=1) |
| zoe-native | 16 | 39.9 (39.9–39.9) | 176 | 201 | 2745 | 1.2% | 98.1% | 1.05 | 65.0 | 20 | 19.3 | 5.91 | 2108.0 | 3.1% | pass (n=1) |
| zoe-asyncio | 4 | 33.6 (33.6–33.6) | 29 | 38 | 54 | 2.3% | 97.7% | 1.07 | 91.5 | 31 | 23.3 | 5.46 | 1851.1 | 2.7% | pass (n=1) |
| zoe-asyncio | 16 | 34.4 (34.4–34.4) | 203 | 233 | 282 | 1.6% | 98.2% | 1.05 | 68.6 | 22 | 24.2 | 5.71 | 2110.7 | 2.8% | pass (n=1) |
| zoe-stock | 4 | 33.6 (33.6–33.6) | 29 | 37 | 47 | 1.9% | 98.0% | 1.06 | 78.2 | 26 | 23.2 | 5.40 | 1858.8 | 2.6% | pass (n=1) |
| zoe-stock | 16 | 34.2 (34.2–34.2) | 203 | 237 | 269 | 1.6% | 98.3% | 1.05 | 65.5 | 22 | 24.0 | 5.67 | 2112.8 | 2.7% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 212, IOBTree 1, IIBTree 1; server resolved [0], ZEO server resolver [4037], mean fsync batch ['–']
- zoe-native: IOBucket 226; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-asyncio: IOBucket 219, IIBTree 1, <read conflict> 1, IOBTree 1; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-stock: IOBucket 197, <read conflict> 1; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

### create-shared

the add form of Plone, every user in the one folder bench/inbox (mix {'create_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 4.5 (4.5–4.5) | 408 | 1868 | 4880~ | 3.2% | 68.4% | 3.46 | 524.1 | 6 | 322.1 | 18.58 | 935.6 | 5.0% | pass (n=1) |
| zeo | 16 | 4.2 (4.2–4.2) | 3723 | 5236 | 6193~ | 0.0% | 81.1% | 3.24 | 242.6 | 0 | 485.4 | 15.35 | 872.0 | 8.1% | pass (n=1) |
| zoe-native | 4 | 5.0 (5.0–5.0) | 424 | 2535 | 5584 | 0.5% | 72.4% | 3.42 | 433.5 | 1 | 299.3 | 25.68 | 4631.1 | 5.9% | pass (n=1) |
| zoe-native | 16 | 4.1 (4.1–4.1) | 3884 | 5637 | 6143~ | 0.6% | 77.4% | 3.30 | 317.1 | 1 | 479.6 | 22.37 | 4951.2 | 8.2% | pass (n=1) |
| zoe-asyncio | 4 | 4.6 (4.6–4.6) | 375 | 2557 | 4154~ | 4.1% | 74.6% | 3.40 | 487.0 | 8 | 322.1 | 25.12 | 4601.4 | 5.4% | pass (n=1) |
| zoe-asyncio | 16 | 4.0 (4.0–4.0) | 3660 | 5614 | 8193~ | 2.4% | 83.6% | 3.28 | 327.3 | 4 | 490.9 | 22.18 | 5042.7 | 8.0% | pass (n=1) |
| zoe-stock | 4 | 5.1 (5.1–5.1) | 358 | 2228 | 4205 | 2.9% | 81.3% | 3.32 | 373.2 | 6 | 320.5 | 21.99 | 4586.5 | 6.2% | pass (n=1) |
| zoe-stock | 16 | 3.9 (3.9–3.9) | 3910 | 5292 | 7464~ | 1.3% | 81.0% | 3.26 | 284.8 | 2 | 507.6 | 20.68 | 5097.4 | 8.0% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 139; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: IOBucket 67, <read conflict> 51, OOBTree 16, IOBTree 4, OIBTree 2; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-asyncio: IOBucket 78, <read conflict> 47, OOBTree 18, OIBTree 5; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']
- zoe-stock: <read conflict> 68, UniqueIdGeneratorTool 55; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']


## Background work

- Runs: 2; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

### mixed-background

the mixed scenario, with one reindex of SearchableText in a loop and one REST import in a loop beside it (mix {'browse_anon': 8, 'browse_member': 3, 'search': 4, 'edit': 2, 'create_spread': 1, 'workflow': 1, 'create_shared': 1}, background ['reindex', 'import']; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 38.7 (38.7–38.7) | 110 | 338 | 960 | 0.0% | 100.0% | 1.01 | 0.0 | 0 | 53.8 | 1.00 | 60.6 | 3.1% | pass (n=1) |
| zeo | 8 | 32.5 (32.5–32.5) | 197 | 823 | 1731 | 0.2% | 97.3% | 1.12 | 45.8 | 4 | 61.2 | 1.61 | 96.7 | 3.7% | pass (n=1) |
| zoe-native | 4 | 40.1 (40.1–40.1) | 109 | 222 | 933 | 0.0% | 100.0% | 1.01 | 0.0 | 0 | 51.0 | 2.01 | 556.4 | 3.3% | pass (n=1) |
| zoe-native | 8 | 33.9 (33.9–33.9) | 190 | 759 | 1755 | 0.2% | 97.3% | 1.12 | 40.1 | 3 | 59.3 | 2.47 | 553.7 | 4.0% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 49, IOBucket 32, IOBTree 1; server resolved [0], ZEO server resolver [714], mean fsync batch ['–']
- zoe-native: IOBucket 43, <read conflict> 27, OOBTree 4, OIBTree 1; server resolved [60], ZEO server resolver [None], mean fsync batch ['1.00']


## Cold start



- Runs: 2; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 0.0 s

### browse

rendered pages: folders, documents, news, files, breadcrumbs; anonymous and logged-in members (mix {'browse_anon': 1, 'browse_member': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 42.6 (42.6–42.6) | 92 | 119 | 143 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 48.2 | 0.12 | 0.1 | 3.4% | pass (n=1) |
| zoe-native | 4 | 43.0 (43.0–43.0) | 91 | 118 | 124 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 47.8 | 0.15 | 0.2 | 3.2% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']


#### Timeline, 10 s buckets

##### 20261016T004043Z-browse-zeo-r0

| from s | ok ops/s | p50 ms | p95 ms | failed | server CPU | Zope CPU | server MB written |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 37.8 | 96 | 127 | 0 | 2.2% | 203.3% | 0.1 |
| 10 | 42.4 | 92 | 118 | 0 | 0.4% | 207.8% | 0.0 |
| 20 | 42.9 | 91 | 117 | 0 | 0.2% | 208.0% | 0.0 |
| 30 | 42.6 | 91 | 118 | 0 | 0.1% | 207.8% | 0.0 |
| 40 | 43.0 | 91 | 118 | 0 | 0.1% | 207.6% | 0.0 |
| 50 | 43.2 | 91 | 118 | 0 | 0.1% | 207.7% | 0.0 |
| 60 | 3.5 | 87 | 114 | 0 | 1.8% | 0.0% | 0.0 |

##### 20261016T004153Z-browse-zoe-native-r0

| from s | ok ops/s | p50 ms | p95 ms | failed | server CPU | Zope CPU | server MB written |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 38.9 | 92 | 121 | 0 | 2.9% | 206.6% | 0.5 |
| 10 | 42.5 | 92 | 119 | 0 | 0.5% | 207.7% | 0.0 |
| 20 | 43.4 | 91 | 117 | 0 | 0.4% | 208.0% | 0.0 |
| 30 | 43.0 | 91 | 118 | 0 | 0.2% | 207.8% | 0.0 |
| 40 | 43.0 | 91 | 117 | 0 | 0.1% | 207.7% | 0.0 |
| 50 | 44.0 | 90 | 118 | 0 | 0.1% | 207.8% | 0.0 |
| 60 | 3.4 | 87 | 117 | 0 | 1.9% | 0.0% | 0.0 |



## Probe on



- Runs: 2; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

### browse

rendered pages: folders, documents, news, files, breadcrumbs; anonymous and logged-in members (mix {'browse_anon': 1, 'browse_member': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 44.0 (44.0–44.0) | 91 | 117 | 121 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 46.4 | 0.02 | 0.1 | 3.2% | pass (n=1) |
| zoe-native | 4 | 44.4 (44.4–44.4) | 90 | 117 | 120 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 44.8 | 0.03 | 0.0 | 3.3% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']


## Probe off



- Runs: 2; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': False, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

### browse

rendered pages: folders, documents, news, files, breadcrumbs; anonymous and logged-in members (mix {'browse_anon': 1, 'browse_member': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 4 | 43.9 (43.9–43.9) | 90 | 118 | 121 | 0.0% | 100.0% | 0.00 | 0.0 | 0 | 46.4 | 0.02 | 0.1 | 3.3% | pass (n=1) |
| zoe-native | 4 | 44.1 (44.1–44.1) | 91 | 116 | 120 | 0.0% | 100.0% | 0.00 | 0.0 | 0 | 45.1 | 0.03 | 0.0 | 3.2% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']


## Sustained writes



- Runs: 2; commit ee42ff82ddf9
- Machine: Apple M3 Pro, 12 logical CPUs, 36 GiB, kernel 27.0.0, cores {'Performance': 6, 'Efficiency': 6}
- CPU pinning: none: macOS has no CPU affinity
- Durability: zeo: F_FULLFSYNC: the harness makes os.fsync of the ZEO server use it; zoe: F_FULLFSYNC (Rust File::sync_data)
- Zope Python: {'version': '3.13.15', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True, 'zeo_fsync': 'full'}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

### create-spread

the add form of Plone, each user in folders of its own section (mix {'create_spread': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 8 | 5.1 (5.1–5.1) | 1096 | 3165 | 5285 | 3.6% | 76.8% | 3.37 | 442.4 | 113 | 328.0 | 16.51 | 923.6 | 5.8% | pass (n=1) |
| zoe-native | 8 | 5.6 (5.6–5.6) | 1083 | 3114 | 5456 | 1.5% | 77.0% | 3.34 | 374.0 | 51 | 301.8 | 23.89 | 5828.7 | 6.5% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 1389; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: <read conflict> 684, IOBucket 509, OOBTree 62, IOBTree 7, OIBTree 2, IITreeSet 1, IIBTree 1; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']


#### Timeline, 60 s buckets

##### 20261016T004950Z-create-spread-zeo-r0

| from s | ok ops/s | p50 ms | p95 ms | failed | server CPU | Zope CPU | server MB written |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 5.6 | 773 | 3252 | 12 | 9.3% | 143.3% | 285.4 |
| 60 | 5.6 | 881 | 3340 | 12 | 8.9% | 151.2% | 288.3 |
| 120 | 5.4 | 886 | 2997 | 19 | 8.8% | 156.9% | 288.9 |
| 180 | 5.3 | 935 | 3315 | 14 | 8.8% | 164.6% | 282.7 |
| 240 | 5.4 | 1009 | 2735 | 13 | 8.9% | 174.2% | 288.0 |
| 300 | 4.9 | 1137 | 3209 | 9 | 8.2% | 167.6% | 263.4 |
| 360 | 4.9 | 1128 | 2999 | 11 | 8.6% | 179.3% | 269.5 |
| 420 | 4.7 | 1221 | 3168 | 10 | 7.9% | 174.9% | 253.1 |
| 480 | 4.3 | 1348 | 3618 | 7 | 7.4% | 170.9% | 241.7 |
| 540 | 4.8 | 1241 | 3805 | 7 | 7.7% | 186.9% | 248.6 |
| 600 | 0.1 | 1126 | 2059 | 0 | 5.2% | 0.0% | 6.1 |

##### 20261016T010040Z-create-spread-zoe-native-r0

| from s | ok ops/s | p50 ms | p95 ms | failed | server CPU | Zope CPU | server MB written |
| ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| 0 | 6.3 | 704 | 3191 | 7 | 14.3% | 139.5% | 1697.7 |
| 60 | 6.6 | 778 | 3027 | 6 | 15.8% | 159.5% | 2053.4 |
| 120 | 6.1 | 872 | 2559 | 8 | 14.6% | 158.5% | 1955.2 |
| 180 | 5.7 | 984 | 3266 | 6 | 13.8% | 159.7% | 1945.8 |
| 240 | 5.7 | 979 | 2963 | 7 | 13.8% | 169.5% | 1964.5 |
| 300 | 5.6 | 1072 | 3082 | 4 | 13.5% | 180.8% | 1991.0 |
| 360 | 5.1 | 1167 | 3279 | 5 | 12.4% | 171.5% | 1766.1 |
| 420 | 5.3 | 1204 | 2760 | 4 | 12.8% | 187.6% | 1896.4 |
| 480 | 4.9 | 1298 | 3335 | 3 | 11.9% | 182.1% | 1792.6 |
| 540 | 4.7 | 1362 | 3555 | 1 | 11.5% | 181.4% | 1720.0 |
| 600 | 0.1 | 1160 | 1292 | 0 | 8.2% | 0.0% | 50.5 |


