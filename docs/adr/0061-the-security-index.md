# 0061. The security index: the inherited inputs of `allowedRolesAndUsers`

Status: proposed. Card ZOE-285 (finding: `docs/spikes/security-index.md`); the build is ZOE-301. This record adds a public Python API to `zoe.plone`: the index class `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security` (`install()`, `uninstall()`, `installed()`). It changes what Plone writes for a change of security. It changes no wire message and no on-disk format.

## Context

F33 asks for "inherited allowedRolesAndUsers as parent pointers, resolved at query time", accepted when "security bitmaps equal to reindexObjectSecurity on the corpus; a local-role change on a folder with 50,000 descendants writes O(1) records". The specification also says "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other" ("Step 2", and the invariants of `crates/zoe-catalog/CLAUDE.md`). The decision Q10 of the plan of phase 6 chose an index class of `zoe.plone` with "inherit", resolved at query time in the mirror and in the Python path, to confirm on the finding of ZOE-285.

The facts of the finding:

- The `allowedRolesAndUsers` indexer of CMFPlone computes the value of an object from two inputs. This record calls its result the Plone value. The roles of the permission "Access contents information" come from `rolesForPermissionOn`: the setting of the object, then of each container while the setting is a list or absent. The local roles come from `_getAllLocalRoles`: the providers of borg.localrole on the object and on each container, up to a block. A stock site has one provider, the default adapter, which reads `__ac_local_roles__`.
- `reindexObjectSecurity` of CMFCore reindexes `allowedRolesAndUsers` of every catalog entry below the object. On a folder of 50,004 descendants in the shape of `xlarge`, a share takes 66.7 s and writes 1,852 records (2.65 MB). A transition takes 59.1 s and writes 18 records. With queued indexing, the save writes 50,005 keys for the share and 50,006 for the transition (7.0 MB each), and the Indexer takes 120 to 128 s in 501 commits.
- "Own value or inherit" (option (a) of the card, the value whole) does not give O(1). A share changes the value of the 13,875 private and pending Documents below the folder, which do not hold the value of their parent. A transition makes 36,130 catalog entries write their own value: the folder, and the entries that held its value by chance.
- Each input alone does inherit: a share changes the local roles of the folder only, and a transition changes the permission setting of the folder only. The value of every other catalog entry follows from its own inputs and from those of its containers. On `xlarge-mini` and on the folder of 50,004 descendants, the value that the prototype resolves equals the Plone value for every catalog entry, before and after five changes, and on `xlarge-mini` after two changes that make opaque entries. A share then takes 0.03 s and writes 4 records, a transition 0.02 s and 10 records, and with queued indexing each writes one key.

## Decision

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

`security` is an opt-in backend of `zoe.plone`, as `indexing` is: `ZOE_PLONE_BACKENDS=security` or `zoe.plone:security.zcml`. `install()` replaces the methods of section 3 in the process, and `uninstall()` gives the stock methods back. Every instance of the site needs the backend. An instance without it runs the stock walk: the result is correct, but it writes and waits as before.

The index goes into each catalog in a transaction of its own, which indexes every catalog entry: 36 to 42 s for 50,064 entries in the prototype. The call that does it, also for a site created later, is a point for the human. A site that keeps the stock index runs the stock methods.

The way back of a site puts the `KeywordIndex` back and indexes every catalog entry. Then the option leaves every instance. The way back runs before zoedb leaves the environment: without the class `ZoeSecurityIndex`, the catalog does not load, and every search fails.

A GenericSetup import of `catalog.xml` keeps an index whose id exists. An import that purges the catalog puts the stock index back. An export writes the meta_type `ZoeSecurityIndex`, which a site without zoedb cannot import.

The index holds persistent objects only, so it runs on a `DemoStorage`. The Plone suites with the backend and the stock index run the stock methods. ZOE-301 names the suites that run with the index in the catalog of the test fixture, as section 9 of ADR 0047 does for queued indexing.

### 6. The mirror

The mirror decodes the buckets of the inputs, and keeps the inputs of each node of the path trie (ZOE-288). It resolves the value of each catalog entry and keeps one bitmap for each distinct value. A query of the tokens of a user is an or of the bitmaps whose value holds one of them. How the resolved values follow the versions and the rebase of ADR 0059 is a point for the human.

### 7. The Python path

The Python path resolves the value of each candidate rid at query time, with a memo for each query. When ZCatalog applies other clauses first, the candidates are their result. Otherwise they are every catalog entry: 0.18 to 0.42 s for 50,064 entries in the prototype. ZOE-301 gives the Python path a cache for each process. Its key is the snapshot of the connection. A connection whose transaction changed the index resolves without the cache. The `_counter` of the index is not a key: a connection with changes that it did not commit can hold the count of another snapshot.

### 8. The specification

"Step 2" says: "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex`, or the `ZoeSecurityIndex` of `zoe.plone` (ADR 0061), which the mirror resolves as its Python path does." The invariant of `crates/zoe-catalog/CLAUDE.md` becomes: "Security stays in Plone: a `KeywordIndex` `allowedRolesAndUsers` is an index like any other, and a `ZoeSecurityIndex` resolves as its Python path does."

## Consequences

- A share or a transition of a folder writes the entry of the folder, whatever the size of its subtree, and takes the time of one reindex.
- The value of every catalog entry equals the Plone value, also with queued indexing, where a new object below a folder made private resolves through the folder. This holds when each caller reindexes the objects whose inputs it changes, as the stock catalog also assumes, and when no object becomes opaque without a security reindex.
- A caller that changes the inputs of the objects below a folder, and then calls `reindexObjectSecurity` of the folder alone, leaves those catalog entries stale. In Plone 6.1, `deleteLocalRoles` is the one such caller (section 3). The other callers reindex each object whose inputs change: an ownership change, a workflow mapping update, a copy, a move. An add-on that relies on the walk needs the same care.
- A security reindex above an opaque entry reindexes the opaque entries below it, as the stock walk does. A stock site has no opaque entry.
- The corpus of the spike has no Owner local role, no sequence setting below the site root (no stock workflow of Plone 6.1 acquires "Access contents information"), no comment and no mount. Its only opaque entries come from two changes on `xlarge-mini`. A check on `xlarge` covers none of these cases. ZOE-301 adds a small synthetic fixture for each.
- The acceptance test of ZOE-301 compares, before and after changes of security: the resolved value of every catalog entry with the Plone value; the bitmap of each token in the mirror with the postings of a stock `KeywordIndex`; and the rids of queries. It runs on these fixtures and on `xlarge` (G5).
- The Python path is O(N) in its candidates: 0.18 to 0.42 s at 50,064 catalog entries, not run at one million. Every restricted search pays it while the mirror is `not_ready`, `lag`, `outside_window` or `dropped`.
- ZOE-301 changes the specification: "Step 2" and the invariant of `zoe-catalog` (section 8); the table of the surface of `zoe.plone`, with the backend `security` and `ZoeSecurityIndex`; the list of the opt-in backends of `zoe.plone`, with `zoe.plone:security.zcml`; the line of the Indexer in "`zoe.plone` and queued indexing", which adds no security subtree with the index (section 4); and F33, if the human takes the wording below.

## Points for the human

Accepted:

- Inherit per input (the permission setting and the local roles), not the value whole: the form that Q10 (a) takes.
- `zoe.plone` replaces two methods of CMFCore at run time (`CatalogAware.reindexObjectSecurity`, `MembershipTool.deleteLocalRoles`): no change of the code of CMFCore, but a patch.
- The public Python API: `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security`.
- An add-on that changes the inputs of the objects below a folder and relies on the walk leaves stale entries.
- The new wording of "Step 2" and of the invariant of `zoe-catalog` (section 8).

Open:

- **The mirror resolves at the version of a query.** Q10 and F33 say "resolved at query time", and section 6 resolves in the mirror. The proposal: the base of ADR 0059 holds the inputs of each node, the value of each catalog entry and the bitmaps of the distinct values. A change set holds the changed inputs of a commit, one row for each node. A query at tid `t` resolves, with the inputs at `t`, the catalog entries below the nodes that the change sets up to `t` changed, and puts them over the bitmaps of the base. The transaction overlay does the same, for its query only. For the rebase, the overrides count the catalog entries below their changed nodes. So a share on the folder of 50,004 descendants passes `catalog-rebase-rows`, and the rebase resolves the tree into a new base. Until the rebase, each query at a later tid resolves those 50,004 catalog entries (9.2 ms for 50,064 in the prototype). F33 then says: "incremental security index (the inherited inputs of allowedRolesAndUsers, resolved through the containers of each entry: in the mirror at the version of the query, in the Python path at query time)". The other choice: the change sets hold resolved values. A share then changes 13,875 rows, and two shares make a rebase.
- **The call that puts the index into a catalog.** (a) `replace_index(catalog)` and `restore_index(catalog)` of `zoe.plone.security`, as in the prototype: public functions beyond the accepted API, run from a script of `bin/instance run`, once for each site, a site created later included. (b) A profile of `zoe.plone`: an import step that puts the index in, which a site created later runs when its profile depends on it, and an uninstall profile for the way back.
- The Python path resolves at query time, slower than a `KeywordIndex` until ZOE-301 adds its cache.

## Alternatives

- **"Own value or inherit", the value whole.** A share writes the private and pending entries below the folder, and a transition the entries that held its value by chance.
- **An opaque entry that keeps its inputs, with only its own value opaque.** A str setting makes the walk above it read another permission, and an `IWorkspace` adapter replaces the local roles of a principal instead of adding to them. The entries below such a node cannot resolve through its inputs.
- **The mirror only.** Plone writes as today: the walk of the subtree, its records, and one key for each entry with queued indexing. F33 asks for O(1) records.
- **No change of `reindexObjectSecurity`.** The index writes nothing for the entries that do not change, but the walk wakes every object of the subtree, and queued indexing writes one key for each.
- **A subtree operation in CMFCore.** A change of CMFCore, out of scope.
- **A stock `KeywordIndex` beside the new index.** It writes what the stock index writes.
