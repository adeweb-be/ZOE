# 0034. The client cache file

Status: accepted. The human accepted the layout and decided the questions that the spike ZOE-152 left open (finding: `docs/spikes/client-cache.md`). The ADR of the parallel spike ZOE-151 is 0036.

## Context

F28 asks for a native client with "an mmap'd compressed persistent cache", and F38 asks for "one shared mmap'd cache per node", where two workers on one node share one cache and apply each invalidation once. N4 asks for 3× ZEO 6 in phase 3, `read_hot` included, and `read_hot` reads only from the cache of the client. The build comes in three stages: a cache in memory in one process (ZOE-156), a persistent file (ZOE-162), and one file that the client processes of a node map together (F38).

`ZEO.cache.ClientCache` is the reference for the semantics. It keeps one entry per revision of an oid, with a start tid and an end tid (none for a current entry). `loadBefore` serves the entry whose interval holds the snapshot. `invalidate` ends the current entry at the tid of a new revision. The last tid of the cache is `lastTransaction` of `ZEO.ClientStorage`. Its file is a ring of blocks that it writes in FIFO order. At the open, it reads the whole file to build its index in Python dicts and BTrees. A lock file `<file>.lock` (`zc.lockfile`) stops a second process on the same file. It has no checksum, and it moves its last tid forward without a sync.

## Decision

**One layout for the three stages.** A cache is one mapping: an anonymous mapping in memory (stage 1), or a file that one process maps (stage 2) or that the processes of a node map together (stage 3). The same code reads and writes all three. The mapping holds:

- a header page: magic, size of the ring, number of index slots and their checksum; the boot id of the kernel; a clean flag; `applied`, the tid up to which the cache applied the invalidations; `head`, the logical write position of the ring; a checksum of these words that a clean close writes;
- an index of slots `(oid + 1, position)`, open addressing with linear probes. A slot points at the newest entry of its oid;
- a ring of entries. An entry holds its own logical position, the oid, the start tid, the end tid (0 for a current entry), the position of the older entry of the same oid, the stored and the raw length, the codec, a checksum of the data and a checksum of the head. Logical positions never repeat, so an entry that the ring overwrote fails its position check.

**Eviction is the FIFO ring of ZEO.** A new entry goes to the head of the ring and overwrites the oldest bytes. A hit of a current entry in the oldest fifth of the ring copies it forward, as the `rearrange` of ZEO does, when the writer lock is free. An entry larger than half the ring is not cached.

**A hit never takes a lock and never writes.** A reader loads `applied`, then the index slot, then the entry. It copies the entry, checks both checksums, then checks that the head of the ring stayed within one ring of the entry. A current entry serves only snapshots up to `applied`. A doubt is a miss.

**Compression is per entry, and off by default.** The codec is a field of each entry. zstd 1 halves a 2.8 KB pickle and divides a 100 KB pickle by three, but its decompression makes a hit through ZODB two to four times slower (5.6 against 10.4 µs, 26 against 103 µs). So entries are raw by default, and zstd per entry above a size is a setting. F28 reads "compression as a setting".

**Crash safety: on any doubt, the cache drops.** A process that dies leaves the page cache intact: the checksums drop a torn entry, and every other entry stays. A machine that restarted lost the page cache in an unknown order: when the boot id in the header differs and the file was not closed cleanly, the open drops every entry. A bad header, a bad length, another size or a bad state checksum drops every entry too. A drop never deletes the file while another process maps it.

**Stage 2 keeps the lock of ZEO.** A private cache file takes an exclusive `flock`, so a second process on the same file fails at the open, as with `zc.lockfile`. The file of the native client is named `{client}-{storage}.zoec`, so a native client never takes or overwrites the `.zec` file of a stock client.

**Stage 3 shares the file.** The key `shared-cache true` turns it on for a client with `client`. It is never the default. Each process holds a shared `flock`; a private open and a shared open of the same file exclude each other. Writers exclude each other with an open-file-description lock on byte 0 of the file, which the kernel releases when the process dies, and a mutex inside the process. Each process keeps its own last tid, which is its `lastTransaction`; `applied` belongs to the node. A process applies a transaction to the file only when `applied` is below its tid, under the writer lock, and moves `applied` after the ends: so each invalidation is applied once per node, and each process still gives it to its own DB. A process stores a current entry from a load only when `applied` is not above the tid up to which it had read its stream when the reply arrived; a store that runs later keeps that tid, not the tid of its own time. A verification that finds the cache too old clears it and moves `applied` in one hold of the writer lock, never in two. A process that lags gets more misses, never a stale entry. `clear` drops every entry of the node and keeps `applied`.

**The file is never the only copy of anything.** It holds only what the server gave or what a commit sent to the server, and the server confirmed it first.

## Consequences

- One code path for ZOE-156, ZOE-162 and the F38 card, and an open in constant time: no scan of the file, unlike ZEO.
- A hit costs a few hundred nanoseconds in the cache. Through ZODB, the rest of ZODB costs more than the cache: the finding gives the numbers.
- A crash of the machine drops a cache that was open. A clean close keeps it across a restart.
- The `end` of an entry and its head checksum change together. A reader that sees one change without the other misses once.
- With several processes, verification at the open departs from ZEO: `applied` above the tid of `register` is normal, so ZEO's `AssertionError` for a cache newer than the server stays for a private file only. A shared file detects another history with the identity of the storage in its header (see "Decided with the ADR").
- The layout is an on-disk format of the client. It is not the format of the store, so it is not a red zone, but a change of it changes the magic.

## Decided with the ADR

- **Compression.** Entries are raw by default. zstd per entry is a setting for sites that need capacity more than hit speed.
- **Another history.** A server restored from an older copy, then committed to once, has tids above the `applied` of a shared file, so no comparison of tids finds it. The header of the cache file holds the format version and an identity of the storage, as ZOE-162 asks. There is no extension at `register`, so the wire protocol does not change. A cache whose identity differs from the identity of the storage that it meets is dropped.
- **Reconnects** with a shared file: a process compares the tid of its new `register` with its own last tid, and its DB needs `getInvalidations` from its own last tid, not from `applied`. The verification after a reconnect is ZOE-159.

## Alternatives

- **The ZEO layout as it is.** It scans the whole file at the open, keeps the index in the memory of the process, cannot share the index between processes, and has no checksum.
- **One index per process over a shared ring.** Each process would scan the ring at its open and follow the writes of the others. The shared index costs less.
- **A robust mutex inside the mapping for the writers.** It is faster than a file lock when no other process holds it, but its recovery after the death of its owner is harder to get right. The lock is taken only on a miss, an invalidation and a commit.
- **One process of the node applies every invalidation.** A process that lags or dies then stops the whole node. With "the first one applies", no process waits for another.
- **zstd on every entry.** It doubles the cost of a hit of a small entry.
