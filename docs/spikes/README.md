# Spikes

A spike is a card that explores an unknown and ends in a written finding, with no merged code. Each finding is one file in this directory, named after the question it answers. A finding gives the question, the method, the numbers and the verdict.

| Finding | Question |
| --- | --- |
| [client-cache.md](client-cache.md) | Which layout lets the native client keep one cache in memory, in a private file and in a file that the processes of a node share, and what does a hit cost through ZODB? |
| [concurrent-commits.md](concurrent-commits.md) | Can one client process run several transactions at once, and which rule keeps the invalidation contract of ZODB? |
| [deterministic-simulation.md](deterministic-simulation.md) | How do the server and its clients run in one process on a simulated network and clock with seeded schedules, and should the simulator use turmoil, madsim or an executor of our own? |
| [free-threading-upstream.md](free-threading-upstream.md) | Can persistent, BTrees and zodbpickle declare that they need no GIL, and what must change first? |
| [phase3-budget.md](phase3-budget.md) | What bounds each cell of the 3× gate of phase 3, and which cells can no card move? |
| [performance-levers.md](performance-levers.md) | Where does the core spend its time on a load and a commit, which terms are waste, and what does each lever give? |
| [write-log.md](write-log.md) | Does a write-ahead log with a background checkpoint buy enough of the commit rows to pay for its complexity, and what does it cost on the reads, at recovery and in the rest of the store? |
| [log-replay.md](log-replay.md) | How does the replay rebuild the concurrency of a B5 run from its kept store and its probe records, and does it on one run? |
| [pickle-round-trip.md](pickle-round-trip.md) | Can zoe-pickle decode a ZODB record into a tree and encode it back to the same bytes, and which constructs must stay opaque? |
| [record-store.md](record-store.md) | Does redb or an append-only log with an in-memory index write fewer bytes and fewer fsyncs per commit, and which redb commit setting should the store use? |
| [zeo-wire.md](zeo-wire.md) | What do a stock ZEO 6 server and an unmodified `ZEO.ClientStorage` send each other over msgpack, and where does the specification differ? |
| [queued-indexing.md](queued-indexing.md) | Where do the catalog conflicts of B5 come from, how does the indexing queue of CMFCore process a transaction, and what does Plone read from the catalog right after a save? |
| [concurrent-creations.md](concurrent-creations.md) | Which trees do concurrent creations of versioned content conflict on after queued indexing, why does ZODB refuse each case, and what does each candidate remove? |
| [server-write-volume.md](server-write-volume.md) | Where do the bytes that `zoe serve` writes for one B5 operation go, why does B5 show 20 times the bytes of ZEO, and which levers reduce them? |
