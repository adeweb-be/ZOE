# 0061. The security index: the inherited inputs of `allowedRolesAndUsers`

Status: accepted. Card ZOE-285 (finding: `docs/spikes/security-index.md`); the build is ZOE-301. This record adds a public Python API to `zoe.plone`: the index class `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security` (`install()`, `uninstall()`, `installed()`). It changes what Plone writes for a change of security. It changes no wire message and no on-disk format.

## Context

F33 asks for "inherited allowedRolesAndUsers as parent pointers, resolved at query time", accepted when "security bitmaps equal to reindexObjectSecurity on the corpus; a local-role change on a folder with 50,000 descendants writes O(1) records". The specification also says "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other" ("Step 2", and the invariants of `crates/zoe-catalog/CLAUDE.md`). The decision Q10 of the plan of phase 6 chose an index class of `zoe.plone` with "inherit", resolved at query time in the mirror and in the Python path. The finding of ZOE-285 gives the form of that choice.

The facts of the finding:

- The `allowedRolesAndUsers` indexer of CMFPlone computes the value of an object from two inputs. This record calls its result the Plone value. The roles of the permission "Access contents information" come from `rolesForPermissionOn`: the setting of the object, then of each container while the setting is a list or absent. The local roles come from `_getAllLocalRoles`: the providers of borg.localrole on the object and on each container, up to a block. A stock site has one provider, the default adapter, which reads `__ac_local_roles__`.
- `reindexObjectSecurity` of CMFCore reindexes `allowedRolesAndUsers` of every catalog entry below the object. On a folder of 50,004 descendants in the shape of `xlarge`, a share takes 66.7 s and writes 1,852 records (2.65 MB). A transition takes 59.1 s and writes 18 records. With queued indexing, the save writes 50,005 keys for the share and 50,006 for the transition (7.0 MB each), and the Indexer takes 120 to 128 s in 501 commits.
- "Own value or inherit" (option (a) of the card, the value whole) does not give O(1). A share changes the value of the 13,875 private and pending Documents below the folder, which do not hold the value of their parent. A transition makes 36,130 catalog entries write their own value: the folder, and the entries that held its value by chance.
- Each input alone does inherit: a share changes the local roles of the folder only, and a transition changes the permission setting of the folder only. The value of every other catalog entry follows from its own inputs and from those of its containers. On `xlarge-mini` and on the folder of 50,004 descendants, the value that the prototype resolves equals the Plone value for every catalog entry, before and after five changes, and on `xlarge-mini` after two changes that make opaque entries. A share then takes 0.03 s and writes 4 records, a transition 0.02 s and 10 records, and with queued indexing each writes one key.

## Decision

### Decisions of the human

1. The index inherits each input: the setting of "Access contents information" and the local roles. It does not inherit the value whole (sections 1 and 2).
2. `zoe.plone` replaces `CatalogAware.reindexObjectSecurity` and `MembershipTool.deleteLocalRoles` of CMFCore at run time (section 3). The code of CMFCore does not change.
3. The public Python API is the index class `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security`.
4. An add-on that changes the inputs below a folder through its own walk leaves stale entries (Consequences).
5. The new wording of "Step 2" and of the invariant of `zoe-catalog` stands (section 8).
6. The mirror resolves at the version of a query. The change sets hold inputs, not resolved values (section 6). F33 takes the wording of section 8.
7. The index goes into a catalog through the backend `security`, with `install()`, `uninstall()` and `installed()`, as the other backends of `zoe.plone`. `replace_index` and `restore_index` stay private, and a site created later gets the index in the transaction that creates it (section 5).
8. The Python path is slower than a `KeywordIndex` until ZOE-301 adds its cache. This record accepts that limit (Consequences).

### 1. The index class

`ZoeSecurityIndex` takes the place of the `KeywordIndex` `allowedRolesAndUsers`. For each rid, it keeps the inputs of the object of the catalog entry, which the object holds itself:

- the setting of "Access contents information", as `rolesForPermissionOn` reads it: none (the roles of the container), a tuple (these roles only), another sequence, such as a list or a `PersistentList` (these roles and the roles of the container; an empty one counts as none), or `None` (Anonymous);
- the local roles that the default adapter of borg.localrole gives for the object alone;
- the block flag `__ac_local_roles_block__`.

Each indexing of an object also records the inputs of its containers outside the catalog, up to the first catalog entry: the Zope root, a `++conversation++default`, a container whose own indexing failed. `clear()` of the index (`manage_catalogClear`, `clearFindAndRebuild`) removes these records. The indexing that follows records them again.

A catalog entry is an opaque entry when the index cannot read one of its inputs:

- the object or a container has a setting that is a str (another permission, or the empty str, which gives no roles), a local role provider other than the default adapter, an `IWorkspace` adapter of borg.localrole, or a callable `__ac_local_roles__`;
- plone.indexer finds an `allowedRolesAndUsers` indexer for the object other than the one of CMFPlone;
- the `acl_users` that the indexer finds for the object does not have exactly one active `ILocalRolesPlugin`, the `WorkspaceLocalRoleManager` of borg.localrole.

An opaque entry keeps the value that the indexer gives, as a `KeywordIndex` does. So every catalog entry below an opaque entry is an opaque entry too.

A change of the inputs of an object writes the bucket of its rid. The index writes no forward map.

### 2. The value

The value of a catalog entry that is not opaque is the Plone value. The index computes it from the inputs of the object and of its containers, which it finds by the path of the entry:

- The roles: the setting of the object. A tuple stops. Another sequence adds its roles to the roles of the container. None takes the roles of the container. `None` gives Anonymous. With no tuple, no `None` and no sequence up to the root, the roles are `Manager`; with sequences only, they are the roles of the sequences.
- The local roles: the local roles of the object, added to those of its container unless the object blocks them.
- Anonymous among the roles gives `["Anonymous"]`; else Authenticated gives `["Authenticated"]`. Else the value is the roles and a token `user:<principal>` for each principal whose local roles meet the roles, `Owner` included. Then the token `Owner` goes.

An opaque entry gives the value that it keeps. A catalog entry gives no token when a container on its path is an opaque entry, or is neither a catalog entry nor a recorded container. The first case happens only when an object becomes opaque through a reindex that is not a security reindex. It ends when the security reindex of section 3 reaches the entry.

The mirror and the Python path resolve the same way. ZOE-301 has an acceptance test with a local role provider other than the default adapter and with a setting that is a str.

### 3. `reindexObjectSecurity`

`install()` replaces `reindexObjectSecurity` of `Products.CMFCore.CMFCatalogAware.CatalogAware` at run time, and keeps the stock method. With a `ZoeSecurityIndex` in the catalog, the method reindexes `allowedRolesAndUsers` of the object. Then it reindexes each opaque entry below the object, or every catalog entry below it when the object gives an opaque entry. It reads that from the object and from the entries of its containers, because a queue can still hold the reindex of the object. A container outside the catalog records its inputs in the index. The stock method runs when the catalog has no `ZoeSecurityIndex`, and when `_cmf_security_indexes` of the object names another index. A share or a transition of a folder then writes the entry of the folder.

One caller of Plone 6.1 relies on the walk of the stock method. `MembershipTool.deleteLocalRoles(obj, member_ids, reindex=1, recursive=1)` of CMFCore removes the local roles of the members from each object below `obj`. Then it calls `reindexObjectSecurity` of `obj` alone ("reindexObjectSecurity is always recursive"). The control panel of Plone and plone.restapi call it on the site root when they delete a user. `install()` also replaces this method: with a `ZoeSecurityIndex`, it reindexes each object whose local roles it removes.

### 4. Queued indexing

With a `ZoeSecurityIndex`, the Indexer of ADR 0047 does not add the catalog subtree of a security reindex (section 3 of ADR 0047): the catalog entries below resolve through the folder. A share or a transition writes one key.

### 5. The backend

`security` is an opt-in backend of `zoe.plone`, as `indexing` is: `ZOE_PLONE_BACKENDS=security` or `zoe.plone:security.zcml`, which call `install()`. It has `install()`, `uninstall()` and `installed()`, as the other backends of `zoe.plone`. A second call of `install()` or `uninstall()` does nothing. Every instance of the site needs the backend. An instance without it runs the stock walk: the result is correct, but it writes and waits as before.

- `install()` replaces the methods of section 3 in the process. It also puts a `ZoeSecurityIndex` in place of the `KeywordIndex` `allowedRolesAndUsers` into each catalog that does not hold one yet. It finds the sites as queued indexing does (ADR 0047), in the databases that the process has open and at each open of a database (`IDatabaseOpenedWithRoot`). Each catalog gets the index in a transaction of its own, which indexes every catalog entry: 36 to 42 s for 50,064 entries in the prototype. A thread does this work under the lease of the site (ADR 0047), so the open of a database does not wait. The thread writes the lease again, in a commit of its own, at least every 10 s while the replacement runs, so no other instance takes it. The queued indexer of the site waits while the thread holds the lease. A read-only storage gets no index. A storage without the namespaces of `zoe.kv` has no lease, and gets no index either. Until the catalog holds the index, each instance runs the stock methods, and the result stays correct.
- A site created later gets the index in the transaction that creates it.
- `uninstall()` gives the stock methods back in the process. For each catalog of the databases that the process has open, it puts the `KeywordIndex` back in a transaction of its own and indexes every catalog entry.
- `installed()` tells if the backend is installed in the process.
- The functions `replace_index(catalog)` and `restore_index(catalog)` of `zoe.plone.security` do the work for one catalog. They are private, and no public API.

A catalog without a `ZoeSecurityIndex` runs the stock methods (section 3).

The way back of a site: run `uninstall()` in one instance, then remove the option from every instance. The way back runs before zoedb leaves the environment: without the class `ZoeSecurityIndex`, the catalog does not load, and every search fails.

A GenericSetup import of `catalog.xml` keeps an index whose id exists. An import that purges the catalog puts the stock index back. An export writes the meta_type `ZoeSecurityIndex`, which a site without zoedb cannot import.

The index holds persistent objects only, so it runs on a `DemoStorage`. ZOE-301 names the suites that run with the index in the catalog of the test fixture, as section 9 of ADR 0047 does for queued indexing. The other suites keep the stock index and run the stock methods.

### 6. The mirror

The mirror decodes the buckets of the inputs. It keeps the inputs of each node: each catalog entry and each recorded container. It resolves the value of each catalog entry and keeps one bitmap for each distinct value. A query of the tokens of a user is an or of the bitmaps whose value holds one of them.

ZOE-301 adds `ZoeSecurityIndex` to the classes that the mirror serves (ADR 0059, section 7). Its clause on the wire is the clause of a KeywordIndex of ADR 0059, section 8: `{"k": "v", "i": index, "q": [token...], "and": false, "r": nil, "not": []}`. Until then, a query with the index takes the Python path (`index`).

The mirror resolves at the version of a query, with the versions of ADR 0059:

- The base holds the inputs of each node, the value of each catalog entry and the bitmaps of the distinct values.
- A change set holds the changed inputs of a commit, one row for each changed node. It holds no resolved value.
- A query at tid `t` resolves, with the inputs at `t`, the catalog entries below the nodes that the change sets after the base and up to `t` changed. It puts them over the bitmaps of the base. The transaction overlay does the same, for its query only.
- For the rebase, the overrides count the catalog entries below their changed nodes. So a share on the folder of 50,004 descendants passes `catalog-rebase-rows`, and the rebase resolves the tree into a new base. Until the rebase, each query at a later tid resolves those 50,004 catalog entries: 9.2 ms for 50,064 in the prototype.

### 7. The Python path

The Python path resolves the value of each candidate rid at query time, with a memo for each query. When ZCatalog applies other clauses first, the candidates are their result. Otherwise they are every catalog entry: 0.18 to 0.42 s for 50,064 entries in the prototype. ZOE-301 gives the Python path a cache for each process. Its key is the snapshot of the connection. A connection whose transaction changed the index resolves without the cache. The `_counter` of the index is not a key: a connection with changes that it did not commit can hold the count of another snapshot.

### 8. The specification

"Step 2" says: "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex`, or the `ZoeSecurityIndex` of `zoe.plone` (ADR 0061), which the mirror resolves as its Python path does." The invariant of `crates/zoe-catalog/CLAUDE.md` becomes: "Security stays in Plone: a `KeywordIndex` `allowedRolesAndUsers` is an index like any other, and a `ZoeSecurityIndex` resolves as its Python path does."

F33 says: "incremental security index (the inherited inputs of allowedRolesAndUsers, resolved through the containers of each entry: in the mirror at the version of the query, in the Python path at query time)". Its acceptance does not change.

## Consequences

- A share or a transition of a folder writes the entry of the folder, whatever the size of its subtree, and takes the time of one reindex.
- The value of every catalog entry equals the Plone value, also with queued indexing, where a new object below a folder made private resolves through the folder. This holds when each caller reindexes the objects whose inputs it changes, as the stock catalog also assumes, and when no object becomes opaque without a security reindex.
- A caller that changes the inputs of the objects below a folder, and then calls `reindexObjectSecurity` of the folder alone, leaves those catalog entries stale. In Plone 6.1, `deleteLocalRoles` is the one such caller (section 3). The other callers reindex each object whose inputs change: an ownership change, a workflow mapping update, a copy, a move. An add-on that relies on the walk needs the same care.
- A security reindex above an opaque entry reindexes the opaque entries below it, as the stock walk does. A stock site has no opaque entry.
- The corpus of the spike has no Owner local role, no sequence setting below the site root (no stock workflow of Plone 6.1 acquires "Access contents information"), no comment and no mount. Its only opaque entries come from two changes on `xlarge-mini`. A check on `xlarge` covers none of these cases. ZOE-301 adds a small synthetic fixture for each.
- The acceptance test of ZOE-301 compares, before and after changes of security: the resolved value of every catalog entry with the Plone value; the bitmap of each token in the mirror with the postings of a stock `KeywordIndex`; and the rids of queries. It runs on these fixtures and on `xlarge` (G5).
- The Python path is O(N) in its candidates: 0.18 to 0.42 s at 50,064 catalog entries, not run at one million. Every restricted search pays it while the mirror is `not_ready`, `lag`, `outside_window` or `dropped`. This record accepts the limit until the cache of ZOE-301.
- ZOE-301 changes the specification: "Step 2" and the invariant of `zoe-catalog` (section 8); the table of the surface of `zoe.plone`, with the backend `security` (`install()`, `uninstall()`, `installed()`) and `ZoeSecurityIndex`; the list of the opt-in backends of `zoe.plone`, with `zoe.plone:security.zcml`; the line of the Indexer in "`zoe.plone` and queued indexing", which adds no security subtree with the index (section 4); the row F33 (section 8); and the classes that the mirror serves, with `ZoeSecurityIndex` (section 6).

## Alternatives

- **"Own value or inherit", the value whole.** A share writes the private and pending entries below the folder, and a transition the entries that held its value by chance.
- **An opaque entry that keeps its inputs, with only its own value opaque.** A str setting makes the walk above it read another permission, and an `IWorkspace` adapter replaces the local roles of a principal instead of adding to them. The entries below such a node cannot resolve through its inputs.
- **Change sets that hold resolved values.** A share on the folder of 50,004 descendants then changes 13,875 rows, and two shares make a rebase.
- **Public functions that put the index in.** `replace_index(catalog)` and `restore_index(catalog)`, run from a script of `bin/instance run` once for each site, a site created later included. They add a public API beyond the backend.
- **A profile of `zoe.plone`.** An import step puts the index in, a site created later runs it when its profile depends on it, and an uninstall profile gives the way back. It is a second way to install, beside the backends.
- **The mirror only.** Plone writes as today: the walk of the subtree, its records, and one key for each entry with queued indexing. F33 asks for O(1) records.
- **No change of `reindexObjectSecurity`.** The index writes nothing for the entries that do not change, but the walk wakes every object of the subtree, and queued indexing writes one key for each.
- **A subtree operation in CMFCore.** A change of CMFCore, out of scope.
- **A stock `KeywordIndex` beside the new index.** It writes what the stock index writes.
