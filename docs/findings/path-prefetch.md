# Finding: a prefetch of the chain of a path saves 2 to 5 % of a cold Plone request

Card ZOE-304. The card asked for a traversal prefetch of the chain of a path from the path index of the mirror. Decision: not built. F33 does not name it, and no ADR records a design.

**Verdict.** Do not build the path prefetch. A chain is serial: the oid of each link is in the record of its parent, so the prefetch of references of ZOE-273 cannot shorten it, and a cold walk of a path of depth 5 waits for 5 to 16 round trips (two or three for each level of folders) with ZOE-273 on and off alike. A call that loads the whole chain at once brings that to one round trip: on the walk alone, 4 to 7 times faster. But a cold Plone request loads 850 to 2,600 objects, and its chain is 10 to 12 of them. With the chain loaded at once by an oracle, a cold document view takes 2.6 % less time on localhost and 5.1 % less behind a round trip of about 0.8 ms; a folder listing 2.5 % and 4.0 %. In a warm site, as in B5 `browse`, the chain is in the client cache and nothing changes. The cost is a wire extension and server code that knows two layouts of Zope.

## The question

The card asks what a prefetch of a chain needs (a containment index of oids kept from the records of the folders, or the path of the request sent with a load), and its gain over the prefetch of references of ZOE-273 on the cold traversals of B5 `browse` and of the `traverse` workload of the client benchmark. It asks first for the round trips and the time of a cold traversal today.

## How it was measured

Two probes, outside the diff, on `main` at 76ee39fb. Release `zoe serve`, the native client of the zoeclient scenario of `plone/` on CPython 3.13, one machine (Intel Core Ultra 7 155H). Each probe wraps `loadBefore` of the storage and records the time of each call. A call slower than a threshold waited for the server: a miss, or a load that joined a prefetch in flight. This finding calls it a **wait**. The server counts its loads on the admin port (`zoe_loads_total`). A TCP proxy between the client and the server delays each chunk by a fixed time in each direction, to show a network: no delay (localhost, about 0.1 ms per wait), 250 µs (about 0.8 to 1 ms per wait, through the proxy) and 1 ms (about 2.6 to 3 ms per wait). The threshold is the delay of one direction, and at least 40 µs.

The **oracle** stands for a perfect chain prefetch: the probe first walks the path cold and records the oids that it loads. Then, from a cold cache, it sends all of them in one `prefetch` call (the loads go out at once on the pool), and walks. The clock covers the prefetch. A real call cannot do better than the oracle; it adds the reads of the server and the walk of the client in its own cache.

**The traverse probe.** A store in the shape of the `traverse` workload of the client benchmark (the root, 20 children for each node over 3 levels, 8,421 objects, 1 KB leaves), and three chains of depth 5 under the root: `deep`, of `PersistentMapping`s (20 children per level); `folders`, of mappings with an `OOBTree` `_tree` of 20 children, as `BTreeFolder2Base` keeps them; `big`, the same with 1,000 children per level, so each `_tree` has an inner node and 34 buckets. A fresh process walks once from an empty cache in memory: the whole tree (`full`, the walk of the client benchmark) or one path. `ZEO.ClientStorage`, the native client with `prefetch-references` on and off, and the oracle on both settings. 3 rounds, the medians.

**The Plone probe.** A copy of the zoe store of the `xlarge-mini` fixture of B5 (2,586 items, sections of depth 2 with folders below). One Zope process with the `<zoeclient>` of B5 (client cache of 128 MB in memory, object cache of 30,000), one thread. 15 published Folders and 15 published Documents, drawn as B5 `browse` draws its targets, 3 repeats each. Before each measure, the probe empties the client cache (`_cache.clear()`) and calls `cacheMinimize()`. It measures the chain (`unrestrictedTraverse` of the path, then the activation of the object) and the request (an anonymous GET through the WSGI publisher, all 200), each with and without the oracle of the chain. A first pass over the paths warms the templates and the imports.

## The numbers

### The traverse probe: one path of depth 5

Time of the walk in ms, the waits, and the loads that the server counted.

| chain | client | no delay | waits | 250 µs | 1 ms | server loads |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| `deep` | ZEO | 1.94 | 6 | 8.49 | 16.64 | 6 |
| | native, off | 0.97 | 6 | 8.04 | 18.67 | 6 |
| | native, on | 2.46 | 5 | 5.29 | 14.00 | 81 |
| | oracle, on | 0.44 | 0–1 | 1.40 | 3.24 | 5–24 |
| `folders` | ZEO | 3.02 | 11 | 11.33 | 29.72 | 11 |
| | native, off | 2.31 | 11 | 11.85 | 32.65 | 11 |
| | native, on | 4.00 | 10 | 10.14 | 25.90 | 106 |
| | oracle, on | 0.94 | 1–2 | 1.63 | 3.45 | 29 |
| `big` | ZEO | 4.21 | 16 | 16.34 | 43.72 | 16 |
| | native, off | 3.15 | 16 | 15.65 | 43.21 | 16 |
| | native, on | 9.54 | 15 | 15.36 | 37.14 | 416 |
| | oracle, on | 1.60 | 1 | 2.16 | 3.48 | 80–95 |

- A level of a folder costs two waits (the folder, then its `_tree`), and three in a large folder (the inner node and the bucket). ZOE-273 saves one wait of 11 to 16: the first child of the root, which the miss of the root prefetched.
- On localhost, ZOE-273 makes a cold chain 1.7 to 3 times slower: it loads the siblings of each link (416 loads for 16 records on `big`), and those loads hold the connections of the pool and the server while the chain waits. Behind a delay, it gains 5 to 30 %, one wait or a part of each.
- The oracle gives one wait: 4 to 7 times faster than today on localhost, 7 to 12 times behind a delay.

### The traverse probe: the whole tree

| client | no delay | waits | 250 µs | waits | 1 ms | waits |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| ZEO | 1,227 ms | 8,420 | 7,295 ms | 8,420 | 21,191 ms | 8,420 |
| native, off | 573 ms | 7,801 | 6,637 ms | 8,420 | 20,143 ms | 8,420 |
| native, on | 217 ms | 1,071 | 765 ms | 810 | 2,128 ms | 802 |

This is the workload that ZOE-273 serves: a walk that touches every child. It cuts the waits by 8 to 10 and the time by 2.6 on localhost and 9 behind a delay. A chain prefetch does nothing here: the walk has no path.

### The Plone probe: the chain and the request

The medians of 45 measures of each kind (15 paths, 3 repeats). A chain is 10 records for a Folder and 12 for a Document (the medians): the Zope root, the site, and about two records for each level, the folder and its `_tree`.

| kind | delay | setting | chain | chain, oracle | request | waits | request, oracle | waits | gain of the oracle |
| --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| Document | none | on | 2.55 ms | 1.15 ms | 305.3 ms | 903 | 297.4 ms | 885 | 2.6 % |
| Document | none | off | 2.28 ms | 1.14 ms | 402.2 ms | 2,474 | 371.8 ms | 2,463 | 7.5 % (spread) |
| Document | 250 µs | on | 11.17 ms | 1.82 ms | 973.7 ms | 889 | 924.2 ms | 859 | 5.1 % |
| Document | 250 µs | off | 11.52 ms | 1.88 ms | 2,274.8 ms | 2,604 | 2,233.4 ms | 2,592 | 1.8 % |
| Folder | none | on | 2.83 ms | 1.12 ms | 260.6 ms | 444 | 254.1 ms | 447 | 2.5 % |
| Folder | none | off | 2.10 ms | 1.01 ms | 270.0 ms | 832 | 277.7 ms | 833 | -2.8 % (spread) |
| Folder | 250 µs | on | 9.09 ms | 1.72 ms | 613.2 ms | 481 | 588.5 ms | 460 | 4.0 % |
| Folder | 250 µs | off | 9.19 ms | 1.88 ms | 865.3 ms | 832 | 861.9 ms | 835 | 0.4 % |

- A cold Document view loads 2,605 objects, a cold Folder listing about 845. The server serves 2,604 and 845 without ZOE-273, and 3,573 and 1,605 with it (the prefetched siblings).
- ZOE-273 already removes two thirds of the waits of a cold Document view (2,604 to 889 behind a delay, 2,275 to 974 ms) and 42 % of those of a Folder listing.
- The chain is 11 of the 889 waits that stay: 1.2 %. The other waits are the objects that the request reads beside its path: the component registry of the site, the tools, the catalog, the registry of `plone.registry`, the objects of the views.
- The oracle saves 6 to 50 ms of a cold request: 2.5 to 5.1 % with ZOE-273 on. A real call saves less.
- With the setting off on localhost, the two medians of a request differ by more than the waits of the chain explain (11 waits of about 0.1 ms): that is the spread of the requests, marked `(spread)`.

### B5 `browse`

The steady state of B5 `browse` is warm: the client cache of 128 MB holds the site, and the storage server uses under 1 % of a CPU (`docs/findings/plone-end-to-end.md`, items 1, 19 and 21). There a chain is in the cache, and a path prefetch changes nothing. A cold chain happens at the start of a process without a cache file, after a loss of the cache, or for a path that no request of the process read before. In the last case the upper levels of the path are warm, so the cold part is one or two levels, two to four records. A process that keeps its cache file (`client` and `var`, ADR 0034) starts warm.

## What a chain prefetch needs

- **No index of the mirror.** The path index of the mirror (ZOE-288) maps a path to a rid, keeps the rows of each (component, level) and of each parent path, and reads no record of a folder. It holds no oid of a container, and no oid of the `_tree` or of the buckets that lead to the next id.
- **Not a containment index either.** A key space of `(parent oid, name) -> child oid` would be a format change, need the decode of every record of a folder at each commit, and need pack, restore and the converter to keep it. It saves reads that cost microseconds: the server can walk the chain from the records themselves.
- **A call that carries the path.** The server reads the record of each link, decodes it with `zoe-pickle`, and finds the next one: an attribute of an `ObjectManager` (the Zope root keeps the site so), or a key of the `_tree` of `BTreeFolder2Base` (the site of Plone 6 and the folders of `plone.folder`), through its inner nodes and bucket. The reply holds every record that it read. That is a new extension of the wire protocol: the reply of `loadBefore` holds one record, so the path cannot ride on it.
- **A trigger.** ZODB does not know the path of a request. `zoe.plone` already subscribes to `IPubStart`; a subscriber would take the path of the request without the names of the virtual host monster and call the client before traversal starts.
- **A walk in the client cache first**, so a warm chain sends nothing: the client resolves the chain in its own cache with the decoder of the reader of ZOE, and sends the call only from the first link that it misses.

The design that follows is an extension `load_path`. It has no index and no format change.

## Options and their costs

| option | gain on a cold request | cost |
| --- | --- | --- |
| A. Build `load_path`: the extension, the walk in the client cache, the subscriber of `zoe.plone` | at most 2.5 to 5 % of a cold request, 0 on a warm one | a wire extension (a decision of the human); server code that knows two layouts of Zope; a walk in the client cache on each GET; tests in `zoe-server`, `zoe-client`, `zoe-py`, `zoe.plone` |
| B. A containment index in the store | the same as A | A, plus a format change, a decode of the folders at each commit, and pack, restore and the converter |
| C. A deeper prefetch of references in the client | none on a chain: each link still waits for its parent | more loads per level (all the children of each folder), already 1.7 to 3 times slower on a localhost chain |
| D. No build | none | none; a restart keeps its cache file where it matters |

## Recommendation

Option D: do not build the path prefetch, and close the card. The gain over ZOE-273 is one round trip per level of a cold path, which is 1 to 2 % of the waits of a cold Plone request and 2.5 to 5 % of its time, and nothing on a warm site. What makes a cold request slow is the hundreds of objects beside the path, which ZOE-273 already overlaps in part, and a process that keeps its client cache file does not start cold.

## Limits

- The oracle is an upper bound. A real `load_path` adds the reads and decodes of the server and the walk of the client in its cache; neither is measured.
- The delays come from a proxy in Python. The time per wait behind it (0.8 to 3 ms) is more than the delay of the proxy, so the absolute times behind it are high; the counts of waits and the ratios hold.
- One machine, one fixture of B5 (`xlarge-mini`), 15 paths of each kind. The fixture is shallow: its documents sit at depth 3 to 5 below the site. A deeper site gives the chain a larger share, two records per level.
- A cold request was measured in one thread. Several cold requests at once share the pool of the client and the server.

## Decision

- The path prefetch is not built. F33 and the roadmap row of phase 6 do not name it, the feature triage lists it as Won't, and the card ZOE-304 is closed.
- ZOE-273 stays as it is. It makes a cold chain through large folders slower on localhost, because it loads the siblings. Its gain on a fan-out (the whole tree, the cold request) outweighs that, so the prefetch of a bucket gets no cap.
