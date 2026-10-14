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

**Intent lock**:
An exclusive per-OID lock that a client asks for when a transaction first changes an object, before the vote.
_Avoid_: early lock, pessimistic lock

**Conflict**:
Two transactions that write the same oid, where the second one read a serial that is no longer current.
_Avoid_: collision, ConflictError (that is the exception, not the event)

**Conflict tier**:
One of the three levels that try to resolve a conflict: structural merges, field-level merges by policy, and resolution on the client.
_Avoid_: tier (alone; see Deployment tier)

**Merge policy**:
A versioned object in the store that says which classes merge field by field and with which strategy per field.
_Avoid_: resolver config, merge rules

**Shadow mode**:
A mode where the conflict tiers compute a merge but the conflict is still raised, to show what merging would have done.
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
The trait through which `zoe-client` reaches the network, its tasks and the time (connect, spawn, sleep); `Tokio` is the runtime of production, and a simulator gives its own.
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
