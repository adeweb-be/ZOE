# 0062. The path prefetch: one call loads the chain of a path

Status: proposed. Card ZOE-304 (finding: `docs/findings/path-prefetch.md`). The finding recommends that the build does not start now: see "Recommendation" there. If the human accepts this record, it adds a wire extension `load_path` to `zoe serve` and to the native client, a key `prefetch-paths` of `<zoeclient>` and a subscriber of `zoe.plone`. It changes no on-disk format and no public method of `zoe.zeo.ClientStorage`.

## Context

F33 asks for the "traversal prefetch of a path's chain from the path trie": on a cold client cache, the traversal of a path costs one round trip for the chain of its objects, instead of one round trip for each level.

The facts of the finding:

- The mirror of ADR 0059 has no trie. Its path index keeps the rows of each (component, level) and of each parent path, and it maps a path to a rid, not to an oid. It reads no record of a folder.
- A chain is serial. The oid of each link is in the record of its parent. The prefetch of the references of ZOE-273 ("Prefetch-then-touch") sends the load of a child when the reply of its parent arrives, which is about when ZODB asks for it. So ZOE-273 does not shorten a chain: a cold walk of a path of depth 5 through folders in the shape of `BTreeFolder2Base` waits for 10 to 15 round trips with the prefetch on, one fewer than with it off. ZOE-273 helps where the application touches several children of one record.
- A level of a Plone path is two to four records: the container, its `_tree` (an `OOBTree`), and the inner node and the bucket of `_tree` when the folder holds more than one bucket of children.
- Only a call that knows the path can load the chain in one round trip: the server reads each link from the record of its parent, without a round trip for each one.

## Decision

### 1. No containment index

The server resolves a path from the records at the tid of the call. It keeps no index of the containment of the oids: no key space, no `meta` key, no structure of the mirror. The records of a chain are few (2 to 4 per level), and the server reads each one from its page cache in microseconds. A containment index would need the decode of every record of a folder at each commit, and a rebuild after a pack or a restore, to save these few reads.

### 2. The extension `load_path`

A client that declares `load_path` at `register` may call:

    load_path(oid, names, tid) -> [count, [[oid, data, serial, end], ...]]

- `oid` is the object where the walk starts, `names` a list of at most 16 `str`, and `tid` the exclusive tid of the loads, as for `loadBefore`.
- The server walks the names in order. For each name, it reads the record of the current object before `tid` and finds the persistent reference of the child:
  1. a key of the state dict of the object, whose value is a persistent reference (`OFS.ObjectManager` keeps its subobjects as attributes, as the Zope root does);
  2. else the key `_tree` of the state dict, a reference to an `OOBTree` of `BTrees`: the server reads the tree record, then the inner nodes and the bucket that hold the name, as the lookup of `BTrees` does (`BTreeFolder2Base`, so the folders of `plone.folder` and the site of Plone 6).
- The reply holds every record that the walk read, in the order of the reads: the start object, and for each level the tree, its nodes and bucket, and the child. Each item gives the data, the serial and the end tid of the record before `tid`, or `nil` for a current record, as the reply of `loadBefore`. `count` is the number of names that the walk resolved.
- The walk stops at the first name that it cannot resolve: an absent key, a value that is not a persistent reference, a reference into another database, a record that the decoder refuses, a record of another class layout. It then replies with what it read. It never raises for such a stop.
- The reply holds at most 64 records and at most 1 MiB of data. The walk stops before a record that would pass a bound.
- The call runs on the read pool, as `loadBefore`. `zoe_loads_total` counts each record of the reply.

A server without the extension does not announce it, and the client then never sends the call. A stock ZEO client never sees it (F13).

### 3. The native client

- The pool sends `load_path` only when the cache does not hold the chain. First, it walks the chain in its own cache with the same rules as the server, through the decoder of the reader of ZOE (ADR 0057). When every link is in the cache, it sends nothing. Else it sends the call from the last object that it holds, with the names that remain.
- Each record of the reply goes into the cache through the store rule of ADR 0034, as the reply of a `loadBefore` before `tid`. The call only adds entries, so what a Connection reads does not change.
- The records of the reply do not start the prefetch of their references (ZOE-273): the prefetch stays one level deep.
- `prefetch_path(oid, names, tid)` of `zoe._zoe.Client` waits for the reply. It is private: the method of `zoe.zeo.ClientStorage` is `_prefetch_path`, which does nothing when the server did not accept the extension, when the client has no cache, or when the setting is off.

### 4. The trigger in `zoe.plone`

A subscriber of `ZPublisher.interfaces.IPubStart` (a GET or a HEAD) takes the path of the request, removes the names of the virtual host monster (`VirtualHostBase` and its two names, `VirtualHostRoot`, the names that start with `_vh_`), stops at the first name that starts with `@@` or `++`, and calls `_prefetch_path` with the oid of the Zope root, the names and the tid of the connection of the request. An error of the call logs one warning and changes nothing: traversal then loads as it does today.

### 5. The setting

The key `prefetch-paths` of `<zoeclient>`, else its twin `ZOE_PREFETCH_PATHS`, turns the call on. The default is a decision of the human (question 3 of the finding).

### 6. The acceptance test

`python/zoedb/tests/client/test_native_path_prefetch.py`:

- a traversal of a path of depth 5 (a chain of `BTreeFolder2`-like containers with `_tree`) through a native client with an empty cache makes one `load_path` call for the chain, no `loadBefore` for a link of the chain, and the server counts one load for each record of the chain;
- the same traversal with a warm cache sends nothing;
- with the setting off, the client sends the `loadBefore` calls of today;
- a name that does not resolve stops the walk with what it read, and traversal loads the rest as today;
- a commit that changes a link between the call and the traversal gives the new record to the Connection (the store rule).

## Consequences

- The server learns two layouts of Zope: an attribute of an `ObjectManager` and the `_tree` of `BTreeFolder2Base`. It decodes them with `zoe-pickle`, as the mirror decodes the trees of a catalog. Another layout stops the walk; it never gives a wrong record, because each record of the reply is a record of the store before `tid`.
- A cold request of Plone saves at most the round trips of its chain, about 2 per level. The finding measures that this is a small part of a cold request: most of its loads are not on the chain (the component registry, the tools, the catalog, the templates).
- A request with a warm chain pays the walk in the cache of the client. The finding does not measure that cost.

## Alternatives

1. **A containment index in the store.** A key space `(parent oid, name) -> child oid`, kept at each commit from the records of the folders. It is a format change, it needs the decode of the folders at each commit, and pack, restore and the converter must keep it. It saves the reads of the server that section 1 makes, which cost microseconds.
2. **A deeper prefetch of references in the client.** Scan the records that a prefetch brings when they are trees or buckets. It does not shorten a chain: each link still waits for the reply of its parent. It widens each level to every child of a folder.
3. **The path of the request sent with a load.** The client adds the path to the `loadBefore` of the root. The reply shape of `loadBefore` stays one record, so it needs a new reply shape anyway: the same call under another name, which a stock ZEO message cannot carry.
4. **No build.** Keep ZOE-273 and the persistent client cache file (ADR 0034). A restarted instance with its cache file is warm, so a cold chain happens after a loss of the cache or for a path that no request read before.
