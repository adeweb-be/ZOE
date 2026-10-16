# Plone end-to-end benchmark: ZEO and ZOE

- Runs: 14; commit 0f03dfbe869a
- Machine: Intel(R) Xeon(R) Processor @ 2.10GHz, 4 logical CPUs, 16 GiB, kernel 6.18.44-fc-v114
- Zope Python: {'version': '3.13.16', 'free_threaded': False, 'gil_enabled': True}
- Eggs: {'BTrees': '6.3', 'Plone': '6.1.5', 'Products.CMFPlone': '6.1.5', 'ZEO': '6.2', 'ZODB': '6.2', 'Zope': '5.14.2', 'msgpack': '1.1.2', 'persistent': '6.5', 'plone.restapi': '9.15.6', 'zodbpickle': '4.3'}; zoedb 0.0.0.dist; zoe 0.0.0; Locust 2.46.7
- Settings: {'instances': 2, 'threads': 2, 'zodb_cache_objects': 30000, 'pool_size': 7, 'client_cache': '128MB', 'server_cpus': '0', 'zope_cpus': ['1', '2'], 'locust_cpus': '3', 'base_port': 18080, 'server_port': 18100, 'admin_port': 18199, 'probe': True}
- Fixture: small-ba01ff255313, 685 objects, equivalent stores: True
- Workload model: closed: each user waits for its response before its next operation, so a slower server lowers the offered rate; think time 0,0; client retries 0; each step trimmed by 5.0 s

## browse

rendered pages: folders, documents, news, files, breadcrumbs; anonymous and logged-in members (mix {'browse_anon': 1, 'browse_member': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 10.6 (10.6–10.6) | 95 | 129 | 143 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 88.6 | 0.12 | 0.1 | 2.3% | pass (n=1) |
| zeo | 4 | 18.6 (18.6–18.6) | 220 | 314 | 350 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 103.8 | 0.09 | 0.0 | 3.7% | pass (n=1) |
| zeo | 16 | 19.5 (19.5–19.5) | 826 | 1135 | 1207 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 99.0 | 0.05 | 0.0 | 3.9% | pass (n=1) |
| zoe-native | 1 | 10.4 (10.4–10.4) | 96 | 128 | 142 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 90.3 | 0.12 | 0.1 | 2.2% | pass (n=1) |
| zoe-native | 4 | 18.4 (18.4–18.4) | 217 | 312 | 368 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 104.3 | 0.10 | 0.0 | 3.8% | pass (n=1) |
| zoe-native | 16 | 20.3 (20.3–20.3) | 801 | 1074 | 1203 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 95.2 | 0.06 | 0.0 | 4.0% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## catalog

@@search with text, filters, sorting and pages; collections; REST @search with metadata (mix {'search': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 8.9 (8.9–8.9) | 118 | 148 | 163 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 105.7 | 0.03 | 0.0 | 2.0% | pass (n=1) |
| zeo | 4 | 17.6 (17.6–17.6) | 240 | 312 | 357 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 106.7 | 0.03 | 0.0 | 3.5% | pass (n=1) |
| zeo | 16 | 18.1 (18.1–18.1) | 916 | 1101 | 1198 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 106.7 | 0.03 | 0.0 | 3.7% | pass (n=1) |
| zoe-native | 1 | 8.4 (8.4–8.4) | 119 | 173 | 188 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 111.8 | 0.03 | 0.0 | 1.9% | pass (n=1) |
| zoe-native | 4 | 17.5 (17.5–17.5) | 236 | 318 | 376 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 109.9 | 0.03 | 0.0 | 3.4% | pass (n=1) |
| zoe-native | 16 | 17.9 (17.9–17.9) | 927 | 1110 | 1233 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 107.4 | 0.00 | 0.0 | 3.6% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: none; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: none; server resolved [1], ZEO server resolver [None], mean fsync batch ['1.00']

## create-shared

the add form of Plone, every user in the one folder bench/inbox (mix {'create_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 2.0 (2.0–2.0) | 497 | 639 | 681~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 456.4 | 14.75 | 671.5 | 3.8% | pass (n=1) |
| zeo | 4 | 2.2 (2.2–2.2) | 1222 | 3740 | 5381~ | 3.3% | 63.3% | 3.52 | 588.9 | 3 | 808.3 | 36.67 | 914.8 | 6.0% | pass (n=1) |
| zeo | 16 | 2.0 (2.0–2.0) | 7588 | 9373 | 12024~ | 1.2% | 73.5% | 3.39 | 409.6 | 1 | 920.0 | 28.29 | 841.9 | 6.4% | pass (n=1) |
| zoe-native | 1 | 2.0 (2.0–2.0) | 523 | 644 | 667~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 457.3 | 16.33 | 5121.8 | 3.8% | pass (n=1) |
| zoe-native | 4 | 2.4 (2.4–2.4) | 1399 | 2538 | 3531~ | 2.1% | 66.7% | 3.46 | 500.0 | 2 | 763.3 | 35.43 | 2312.9 | 6.2% | pass (n=1) |
| zoe-native | 16 | 2.1 (2.1–2.1) | 7468 | 9242 | 10627~ | 0.0% | 83.1% | 3.24 | 241.0 | 0 | 903.4 | 29.76 | 1909.5 | 7.0% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 87; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: <read conflict> 28, IOBucket 25, OOBTree 7, IOBTree 5, OIBTree 3; server resolved [1], ZEO server resolver [None], mean fsync batch ['1.00']

## create-spread

the add form of Plone, each user in folders of its own section (mix {'create_spread': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 2.1 (2.1–2.1) | 473 | 576 | 600~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 435.1 | 14.29 | 641.2 | 3.5% | pass (n=1) |
| zeo | 4 | 2.5 (2.5–2.5) | 722 | 4470 | 5360~ | 4.7% | 64.2% | 3.59 | 688.7 | 5 | 655.0 | 44.95 | 1053.1 | 4.8% | pass (n=1) |
| zeo | 16 | 3.2 (3.2–3.2) | 4458 | 6924 | 9926~ | 3.0% | 76.9% | 3.40 | 462.7 | 4 | 554.2 | 28.46 | 851.0 | 5.9% | pass (n=1) |
| zoe-native | 1 | 2.0 (2.0–2.0) | 485 | 610 | 657~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 447.4 | 16.42 | 5218.5 | 3.7% | pass (n=1) |
| zoe-native | 4 | 2.8 (2.8–2.8) | 633 | 3230 | 5866~ | 5.1% | 65.8% | 3.53 | 632.5 | 6 | 579.7 | 40.54 | 2571.6 | 5.2% | pass (n=1) |
| zoe-native | 16 | 3.2 (3.2–3.2) | 4397 | 7881 | 8974~ | 2.3% | 77.4% | 3.35 | 391.0 | 3 | 537.8 | 29.15 | 1729.5 | 5.9% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: UniqueIdGeneratorTool 135; server resolved [0], ZEO server resolver [0], mean fsync batch ['–']
- zoe-native: IOBucket 65, <read conflict> 40, OOBTree 12, IOBTree 6, OIBTree 3; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## edit

the edit form of Plone on Documents, each user its own Documents (mix {'edit': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 2.2 (2.2–2.2) | 449 | 559 | 595~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 420.1 | 8.39 | 332.6 | 3.8% | pass (n=1) |
| zeo | 4 | 3.3 (3.3–3.3) | 718 | 2062 | 3888~ | 3.6% | 65.2% | 3.45 | 521.7 | 5 | 511.8 | 19.02 | 461.4 | 5.9% | pass (n=1) |
| zeo | 16 | 3.5 (3.5–3.5) | 4017 | 5843 | 7180~ | 16.9% | 60.2% | 3.18 | 361.4 | 2 | 532.0 | 18.55 | 426.6 | 6.9% | pass (n=1) |
| zoe-native | 1 | 2.2 (2.2–2.2) | 455 | 566 | 640~ | 0.0% | 100.0% | 3.00 | 0.0 | 0 | 417.6 | 9.20 | 4299.8 | 3.8% | pass (n=1) |
| zoe-native | 4 | 3.4 (3.4–3.4) | 746 | 3357 | 4957~ | 1.4% | 67.4% | 3.45 | 478.3 | 2 | 497.0 | 19.78 | 2660.9 | 6.3% | pass (n=1) |
| zoe-native | 16 | 3.8 (3.8–3.8) | 3719 | 5977 | 6925~ | 15.3% | 64.4% | 3.17 | 322.0 | 0 | 473.0 | 18.40 | 2233.5 | 7.1% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 125, IOBTree 4, OOBTree 3; server resolved [0], ZEO server resolver [2588], mean fsync batch ['–']
- zoe-native: IOBucket 104, <read conflict> 15, IOBTree 4; server resolved [0], ZEO server resolver [None], mean fsync batch ['1.00']

## mixed

a mix of interactive traffic, weights in the mix (mix {'browse_anon': 8, 'browse_member': 3, 'search': 4, 'edit': 2, 'create_spread': 1, 'workflow': 1, 'create_shared': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 10.9 (10.9–10.9) | 93 | 122 | 136 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 86.3 | 0.11 | 0.0 | 2.3% | pass (n=1) |
| zeo | 4 | 19.6 (19.6–19.6) | 212 | 281 | 320 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 98.2 | 0.11 | 0.1 | 4.0% | pass (n=1) |
| zeo | 16 | 13.1 (13.1–13.1) | 1010 | 3226 | 4054 | 0.0% | 98.5% | 1.24 | 19.1 | 0 | 146.9 | 3.00 | 51.6 | 5.0% | pass (n=1) |
| zoe-native | 1 | 10.2 (10.2–10.2) | 96 | 142 | 156 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 92.1 | 0.10 | 0.0 | 2.2% | pass (n=1) |
| zoe-native | 4 | 19.3 (19.3–19.3) | 212 | 302 | 367 | 0.0% | 100.0% | 1.00 | 0.0 | 0 | 99.9 | 0.12 | 0.0 | 4.1% | pass (n=1) |
| zoe-native | 16 | 14.3 (14.3–14.3) | 931 | 3102 | 3550 | 0.0% | 98.1% | 1.25 | 19.3 | 0 | 135.3 | 2.98 | 544.8 | 5.2% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: IOBucket 6, UniqueIdGeneratorTool 2, IOBTree 1, OOBTree 1; server resolved [0], ZEO server resolver [161], mean fsync batch ['–']
- zoe-native: IOBucket 9, <read conflict> 2; server resolved [84], ZEO server resolver [None], mean fsync batch ['1.00']

## workflow

publish and retract through the workflow menu, with an anonymous visibility check (mix {'workflow': 1}; interface ui)

| stack | users | ok ops/s (min–max) | p50 ms | p95 ms | p99 ms | failed | 1st-try ok | srv att/op | conflicts/1k | exhausted | Zope CPU ms/op | server CPU ms/op | server KB written/op | Locust CPU | verified |
| --- | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |
| zeo | 1 | 3.8 (3.8–3.8) | 328 | 450 | 481~ | 0.0% | 100.0% | 2.58 | 0.0 | 0 | 241.7 | 1.84 | 36.7 | 4.0% | pass (n=1) |
| zeo | 4 | 7.1 (7.1–7.1) | 685 | 904 | 981 | 0.0% | 100.0% | 2.71 | 0.0 | 0 | 269.1 | 3.65 | 40.2 | 7.9% | pass (n=1) |
| zeo | 16 | 7.4 (7.4–7.4) | 2462 | 2988 | 3234 | 0.0% | 99.7% | 2.68 | 3.4 | 0 | 259.7 | 3.47 | 41.8 | 8.2% | pass (n=1) |
| zoe-native | 1 | 3.9 (3.9–3.9) | 313 | 437 | 467~ | 0.0% | 100.0% | 2.59 | 0.0 | 0 | 234.6 | 1.86 | 1079.3 | 4.2% | pass (n=1) |
| zoe-native | 4 | 7.2 (7.2–7.2) | 682 | 921 | 1018 | 0.0% | 99.7% | 2.71 | 3.4 | 0 | 264.7 | 3.76 | 861.8 | 8.1% | pass (n=1) |
| zoe-native | 16 | 7.3 (7.3–7.3) | 2484 | 3055 | 3286 | 0.0% | 100.0% | 2.68 | 0.0 | 0 | 263.5 | 3.73 | 936.2 | 8.1% | pass (n=1) |

Conflict classes (all steps, all repetitions):

- zeo: <read conflict> 1; server resolved [0], ZEO server resolver [1266], mean fsync batch ['–']
- zoe-native: IOBTree 1; server resolved [1395], ZEO server resolver [None], mean fsync batch ['1.00']

