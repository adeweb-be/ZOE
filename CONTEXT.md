# ZOE

ZOE, the Zope Object Engine, is a ZODB storage written in Rust that Plone uses unchanged, in-process or behind a server that speaks the ZEO protocol. This glossary gives agents and humans one vocabulary for the storage and for the way the project is built.

## Storage

**Store**:
One ZOE database: the records, the transaction log and the blobs in one directory or one backend.
_Avoid_: database file, Data.fs

**Record**:
One version of one object in the store: the pickle that one transaction wrote for one oid.
_Avoid_: row, revision

**Oid**:
The 8-byte identifier of a persistent object, stable for the life of the object.
_Avoid_: object id, zoid

**Tid**:
The 8-byte identifier of a committed transaction, a ZODB `TimeStamp` that always increases.
_Avoid_: transaction number, timestamp

**Serial**:
The tid of the transaction that wrote a given record; a client compares the serial it read with the current one to detect a conflict.
_Avoid_: version number

**Pickle**:
The bytes of a record: a class pickle followed by a state pickle.
_Avoid_: payload, blob (a blob is something else)

**Ghost**:
A persistent object that a Connection knows by oid but whose state is not loaded yet.
_Avoid_: stub, proxy

**Blob**:
A file attached to a record and stored beside the store, immutable per oid and tid.
_Avoid_: attachment, binary

**Blob directory**:
The directory that holds the blob files of a store in ZODB's bushy layout: `blobs` in the store directory, or the directory that `blob-dir` gives; the store does not record it, so each open must give the same one.
_Avoid_: blob store (that is the trait with a directory and an S3 implementation), blob cache

**Backend**:
The component that keeps the bytes of a store durable under the data model's keys; the core owns everything else.
_Avoid_: driver, engine

**Skin**:
One of the two ways in to the core: the in-process storage or the server.
_Avoid_: frontend, adapter

**Named storage**:
One of several stores that one server process serves, selected by the client at registration.
_Avoid_: tenant, database

**Snapshot**:
The view of the store as of one tid; a reader sees all of a transaction or none of it.
_Avoid_: checkpoint

**Transaction index**:
The key space that lists, for each tid, the oids that its transaction wrote; the iterator and the server read the oids of a transaction from it.
_Avoid_: oid list, transaction log (that is the rows of the transactions)

## Commit and conflicts

**Two-phase commit**:
The commit protocol of ZODB: `tpc_begin`, stores, vote, then `tpc_finish` or `tpc_abort`.
_Avoid_: 2PC (in prose), commit (alone)

**Vote**:
The second phase of a commit, where the storage takes the locks, detects conflicts and resolves them before anything is durable.
_Avoid_: prepare

**Finish**:
The last phase of a commit, where the storage allocates the tid and makes the transaction durable.
_Avoid_: commit (alone)

**Group commit**:
A batch of concurrent finishes that share one durable write.
_Avoid_: batch commit

**Restore transaction**:
A transaction that `tpc_begin` begins with a tid, for a copy or a replica; it commits under that tid, which must come after the last tid of the store.
_Avoid_: import, replay

**Per-OID lock**:
A lock that one transaction holds on one oid during a commit; exclusive for a write, shared for a read-current check.
_Avoid_: row lock, commit lock

**Lock hand-off**:
The pass of a per-OID lock from a commit whose finish started to the vote at the head of the queue of the oid, with a copy of the staged record of the holder; the vote merges onto that record, and its commit comes after the commit of the holder.
_Avoid_: lock transfer, lock steal

**Commit in flight**:
A commit whose finish started and that is not durable or failed yet; its staged records are final.
_Avoid_: pending commit

**Intent lock**:
An exclusive per-OID lock that a client asks for before the vote, when its transaction loads an object of a hot class, so a second writer waits before it reads (ADR 0045).
_Avoid_: early lock, pessimistic lock

**Conflict**:
Two transactions that write the same oid, where the second one read a serial that is no longer current.
_Avoid_: collision, ConflictError (that is the exception, not the event)

**Conflict tier**:
One of the three levels that try to resolve a conflict: structural merges, field-level merges by policy, and resolution on the client.
_Avoid_: tier (alone; see Deployment tier)

**Merge policy**:
A versioned object in the store, `root['zoe.merge_policy']`, that says which classes merge field by field and with which strategy per field, and which switches of the structural merges win over the keys of the store.
_Avoid_: resolver config, merge rules

**Strategy**:
What tier 2 does with a field that both sides changed to different values: `strict`, `lww`, `max`, `sum`, `union`, `append` or `keys`. `keys` merges a dict key by key, with the rules of tier 2 one level down (ADR 0046, section 11).
_Avoid_: merge function, resolver

**Coupled fields**:
A group of fields of a class that tier 2 never merges apart: when both sides changed the group, they must agree on each of its fields.
_Avoid_: linked fields, invariant fields

**Split merge**:
The rules of tier 1 that merge the splits of a BTree where ZODB's resolver refuses (ADR 0048). Rule 1: a read-current node that the other side changed only by splits passes, when the other side split none of the children under which the transaction wrote. Rule 2: splits of different children of one node merge. Rule 3: a bucket that the transaction split once, and that the other side split once or changed without a split, merges the keys of its range. The switch `split_merges` of the merge policy turns it on or into shadow mode; it is off by default.
_Avoid_: split resolution, BTree merge

**Anchor**:
The child of a node that a split cuts in two: it keeps the keys below the new separator, and the new child right after it gets the others.
_Avoid_: parent bucket, split source

**Passed node**:
A node that the transaction marked read-current and did not store, that the other side changed only by splits, and that rule 1 of the split merge lets through the vote, because the other side split none of the children under which the transaction wrote. The vote writes nothing for it, and its locks stay shared. Its line in the resolution log has the outcome `passed`.
_Avoid_: merged node (rule 1 writes no record), skipped node

**Verification**:
The check of a merge of shadow mode against the resolution of the client: the second vote compares the record that the client stored with the merged record, and the resolution log says whether they are the same.
_Avoid_: validation, proof

**Shadow mode**:
A mode where a conflict tier computes its merge and writes it to the resolution log, and the conflict then goes on as without that tier, to show what merging would have done.
_Avoid_: dry run (that word belongs to pack)

**Invalidation**:
The message that tells every other client which oids a committed transaction changed.
_Avoid_: cache purge, notification

**Published tid**:
The tid of the last commit whose invalidations went out: in-process when the callback of its `tpc_finish` ran, in the server when the invalidation hub queued them; `lastTransaction` gives it, and the store makes a commit visible before it is published.
_Avoid_: last tid (the last tid of the store can be ahead of it)

**Sequence**:
A named counter in the store that gives monotonic values outside the commit locks.
_Avoid_: counter, autoincrement

**KV namespace**:
A named set of keys and values beside the object graph, written in the transaction of a commit, with one row per key and no history. Its flavour is ephemeral or durable.
_Avoid_: key-value store, cache, bucket

**Ephemeral namespace**:
A KV namespace whose keys each have a TTL and go without a pack: sessions and locks.
_Avoid_: temporary storage, transient container

**Durable namespace**:
A KV namespace whose keys stay until a delete: the registry.
_Avoid_: persistent namespace

**Generation**:
The tid of the last commit that wrote a durable namespace. A cache of the namespace stays valid while it stays the same.
_Avoid_: version, revision

**Sweep step**:
One backend write that removes the expired keys of the ephemeral namespaces, at most 1,000.
_Avoid_: garbage collection, expiry pack

## Server

**Invalidation hub**:
The part of the server, one for each store, that publishes the commits of the store in commit order: it puts the invalidations, and the `info` and the reply of each `tpc_finish`, in the hub queue of each connection, and it never waits.
_Avoid_: broadcaster, invalidation queue (`invalidation-queue-size` bounds the replay of `getInvalidations`)

**Hub queue**:
The queue of one connection that holds the frames of the invalidation hub; it holds at most `client-queue-size` frames, and a full one closes the connection, so a slow client never stops a commit.
_Avoid_: outbox (that word belongs to post-commit delivery), send buffer

**Session queue**:
The queue of one connection that holds the replies of its session, a few frames (`QUEUE`); the session waits while it is full, so a slow client slows only its own connection.
_Avoid_: outbox, reply buffer

**Stop**:
The graceful end of a server on SIGTERM or SIGINT: it accepts no more connections, runs the calls that it received, so a received `tpc_finish` commits and gets its reply, aborts the open transactions, closes each connection, then closes the stores.
_Avoid_: kill (SIGKILL is not a stop), drain (that is maintenance mode)

**Shutdown timeout**:
The longest time that a stop takes, `--shutdown-timeout`; at its end the process exits, and the store of a session whose call still runs does not close cleanly.
_Avoid_: grace period (that is the setting of the orchestrator, which must be longer)

**Heartbeat**:
The frame with the id -1 that a ZEO client sends every 60 seconds, busy or idle; the server sends no reply, and the frame counts only for the idle timeout.
_Avoid_: keepalive (that is TCP's, which the server does not set), ping

**Idle timeout**:
The longest time that a connection may send no frame, `--idle-timeout`; at its end the server closes the connection, which aborts its open transaction and releases its locks.
_Avoid_: lock timeout (that bounds a lock wait), keepalive

## Client

**Runtime**:
The trait through which `zoe-client` reaches the network, its tasks and the time (connect, spawn, sleep); `Tokio` is the runtime of production, and a test gives its own.
_Avoid_: executor (that is the scheduler of the simulator), tokio runtime

## Operations

**Pack**:
The removal of record versions that no reader can reach, older than a given tid.
_Avoid_: garbage collection, compaction, vacuum

**Pack watermark**:
The tid of the last pack, which the first batch of a pack writes in the store; undo never reaches back past it, and a pack at an earlier tid does nothing.
_Avoid_: pack horizon, low-water mark

**Retention window**:
The period during which records that a pack removed can still be restored.
_Avoid_: grace period, trash

**Fork**:
A copy-on-write store over a base store at a fixed tid; its writes never go back to the base.
_Avoid_: branch, clone, snapshot

**Feed**:
The resumable stream of committed transactions, from any tid, in commit order, exactly once.
_Avoid_: change log, event stream, CDC

**Scrub**:
A walk of the whole store that verifies every checksum and reports every mismatch.
_Avoid_: fsck, verify (verify checks references and classes)

**Dump**:
One file that `zoe backup` writes: the transactions of one tid range with their records and blobs, the ends of the sequences, and a BLAKE3 hash of its bytes; a backup never changes it.
_Avoid_: snapshot (a snapshot is a view of the store), backup file

**Full dump**:
The first dump of a chain: every transaction of the store, from tid 0.
_Avoid_: base backup

**Chain of dumps**:
The full dump and the dumps after it, each one starting after the last tid of the dump before it; one backup directory holds one chain, and `zoe restore` reads the chain in order.
_Avoid_: backup set, backup history

**Deployment tier**:
One of the four Kubernetes shapes: in-process (0), one server (1), leader and follower (2), stateless fronts over a distributed backend (3).
_Avoid_: tier (alone; see Conflict tier)

**Loupe**:
The desktop inspector and editor for ZOE stores and servers.
_Avoid_: GUI, admin app

## Catalog and retrieval

**Mirror**:
An in-memory copy of the catalog indexes, versioned per tid, that answers catalog queries as of the client's snapshot.
_Avoid_: cache, replica

**Delta record**:
A record that says how one index entry changed in a transaction, instead of a rewritten index bucket.
_Avoid_: patch, diff

**Projection**:
The catalog kept only as delta records and snapshots, with no index buckets in the store.
_Avoid_: materialised view

**Queued indexing**:
The opt-in backend `indexing` of `zoe.plone` (ADR 0047): a save writes its catalog operations as keys of the queue, and writes no object of the catalog; the indexer applies them later.
_Avoid_: async indexing, deferred indexing

**Queue**:
The durable namespace `zoe.plone.indexing:` followed by the oid of the catalog, with one key for each catalog operation of a save: the time, a random part and the number of the operation, with the operation as JSON.
_Avoid_: indexing queue (that is the queue of CMFCore, in memory, for one transaction)

**Indexer**:
The object that applies the queue of one site in batches, on a connection of its own: each batch resolves each path in its snapshot, applies the removals first, and deletes its keys in the transaction that applies them, so each key applies once.
_Avoid_: worker, consumer

**Lease**:
The key of the ephemeral namespace `zoe.plone.indexing.lease` for one catalog, with the token of one thread and a TTL of 30 s; only the thread that holds it runs the indexer of that site, and each batch writes it again.
_Avoid_: lock (it is no per-OID lock), the Lease of Kubernetes (that elects the leader of deployment tier 2)

**Window**:
The time from the commit of a save to the commit of the batch that applies it; meanwhile the catalog shows the site as before the save.
_Avoid_: lag, delay (the delay of a key runs from the time in the key to the commit of its batch)

**Cookie wait**:
The wait of a request that carries the session cookie `zoe.indexing`: at `PubStart`, at most 2 s, until the queue holds no key at or below the last key of the save of that browser, so the user who saved reads a catalog that holds the save.
_Avoid_: sticky session, read-your-writes

**Derivation**:
A value computed from one record and keyed by oid, tid and kind, such as an image scale or an embedding.
_Avoid_: artefact, cache entry

## Way of working

**Card**:
One unit of work: one requirement, one acceptance test, one builder session, one pull request.
_Avoid_: ticket, task, story

**Phase**:
A band of the roadmap that ends with a gate.
_Avoid_: milestone, sprint, release

**Gate**:
A measurable check from the testing harness that must be green before the next phase starts.
_Avoid_: deadline, checkpoint

**Acceptance test**:
The test named on a card that decides it; it fails before the build and only the planner or the human changes it.
_Avoid_: spec test, definition test

**Scope fence**:
The paths a card may change.
_Avoid_: allowed files

**Spike**:
A card that ends in a written finding and no merged code.
_Avoid_: prototype, POC

**Walking skeleton**:
The thinnest end-to-end slice of a phase, built first, with its crash simulation attached.
_Avoid_: MVP, prototype

**Red zone**:
A path whose changes need the human's approval before merge.
_Avoid_: protected area, critical code

**Board**:
The project board whose columns hold the state of every card.
_Avoid_: kanban, backlog (Backlog is one column)

**Handoff**:
The file a session writes when it ends unfinished: state, what was tried, what failed, the next step.
_Avoid_: notes, summary

**Claims and evidence**:
The section of a pull request that lists what was tested, with the proof, and what was not.
_Avoid_: test plan

**Adopting site**:
A site that runs ZOE in production; it is a user of the project, not an owner.
_Avoid_: customer, client
