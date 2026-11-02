# 0061. The security index: `allowedRolesAndUsers` keeps the inputs of each object and resolves them through its containers

Status: proposed. Card ZOE-285 (finding: `docs/spikes/security-index.md`); the build is ZOE-301. This record adds a public Python API to `zoe.plone`: the index class `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security` (`install()`, `uninstall()`, `installed()`). It changes what Plone writes for a change of security. It changes no wire message and no on-disk format.

## Context

F33 asks for "inherited allowedRolesAndUsers as parent pointers, resolved at query time", accepted when "security bitmaps equal to reindexObjectSecurity on the corpus; a local-role change on a folder with 50,000 descendants writes O(1) records". The specification also says "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other" ("Step 2", and the invariants of `crates/zoe-catalog/CLAUDE.md`). The decision Q10 of the plan of phase 6 chose an index class of `zoe.plone` with "inherit", resolved at query time in the mirror and in the Python path, to confirm on the finding of ZOE-285.

The facts of the finding:

- The indexer of Plone 6.1 computes the value of an object from two inputs. The roles of the permission "Access contents information" come from `rolesForPermissionOn`: the setting of the object, then of each container while the setting is a list or absent. The local roles come from `_getAllLocalRoles`: the providers of borg.localrole on the object and on each container, up to a block. A stock site has one provider, the default adapter, which reads `__ac_local_roles__`.
- `reindexObjectSecurity` of CMFCore reindexes `allowedRolesAndUsers` of every catalog entry below the object. On a folder of 50,004 descendants in the shape of `xlarge`, a share takes 66.7 s and writes 1,852 records (2.65 MB), and a transition 59.1 s and 18 records. With queued indexing, the save writes 50,005 keys (7.0 MB), and the indexer takes 120 to 128 s in 501 commits.
- "Own value or inherit" (option (a) of the card, the value whole) does not give O(1): a share changes the value of the 13,875 private and pending Documents below the folder, which do not hold the value of their parent, and a transition breaks the 36,130 entries that held the value of the folder by chance.
- Each input alone does inherit: a share changes the local roles of the folder only, and a transition changes the permission setting of the folder only. The value of every other entry follows from its own inputs and from those of its containers. On `xlarge-mini` and on the folder of 50,004 descendants, the value that the prototype resolves from these inputs equals the value of the indexer of Plone for every entry, before and after five changes; a share then takes 0.03 s and writes 4 records, a transition 0.02 s and 10 records, and with queued indexing each writes one key.

## Decision

### 1. The index class

`zoe.plone.security.ZoeSecurityIndex` takes the place of the `KeywordIndex` `allowedRolesAndUsers`. It keeps, for each rid, the inputs that the object holds itself:

- the permission setting: none (the roles of the container), a tuple (these roles only), a list (these roles and the roles of the container), or `None` (Anonymous);
- the local roles of the object, from the providers of borg.localrole on the object alone;
- the block flag `__ac_local_roles_block__`.

It also keeps the inputs of the containers outside the catalog (the Zope root; the site root is a catalog entry in Plone 6.1), by path. An object with a provider other than the default adapter, or a permission setting that names another permission, keeps its whole value: the entry is opaque.

A change of the inputs of an object writes the bucket of its rid. The index writes no forward map.

### 2. The value

The value of an entry is the value of the indexer of Plone, computed from the inputs of its object and of its containers, found by the path of the entry:

- The roles: the setting of the object; a tuple stops, a list adds the roles of the container, none takes the roles of the container, `None` gives Anonymous; with no setting up to the root, `Manager`.
- The local roles: the local roles of the object, added to those of its container unless the object blocks them.
- Anonymous among the roles gives `["Anonymous"]`; else Authenticated gives `["Authenticated"]`; else the roles, with `user:<principal>` for each principal whose local roles meet them, without `Owner`.

The mirror and the Python path resolve the same way. An opaque entry gives its value.

### 3. `reindexObjectSecurity`

`install()` replaces `reindexObjectSecurity` of `Products.CMFCore.CMFCatalogAware.CatalogAware`, at run time, and keeps the stock method. With a `ZoeSecurityIndex` in the catalog, the method reindexes `allowedRolesAndUsers` of the object alone, and of each opaque entry below it. A container outside the catalog writes its inputs to the index. Without that index, the stock method runs. A share or a transition of a folder then writes the entry of the folder.

One caller of Plone 6.1 changes the inputs of the objects below a folder and relies on the walk of the stock method: `MembershipTool.deleteLocalRoles(obj, member_ids, reindex=1, recursive=1)` of CMFCore removes the local roles of the members from each object below `obj`, then calls `reindexObjectSecurity` of `obj` ("reindexObjectSecurity is always recursive"). The deletion of a user in the control panel of Plone and in plone.restapi calls it on the site root. `install()` also replaces this method: with a `ZoeSecurityIndex`, it reindexes each object whose local roles it removes.

### 4. Queued indexing

With a `ZoeSecurityIndex`, the indexer of ADR 0047 does not add the catalog subtree of a security reindex (section 3 of ADR 0047): the entries below resolve through the folder. A share or a transition writes one key.

### 5. The backend

`security` is an opt-in backend of `zoe.plone`, as `indexing` is: `ZOE_PLONE_BACKENDS=security` or its ZCML. `install()` replaces the methods of section 3 in the process. The index itself goes into each catalog in a transaction of its own, which indexes every entry: 36 to 42 s for 50,064 entries in the prototype. Every instance of the site needs the backend: an instance without it runs the stock walk, which is correct but writes and waits as before. `uninstall()` gives the stock method back; the way back of a site puts the `KeywordIndex` back and indexes every entry, then removes the option from every instance.

### 6. The mirror

The mirror decodes the buckets of the inputs, keeps the inputs of each node of the path trie, and resolves the value of the nodes below a node whose inputs change. It keeps one bitmap for each distinct value, so a query of the tokens of a user is an or of the bitmaps whose value holds one of them.

### 7. The Python path

The Python path resolves the value of each candidate rid at query time, with a memo for each query. When ZCatalog applies other clauses first, the candidates are their result; otherwise they are every entry: 0.18 to 0.42 s for 50,064 entries in the prototype. ZOE-301 gives the Python path a cache for each process.

### 8. The specification

"Step 2" says: "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex`, or the `ZoeSecurityIndex` of `zoe.plone` (ADR 0061), which the mirror resolves as its Python path does." The invariant of `crates/zoe-catalog/CLAUDE.md` becomes: "Security stays in Plone: a `KeywordIndex` `allowedRolesAndUsers` is an index like any other, and a `ZoeSecurityIndex` resolves as its Python path does." F33 stays as it is.

## Consequences

- A share or a transition of a folder writes the entry of the folder, whatever the size of its subtree, and takes the time of one reindex.
- The value of every entry equals the value of the indexer of Plone, also with queued indexing, where a new object below a folder made private resolves through the folder.
- A caller that changes the inputs of the objects below a folder and then calls `reindexObjectSecurity` of the folder alone leaves those entries stale. In Plone 6.1, `deleteLocalRoles` is the one such caller (section 3); the other callers reindex each object whose inputs change (an ownership change, a workflow mapping update, a copy, a move). An add-on that relies on the walk needs the same care.
- A query of the Python path resolves its candidates; the mirror resolves at each change.
- A site with a provider of local roles other than the default adapter gets opaque entries, and a security change of a folder reindexes the opaque entries below it.
- The acceptance test of ZOE-301 compares the resolved value of every entry with the indexer of Plone on `xlarge`, before and after changes of security (G5).

## Points for the human

- Inherit per input (the permission setting and the local roles), not the value whole: the form that Q10 (a) takes.
- `zoe.plone` replaces two methods of CMFCore at run time (`CatalogAware.reindexObjectSecurity`, `MembershipTool.deleteLocalRoles`): no change of the code of CMFCore, but a patch.
- The public Python API: `zoe.plone.security.ZoeSecurityIndex` and the opt-in backend `security`.
- An add-on that changes the inputs of the objects below a folder and relies on the walk leaves stale entries.
- The Python path resolves at query time, slower than a `KeywordIndex` until ZOE-301 adds its cache.
- The new wording of "Step 2" and of the invariant of `zoe-catalog` (section 8).

## Alternatives

- **"Own value or inherit", the value whole.** A share writes the private and pending entries below the folder, and a transition the entries that held its value by chance.
- **The mirror only.** Plone writes as today: the walk of the subtree, its records, and one key for each entry with queued indexing. F33 asks for O(1) records.
- **No change of `reindexObjectSecurity`.** The index writes nothing for the entries that do not change, but the walk wakes every object of the subtree, and queued indexing writes one key for each.
- **A subtree operation in CMFCore.** A change of CMFCore, out of scope.
- **A stock `KeywordIndex` beside the new index.** It writes what the stock index writes.
