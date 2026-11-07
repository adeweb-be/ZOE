# Finding: the incremental security index

Card ZOE-285; requirement F33 ("incremental security index"), the gate item G5 of phase 6, the triage row "Incremental security index", and the decision Q10 of the plan of phase 6. The prototype and the scripts are on the branch `spike/ZOE-285` at [`cc80ba1d`](https://github.com/adeweb-be/ZOE/tree/cc80ba1d423e360025f4517031e6a8911426c8da), not for merge. The decision that follows is ADR 0061 (accepted).

**Verdict.** The value of `allowedRolesAndUsers` does not inherit, but its two inputs do. The `allowedRolesAndUsers` indexer of CMFPlone computes the value of an object (here, the Plone value) from the roles of the permission "Access contents information" and from the local roles, and each input reads the object and then its containers. A share changes the local roles of the folder only; a transition changes the permission setting of the folder only. An index class of `zoe.plone` that keeps, for each entry, what the object holds itself (its permission setting, its own local roles, its block flag) and resolves the value through the containers at query time gives the Plone value for every catalog entry, and a share or a transition of a folder of 50,000 descendants writes the entry of the folder alone. So Q10 option (a) holds as an index class of `zoe.plone` with "inherit", resolved in the mirror and in the Python path, with one change: the inherit is per input, not per value. "Own value or inherit" with the value whole rewrites thousands of entries for one share or one transition.

**The numbers, in short.**

- **Today**, on a folder of 50,004 descendants in the shape of `xlarge`: a share takes 66.7 s and writes 1,852 records (2.65 MB); a transition takes 59.1 s and writes 18 records (47 KB), since only the value of the folder changes, but the walk wakes and computes every entry. With queued indexing the save takes 39 to 42 s and writes 50,005 keys for the share and 50,006 for the transition (7.0 MB each), and the Indexer then takes 120 to 128 s in 501 commits.
- **The corpus.** 72 to 77 % of the entries hold the value of their parent. Every other entry is a private or pending Document in a published Folder. The sites have 3 to 17 distinct values.
- **The prototype** writes, for the same changes: a share in 0.03 s, 4 records (5.2 KB); a transition in 0.02 s, 10 records (44 KB: the other indexes, the metadata and the workflow history of the folder). With queued indexing, one key, and the Indexer applies it in 0.01 to 0.02 s in one commit. "Own value or inherit" with the value whole would rewrite 13,875 entries for the share, and 36,130 for the transition, the folder included.
- **The check.** On both sites, the value that the prototype resolves equals the Plone value for every catalog entry (2,587 and 50,064), before and after five changes of security, in the Python path and in the resolver of the mirror in Rust; four queries give the same rids. On `xlarge-mini`, two more changes make opaque entries, with the same result. No difference.

## Question

How can the catalog keep `allowedRolesAndUsers` as inherited values, resolved at query time, with the value that `reindexObjectSecurity` gives for every entry? What does Plone then write for a change of security on a folder? The specification says "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other", and the Python path is the guarantee of correctness: how do the two hold together (Q10)?

## Method

- **Code.** `main` at `a42e35c0`. The Plone instance of the scenario `zoe` (`just plone zoe build`), with Products.CMFPlone 6.1.5, Products.CMFCore 3.7, Products.PlonePAS 8.0.5, borg.localrole 4.0.2, AccessControl 7.3, Products.ZCatalog 7.2.1 and plone.app.workflow 5.0.4, on CPython 3.13 with the GIL, on an in-process `zoe.Storage`.
- **The fixtures.** The machine holds no `xlarge` fixture, and its build takes about 6.5 hours. The spike used two sites that `inzope/build_site.py` of `bench/plone` built in-process on `zoe.Storage`, in the shape of `xlarge`:
  - `xlarge-mini`, the preset: the shape of `xlarge` at 1/100, 2,586 items, 2,587 catalog entries;
  - a site made for the spike and not kept: the spec of `xlarge` with one unit, so the folder `section-00` holds 50,004 descendants (3 Folders of depth 2, 1,500 leaves, 41,052 Documents, 6,100 Files, 1,400 News Items, and the Collection of the unit), 50,063 items in all. Its text is one paragraph of 8 to 16 words, so that it builds in about 55 minutes; the security of an item does not read its text.

  In both, as in `xlarge`, the Documents are 26 % private and 8 % pending, the other items are published or have no workflow (the Files), and the group of the editors of each unit holds Contributor, Editor and Reviewer on the unit. The build runs as the system user, so no item has an Owner local role.
- **The measures.** `inzope/security_change.py` makes a share (`manage_setLocalRoles` of Reader for one member, then `reindexObjectSecurity()`, as the sharing view does) and then the transition `retract` of `section-00` (the workflow tool calls `reindexObjectSecurity()`), each in its own transaction, on a fresh copy of the store, in a fresh process with a cold object cache. The time runs from the start of the change to the end of its commit. The records and the bytes are those of the pickles of the commit, from the iterator of the store; the keys of queued indexing come from a scan of the namespace of the queue, with their key and value bytes. With queued indexing, `Indexer(site).run()` then applies the queue, and its commits are counted the same way.
- **The check.** `inzope/security_check.py`, on a copy of each store: the Plone value of every catalog entry (the value that `reindexObjectSecurity` writes), compared with the stock index; the share of entries whose value equals the value of their parent; then the index of the prototype in place of the stock index, and the resolved value of every catalog entry compared with the Plone value, before and after five changes: the share and the transition above, a block of the local roles of a leaf Folder, a local role on a private Document, and the deletion of the local roles of the member on the whole site, as the deletion of a user does. On `xlarge-mini`, two changes before the deletion make opaque entries: a local role provider for every Folder, which gives Reader to the other member on one leaf Folder, and the setting `"View"`, a str, on another leaf Folder; `reindexObjectSecurity()` of the folder follows each. After each change it also counts what "own value or inherit" would write. `examples/security_check.rs` of `zoe-catalog` resolves the exported inputs of every entry in Rust and compares them with the same values.
- **The code read**: `Products/CMFPlone/CatalogTool.py`, `AccessControl/ImplPython.py` (`rolesForPermissionOn`), `Products/PlonePAS/pas.py` (`_getAllLocalRoles`), `borg/localrole/workspace.py` and `default_adapter.py`, `Products/CMFCore/CMFCatalogAware.py`, `WorkflowTool.py` and `MembershipTool.py`, `plone/app/workflow/browser/sharing.py`, the workflows of `Products/CMFPlone/profiles/default`, and `zoe/plone/indexing.py`.
- **The machine.** Linux x86-64, 22 CPUs, 30 GiB, ext4 on an SSD. Other spikes ran on the machine; each Zope process of this spike ran alone under the lock of the heavy commands.

## Findings

### 1. The facts of Plone 6.1

- **The `allowedRolesAndUsers` indexer of CMFPlone** (`Products/CMFPlone/CatalogTool.py:127-155`) takes the roles of the permission "Access contents information" on the object (`rolesForPermissionOn`). With Anonymous among them the value is `["Anonymous"]`; else with Authenticated it is `["Authenticated"]`. Else it takes the local roles of the object (`acl_users._getAllLocalRoles`), adds `user:<principal>` for each principal that holds one of the roles, and removes `Owner`.
- **The roles of the permission** (`rolesForPermissionOn`, `AccessControl/ImplPython.py:68-120`, and its C twin) read the attribute `_Access_contents_information_Permission` of the object, then of each container: a tuple stops the walk, a list adds its roles and goes on, an absent attribute goes on, `None` gives Anonymous, and a walk that finds nothing gives `Manager`. Acquisition gives no attribute whose name starts with an underscore, so each step reads the setting of that object only.
- **The local roles** (`Products/PlonePAS/pas.py:392-404`) are the union of the local role plugins. A stock site has one, `borg_localroles` (`WorkspaceLocalRoleManager.getAllLocalRolesInContext`, `borg/localrole/workspace.py:495-507`): for the object and each container, the union of the `ILocalRoleProvider` adapters of that object, and the walk stops after an object whose `__ac_local_roles_block__` is true.
- **The providers of a stock site.** One: `DefaultLocalRoleAdapter` of borg.localrole, which reads `__ac_local_roles__` of the object. The other provider of borg.localrole is for the temporary folders of ATContentTypes, which Plone 6.1 does not have. An add-on can register more; such a provider can read anything.
- **What the index cannot read.** `rolesForPermissionOn` reads a str setting as the name of another permission, and goes on with that name at the container; the empty str gives no roles at once. It reads any other setting that is not `None` or a tuple as a list: a `PersistentList` adds its roles, and an empty sequence counts as no setting. `getAllLocalRolesInContext` also runs the old `IWorkspace` path of borg.localrole at each node, and that path replaces the roles of a principal. The default adapter calls a callable `__ac_local_roles__`. Plone reads the value through plone.indexer, so an add-on can register its own `allowedRolesAndUsers` indexer for a more specific interface. `_getAllLocalRoles` of the nearest `acl_users` unions all its active `ILocalRolesPlugin`. `reindexObjectSecurity` walks for the indexes of `_cmf_security_indexes`, which a class can extend. ADR 0061 makes a catalog entry opaque for each case but the last, and keeps the stock method for the last.
- **The workflow of a stock site** (`simple_publication_workflow`) sets "Access contents information" as a tuple in each state: private gives Manager, Owner, Editor, Reader, Contributor and Site Administrator, pending adds Reviewer, published gives Anonymous. File, Image and the site root have no workflow. A File has no setting, so it takes the roles of its folder. The site root has a list (Site Administrator, Reader, Editor, Contributor, from `rolemap.xml`), and the Zope root the class default, the tuple Anonymous and Manager. The site root is a catalog entry in Plone 6.1.
- **So the value of an object equals the value of its parent** when the object has the same roles as its parent (the same state, or a File in its folder) and its own local roles add no principal that holds one of them. A private or pending Document in a published Folder has a value of its own; so does each item with a local role that meets its roles.
- **`reindexObjectSecurity`** (`Products/CMFCore/CMFCatalogAware.py:100-131`) searches the catalog for the path of the object and calls `reindexObject(idxs=["allowedRolesAndUsers"], update_metadata=0)` on every entry it finds, the object included. Its callers in Plone 6.1: the workflow tool after each transition (`WorkflowTool.py:601`), the sharing view after a change of local roles or of the block (`plone/app/workflow/browser/sharing.py`), the local roles of plone.restapi, `PloneTool.acquireLocalRoles`, plone.app.iterate, and `MembershipTool.deleteLocalRoles`. The last one removes the local roles of members on each object below its object, recursively, and then relies on the walk ("reindexObjectSecurity is always recursive", `MembershipTool.py:471-473`); the deletion of a user in the control panel and in plone.restapi calls it on the site root.
- **With queued indexing** (ADR 0047), each `reindexObject` of the walk is one operation of the queue, so the save writes one key for each entry of the subtree, and the Indexer adds the catalog subtree of each security reindex again (`_expand_security`).

### 2. The corpus

The Plone value of each catalog entry equals the stock index on both sites, so the stock catalog is current.

| Site | Entries | Equal to the parent | Not equal | Distinct values |
| --- | ---: | ---: | --- | ---: |
| `xlarge-mini` | 2,587 | 1,993 of 2,586 (77.1 %) | 441 private and 152 pending Documents in a published Folder | 17 |
| the folder of 50,004 descendants | 50,064 | 36,188 of 50,063 (72.3 %) | 10,626 private and 3,249 pending Documents in a published Folder | 3 |

The entry left out of each count is the site root, whose container, the Zope root, is outside the catalog. The value of a published item is `["Anonymous"]`, as is the value of its published Folder and of a File in it. A private or pending Document holds its roles and the `user:` token of the group of its unit, which its published Folder does not hold.

The inputs that the prototype keeps, on the folder of 50,004 descendants: 43,963 entries with a tuple (the states of the workflow), 6,100 without a setting (the Files), 1 with a list (the site root); 3 entries with local roles of their own (the unit, `bench` and `bench/import`), no block, no opaque entry, 8 distinct inputs. On `xlarge-mini`: 2,281 tuples, 305 without a setting, 1 list, 10 entries with local roles, 15 distinct inputs.

These sites have no Owner local role, since the build runs as the system user. On a site where users create content, each object holds the Owner local role of its creator. For the prototype, that is one more input of each object, which a share of a container does not change.

### 3. What a change of security on the folder of 50,000 descendants writes today

The stock catalog, the stock `reindexObjectSecurity`, on the folder `section-00` and its 50,004 descendants. The share gives Reader to one member, so the value of the 13,875 private and pending Documents below gains `user:member-000`. The transition `retract` changes the value of the folder alone.

| Change | Queued indexing | Time of the save | Records of the save | Bytes of the save | Keys | The Indexer |
| --- | --- | ---: | ---: | ---: | --- | --- |
| share | off | 66.7 s | 1,852 | 2,650,185 | – | – |
| transition | off | 59.1 s | 18 | 47,278 | – | – |
| share | on | 42.2 s | 1 | 1,945 | 50,005 (7,025,092 bytes) | 119.9 s, 501 commits, 3,008 records, 2,803,572 bytes |
| transition | on | 39.2 s | 2 | 2,564 | 50,006 (7,025,195 bytes) | 127.6 s, 501 commits, 1,016 records, 79,714 bytes |

- The time is the walk: the search of the subtree, a wake of each object and a Plone value for each. The value of every entry alone takes 34.1 s on this site (`security_check.py`).
- Without queued indexing, the records of the share are the buckets of the `_unindex` and the tree sets of the `_index` of the `KeywordIndex` that hold the 13,875 changed entries. The transition writes few records, because only the value of the folder changes.
- With queued indexing, each `reindexObject` of the walk is one key, so the save writes 7 MB of keys whatever the values do. The Indexer applies the first batch with the whole subtree that `_expand_security` adds, then 500 batches of 100 keys whose subtrees it searches again.
- On `xlarge-mini`, the unit `section-06` of 1,109 descendants: a share with queued indexing writes 1,110 keys (154 KB) in 0.8 to 0.9 s, and the Indexer takes 1.9 to 2.0 s in 12 commits.

### 4. The candidates

The card names (a) and (b); the facts of section 1 give (c), which the spike prototyped. The numbers of (a) are the entries that it must rewrite after the change on the folder of 50,004 descendants, counted by `security_check.py` from the values before and after the change; the spike did not build (a). The numbers of (c) are measured.

- **(a) Own value or inherit, the value whole.** An entry whose value equals the value of its parent holds "inherit", the others their value. A share changes the value of the private and pending Documents below the folder, which hold their own value: 13,875 entries to rewrite (259 on `xlarge-mini`). A transition changes the value of the folder, and the 36,129 entries that inherited from it by chance (published items with `["Anonymous"]`) must then take their own value: 36,130 writes with the folder (851 on `xlarge-mini`). The writes follow the share of entries that equal their parent, not the change.
- **(b) The mirror only.** The mirror keeps the values as parent pointers in memory, and Plone writes as today (section 3). It saves memory in the mirror and nothing in the store, and with queued indexing the save still writes one key for each entry.
- **(c) Inherit per input.** The index keeps, for each entry, the permission setting, the local roles and the block flag of its object, and resolves the value through the containers (in the mirror at the version of the query, in the Python path at query time), as the indexer of CMFPlone does (ADR 0061, section 2). `reindexObjectSecurity` reindexes the object alone. A share changes the local roles of the folder, a transition its setting: one entry each.

| | Today | (a) | (b) | (c), prototyped |
| --- | --- | --- | --- | --- |
| Share on the folder of 50,004 descendants | 66.7 s; 1,852 records, 2.65 MB | 13,875 entries | as today | 0.03 s; 4 records, 5,189 bytes |
| Transition of that folder | 59.1 s; 18 records, 47 KB | 36,130 entries | as today | 0.02 s; 10 records, 44,279 bytes |
| With queued indexing | 50,005 keys (7.0 MB) in a save of 42 s; the Indexer 120 s, 501 commits | as the walk goes | as today | 1 key (112 bytes); the Indexer 0.01 s, 1 commit, 3 records (share), 8 records (transition) |
| A query | the bitmaps of the tokens | a resolution of each entry | the bitmaps | mirror: an or of the bitmaps of the distinct values, 0.8 µs over 6 values, after a resolution at each change, 9.2 ms for 50,064 entries in Rust; Python path: 0.18 to 0.42 s for a query over 50,064 entries, with no cache |
| Size in the store | 3,691 records, 2,284,547 bytes | a value or "inherit" for each rid (not run) | as today | 1,673 records, 2,810,825 bytes |
| Memory of the mirror | a bitmap for each token | a value or "inherit" for each node | a parent for each node | for each node a parent, its inputs and its value; a bitmap for each distinct value |
| Queued indexing | one key for each entry of the subtree, and the subtree again in the Indexer | as the walk goes | as today | one key; the Indexer adds no subtree |
| Providers that read more than the object | the walk reindexes them | their value goes stale | the walk reindexes them | opaque entries keep their value, the entries below them are opaque too, and `reindexObjectSecurity` of a container reindexes them; a stock site has none |
| The way back | – | the `KeywordIndex` back, a reindex of the index | nothing | the `KeywordIndex` back, a reindex of the index: not measured (the install takes 36 to 42 s on this site) |

- The records of (c) for a transition are those of the other indexes (`review_state` and the dates), the metadata and the workflow history of the folder; the security index writes one bucket.
- The prototype stores the inputs of each rid as a tuple of strings, so its buckets are bigger than those of the `KeywordIndex`. An id for each distinct input (8 on this site) would make them smaller; ZOE-301 can choose.
- The Python path resolves the candidates of each query. When `allowedRolesAndUsers` comes first in the plan of ZCatalog, the candidates are every entry: 0.18 to 0.42 s on 50,064 entries; one million entries is not run. A cache for each process keeps it near the cost of a `KeywordIndex`; the mirror serves these queries in normal operation.
- (c) needs two methods of CMFCore replaced at run time by `zoe.plone`: `reindexObjectSecurity`, so that the walk stops, and `deleteLocalRoles`, which relies on the walk (section 1). Without the first, the index writes nothing for the entries that do not change, but the walk keeps its time and queued indexing its 50,000 keys (not run).

### 5. The prototype and its check

The prototype (`spike/ZOE-285` at `cc80ba1d`):

- `python/zoedb/src/zoe/plone/security.py`: `ZoeSecurityIndex`, a subclass of `KeywordIndex` that keeps the inputs of each rid and of the containers outside the catalog, makes an entry opaque by the rule of ADR 0061 (section 1), and resolves through the paths of the catalog; `replace_index` and `restore_index`; `install()`, which replaces `reindexObjectSecurity` and `deleteLocalRoles` of CMFCore.
- `python/zoedb/src/zoe/plone/indexing.py`: `_expand_security` adds no subtree when the catalog has the index.
- `crates/zoe-catalog/src/security.rs`: the resolver of the mirror, with interned names, a memo for each node, and a bitmap for each distinct value; `examples/security_check.rs` resolves the inputs that the check exports.
- `bench/plone/src/plonebench/inzope/security_check.py` and `security_change.py`: the check and the measures.

The check compares the resolved value of every catalog entry with the Plone value, which is the value that `reindexObjectSecurity` of a stock catalog writes.

| Step | `xlarge-mini` (2,587 entries): changed / differ | 50,004 descendants (50,064 entries): changed / differ |
| --- | --- | --- |
| after the index replaces the stock index | – / 0 | – / 0 |
| share on the folder (Reader for a member) | 259 / 0 | 13,875 / 0 |
| transition `retract` of the folder | 1 / 0 | 1 / 0 |
| block of the local roles of a leaf Folder | 8 / 0 | 9 / 0 |
| Reader for another member on a private Document | 1 / 0 | 1 / 0 |
| a provider other than the default adapter for every Folder, Reader for the other member on a leaf Folder | 8 / 0 | not run |
| the setting `"View"` (a str) on another leaf Folder | 5 / 0 | not run |
| the local roles of the first member deleted on the whole site, as the deletion of a user does | 252 / 0 | 13,867 / 0 |
| the resolver in Rust, on the final inputs | – / 0 | – / 0 |

- "Changed" counts the catalog entries whose Plone value changed with the step; "differ" the entries whose resolved value is not that value.
- The `xlarge-mini` column comes from the prototype at `cc80ba1d`, on a fresh build of the fixture. Every catalog entry below a new opaque entry became opaque through the walk of section 3 of ADR 0061. With the provider in place, the deletion of the local roles walks the Folders that it makes opaque: 60 records in 1.4 s, against 4 records in 0.3 s before.
- Four queries (Anonymous, the tokens of two members, the tokens of an editor of the unit) give the same rids as the Plone values, before and after the changes. An anonymous user finds no entry that the stock catalog hides from it.
- Each change wrote 4 records, the transition 10, and the deletion of the local roles 4 records in 9.2 s, the walk of `contentValues` over the site that the stock method of CMFCore also makes.
- The resolver in Rust reads the inputs of 50,064 entries in 20 ms and resolves them in 9.2 ms (release build), and groups them into 6 distinct values in 1.0 ms.
- Not run: the fixture `xlarge` itself, which this machine does not hold (its build takes about 6.5 hours); the two sites have its shape, and the check of G5 on `xlarge` belongs to ZOE-301. Not run either: a site with Owner local roles, a list setting below the site root, a comment or a mount, and the opaque entries on the folder of 50,004 descendants; the corpus has none.

## What the verdict changes

- **Q10.** Option (a) holds as the human decided it: an index class of `zoe.plone` with "inherit", resolved at query time in the mirror and in the Python path, and the Python path stays the guarantee of correctness, since it resolves the same inputs the same way. What inherits is each input, not the value; ADR 0061 records it. ADR 0061 also records how the mirror versions the inputs (section 6).
- **"Step 2" of the specification** says today: "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex` like any other." With the index, it says: "Security stays in Plone: `allowedRolesAndUsers` is a `KeywordIndex`, or the `ZoeSecurityIndex` of `zoe.plone` (ADR 0061), which the mirror resolves as its Python path does." The invariant of `crates/zoe-catalog/CLAUDE.md` changes the same way. ZOE-301 makes both changes, as the plan of phase 6 says (item 8 of its gaps), and the other changes of the specification that ADR 0061 lists.
- **ZOE-301** builds the index class, the backend `security` with its two replaced methods of CMFCore, the skip of the security subtree in the Indexer of queued indexing, the resolution in the mirror (the rows of each parent path of ADR 0059, section 3, built in ZOE-288, give the parents), a cache of the Python path for each process, and the check of G5: the resolved value of every catalog entry against the Plone value, the bitmap of each token, and the rids of queries, before and after changes of security, on small synthetic fixtures and on `xlarge` (ADR 0061, Consequences).
- **Plone writes less, and differently.** A share or a transition writes the entry of the folder; the deletion of a user writes the entries whose local roles it removes. The other indexes and the metadata of a transition are written as before.
