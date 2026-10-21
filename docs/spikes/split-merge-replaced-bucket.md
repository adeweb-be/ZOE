# Spike: the bucket that a split merge replaces, and the insert that it loses

Card ZOE-224, step 1; the success criterion on `ConflictError`s (section 1), F5 (tier 1) and lever 4 of section 9 of the specification; the gate of phase 4 (#440). The decision that follows is ADR 0049 (accepted).

**Verdict.** With the split merge on, `create-spread` on `zoe-native-qi` raised 28 `ConflictError`s in 1,038 operations over three runs (27.0 per 1,000). 18 of them are read conflicts "a path child that the other side replaced": the case of the card. Each one has the order of events of the card. S1 splits the last bucket A into A and Y. T reads the node right after S1 and inserts into Y. S2 read the node before S1, also split A, and rule 3 of ADR 0048 put its own new bucket X in the place of Y. T then fails its vote on the node. Every merge of rule 3 in the runs replaced a bucket (59 of 59), because the keys grow: two creations that read one state of a node split the same last bucket. No Y got a second record before it left the tree. A rule 3 that keeps Y in the tree removes the 18 cases: an estimate from the logs, since the runs did not have such a rule, leaves 10 `ConflictError`s (9.6 per 1,000). To keep Y, the vote of S2 must write Y, which S2 did not store: it needs a lock on Y and a client that invalidates Y in all its Connections.

## Question

Step 1 of the card:

- How often does a bucket leave the tree under a transaction that wrote into it, over three runs?
- Confirm the order of events from the resolution log: S1 splits A into A and Y; S2 read before S1, splits A, and rule 3 puts X in the place of Y; T read between S1 and S2 and inserted into Y.

## Method

- **Code.** `main` at `6ab852d4`, after ZOE-225 (the vote writes a line of the resolution log for each read conflict that rule 1 leaves without an answer) and ZOE-227 (B5 counts the bytes written to the store files). The `zoe` binary of that commit (`cargo build --release`), and the Plone instances of the scenarios `zeo` and `zoeclient`, built in the worktree: CPython 3.13.15, Plone 6.1.5, BTrees 6.3, ZODB 6.2, ZEO 6.2, persistent 6.5 (the eggs of `target/plone/eggs/cp313` and the `environment` of each `result.json`). The line numbers below are in these eggs.
- **The fixture.** `small-ba01ff255313` (685 objects), built by `just bench plone prepare`.
- **The runs.** `just bench plone run --stack zoe-native-qi --scenario create-spread --rep N --steps 4:60,16:60 --port-offset 300 --split-merges on --keep-store --label ZOE-224` for N = 0, 1, 2, one at a time, under `systemd-inhibit --what=sleep:idle`. `ZOE_BINARY` named a script outside the repository that adds `--resolution-log` to `zoe serve`. Every run passed its verification.
- **The machine.** Intel Core Ultra 7 155H, 22 logical CPUs, 32 GB, Linux 7.0. Other agents ran builds and tests. The 1-minute load average was 1.0 to 3.4 at the start of each run and 4.6 to 8.6 at its end. The third run had more load during its steps, and fewer operations.
- **The resolution log.** A script outside the repository read the lines of each run: the read conflicts of the vote (outcome `undecided`, `split` 1) by reason, and the merges of rule 3. It counted the lines inside the measured window of each step, to compare with the counts of B5.
- **The order of events.** The same script read the kept store read-only (stub classes, no Zope). For each line "a path child that the other side replaced" of T, it compared the node at the serial that T read with the node at the committed serial, to find the replaced child Y and the new child X. It found the merge of rule 3 of S2 from the line of S2 on the node (the description of S2 is `committed_description` of the line of T), and the commit of S2 from the history of the node. It checked `read(S2) < creation(Y) <= read(T) < commit(S2)`. For each merge of rule 3, it compared the node before and after the commit of S2, counted the replaced children, the records of Y, the bytes of Y, and the keys of S2 in the range of Y.
- The kept stores were deleted after the reads. The results, the logs, the resolution logs and the scripts are outside the repository.

## Findings

### 1. The runs and the conflicts

| run | users | operations | ok ops/s | 1st-try ok | write | read | "replaced" | conflicts/1k | store KB written/op |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| r0 | 4 | 195 | 3.55 | 192 | 0 | 3 | 1 | 15.4 | 1,483 |
| r0 | 16 | 205 | 3.73 | 194 | 3 | 8 | 8 | 53.7 | 1,413 |
| r1 | 4 | 202 | 3.67 | 201 | 0 | 1 | 1 | 5.0 | 1,481 |
| r1 | 16 | 181 | 3.29 | 176 | 2 | 4 | 4 | 33.1 | 1,269 |
| r2 | 4 | 127 | 2.31 | 123 | 2 | 2 | 2 | 31.5 | 1,412 |
| r2 | 16 | 128 | 2.33 | 125 | 1 | 2 | 2 | 23.4 | 1,307 |
| **all** | | **1,038** | | | **8** | **20** | **18** | **27.0** | |

- "write" and "read" are the `ConflictError`s that B5 counts in the measured window. "replaced" is the number of read conflicts whose line in the resolution log has the reason "a path child that the other side replaced".
- The other two read conflicts (r0, 4 users) have the reason "a path child that the other side split": T inserted into the last bucket of `intids.ids` without a split, and the other side split that bucket. This is the case of question 6 of ADR 0048.
- In r2 at 16 users the log has three lines "replaced" in the window and B5 counts two read conflicts: B5 counts an operation in the step where its request starts.
- The write conflicts: five on a `Folder` (two creations in one folder, out of ADR 0048), two on `OOBucket` and one on `OOBucket` through `UniqueIdGeneratorTool` (r0, 4 users).
- By step: at 4 users 8 `ConflictError`s in 524 operations (15.3 per 1,000), 4 of them "replaced"; at 16 users 20 in 514 (38.9 per 1,000), 14 of them "replaced".
- Over the whole runs, the warmup included, the logs hold 20 lines "replaced" and 2 lines "split": every read conflict of the vote has one of these two reasons.

### 2. The order of events

All 20 lines "replaced" of the three runs have the order of the card:

| check | cases |
| --- | ---: |
| `read(S2) < creation(Y) <= read(T) < commit(S2)` | 20 of 20 |
| Y created by the commit that T read (`creation(Y) = read(T)`) | 20 of 20 |
| one merge of rule 3 between T's read and the commit that T fails on | 16 |
| two merges of rule 3 (a chain: S3 also read before S1 and replaced X of S2) | 4 |

By tree: `_shadowStorage._storage` 12, `intids.ids` 7, `intids.refs` 1. `zvc_repo._histories`, whose keys are random, shows none.

A chain, from r0 on `intids.ids` (the last children of the node, separator and oid):

```
read(S2), read(S3)   ... (KR(0x86e7), 8b1d)
S1 at …19b4b4d3      ... (KR(0x86e7), 8b1d), (KR(0x8af1), 8f4b)   S1 splits 8b1d; Y = 8f4b
S2 at …19f0073b      ... (KR(0x86e7), 8b1d), (KR(0x8af1), 8ed6)   rule 3: X2 = 8ed6 replaces Y
S3 at …1a0b79b2      ... (KR(0x86e7), 8b1d), (KR(0x8af1), 8ed5)   rule 3: X3 = 8ed5 replaces X2
T read …19b4b4d3, inserted into 8f4b, fails on the node at …1a0b79b2: "a path child that the other side replaced"
```

S2 and S3 read the node before S1, so each one split the same bucket 8b1d and merged onto the commit before it. The oids of X are below the oid of Y: S2 and S3 began before S1 and got their oids first.

### 3. Every merge of rule 3 replaced a bucket

| run | merges of rule 3 on a node | with a replaced child | Y with more than one record | keys of S2 all below the separator of Y | bytes of the replaced Y |
| --- | ---: | ---: | ---: | ---: | ---: |
| r0 | 24 | 24 | 0 | 1 | 56,367 |
| r1 | 21 | 21 | 0 | 0 | 56,829 |
| r2 | 14 | 14 | 0 | 0 | 35,523 |
| all | 59 | 59 | 0 | 1 | 148,719 |

- The committed side split the bucket in each merge: rule 3 never met a bucket that the committed side changed without a split. In a tree whose keys grow, a creation inserts into the last bucket, and the bucket is full when a creation splits it, so the next creation that read the same state splits it too.
- No Y had a second record: no transaction wrote into Y and committed between S1 and the merge of S2. So each T that inserted into Y failed: 20 transactions T, on 59 merges.
- In 58 of 59 merges, S2 had a key in the range of Y. A rule that keeps Y only when the keys of S2 all fall below the separator of Y (so Y does not change) would cover 1 merge.
- Each merge left one bucket out of the tree, 148,719 bytes of garbage in the three runs, which a pack with garbage collection removes.

### 4. What removes the case

- **Rule 3 keeps Y.** The keys of the range merge as today and go to A and Y; X gets no place. The node keeps its children, so T's write to Y stays in the tree: its read-current node does not change, and its stale Y merges with the record of S2 (tier 1, or the client for the key references of `intids`). On the runs this removes the 18 cases: an estimate of 10 `ConflictError`s in 1,038 operations (9.6 per 1,000) instead of 28. It also removes the garbage of finding 3. It writes Y, a record that S2 did not store, so the vote must lock Y and the client of S2 must drop Y in all its Connections. The vote finds Y in its plan, after it took its locks in oid order, so the lock of Y cannot wait.
- **Rule 1 merges T into the bucket that replaced Y.** It writes a record that T did not store too, it must follow the chains (4 of 20), and it leaves the garbage.
- **Rule 3 keeps Y when Y does not change.** 1 merge of 59.
- **The plain insert into a bucket that the other side split** (2 cases; ADR 0048, Alternatives) needs the same machinery: a write of a bucket that the transaction did not store.

### 5. What the clients of the process that commits see

The vote of S2 lists each merged oid in its reply as a bare oid, as a ZEO 6 server lists a conflict that it resolved. For an oid that the transaction did not store (Y):

- A stock `ZEO.ClientStorage` ends the current entry of Y in its cache (`TransactionBuffer` yields Y with no data; `tpc_finish_co` calls `cache.invalidate`), and ZODB turns Y into a ghost in the Connection that committed (`Connection.tpc_vote`). The other Connections of the process get only the oids that the committing Connection stored (`MVCCAdapterInstance._modified`, `ZODB/mvccadapter.py:201-223`), and the server sends no invalidation to the client that committed. A Connection that holds Y keeps its old state: a stale read.
- The native client ends the current entry of Y in its cache, as for the oids of an undo. Its other Connections have the same gap: the finish runs `f(tid)`, which gives them `_modified`.
- Every other client gets Y in `invalidateTransaction`, since Y is a record of the commit.

So the write of Y needs a client that invalidates Y in its other Connections before it publishes the tid, and a way for the server to know it. The invalidation must hold every oid of the commit in one call: `MVCCAdapterInstance._invalidate` moves `_ltid` to the tid (`ZODB/mvccadapter.py:130-136`), so a Connection that polls between a call with Y alone and the callback `f(tid)` would get a snapshot after the commit with only Y invalidated. ADR 0049 takes a declaration in `register` and one invalidation with every oid of the commit.
