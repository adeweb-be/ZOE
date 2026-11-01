# Spikes

A spike is a card that explores an unknown and ends in a written finding, with no merged code. Each finding is one file in this directory, named after the question it answers. A finding gives the question, the method, the numbers and the verdict.

The files stay in this directory, also when a decision settled them: tests read some of them, and the code and the fuzz corpus read the traces of `zeo-wire/`.

## Reference

These findings still describe the code and its dependencies.

| Finding | Question |
| --- | --- |
| [zeo-wire.md](zeo-wire.md) | What do a stock ZEO 6 server and an unmodified `ZEO.ClientStorage` send each other over msgpack, and where does the specification differ? The traces of `zeo-wire/` are the wire contract that the tests of `zoe-server` and its fuzz corpus read. |
| [pickle-round-trip.md](pickle-round-trip.md) | Can zoe-pickle decode a ZODB record into a tree and encode it back to the same bytes, and which constructs must stay opaque? |
| [free-threading-upstream.md](free-threading-upstream.md) | Can persistent, BTrees and zodbpickle declare that they need no GIL, and what must change first? |
| [load-path.md](load-path.md) | Where does a load through ZODB on the native client spend its time between the ghost and the client cache, on a hit and on a miss, and which terms do a prefetch (ZOE-273) and a path in Rust (ZOE-274) remove? |

## Archive

A decision settled each of these findings. Read the decision first: the numbers of a finding come from its machine and from the code of its time.

| Finding | Verdict | Settled by |
| --- | --- | --- |
| [record-store.md](record-store.md) | Keep redb with its default commit. The append-only log fails N6 and N7 as built, and quick repair writes the allocator state at each commit. | redb stays the backend, with its default commit (ADR 0002). |
| [deterministic-simulation.md](deterministic-simulation.md) | Build an executor of our own. Neither turmoil nor madsim runs the blocking lock wait of the core on simulated time. | The simulator is the `sim` module of `zoe-server` (N18). |
| [client-cache.md](client-cache.md) | One layout serves the cache in memory, in a private file and in a file that the processes of a node share. A hit takes no lock and writes nothing. | ADR 0034. |
| [concurrent-commits.md](concurrent-commits.md) | A pool of connections with one ordered stream of invalidations keeps the contract of ZODB, with no change of the server or of the wire protocol. | ADR 0036. |
| [phase3-budget.md](phase3-budget.md) | A reduced run reaches 3× ZEO in 26 of 140 rows. Only 8 rows have a bound that no card can move. | ADR 0037, which ADR 0039 supersedes. |
| [performance-levers.md](performance-levers.md) | The core paid zstd where it did not need to. Three levers make the vote, the finish and a load faster, with the same stored bytes. | The three levers are in `zoe-core`. |
| [posting-dicts.md](posting-dicts.md) | Most saves that fail at their first attempt fail on the posting dicts of the text indexes. A store with tree postings changes almost nothing; a merge at the vote can. | ADR 0042. |
| [early-lock-hook.md](early-lock-hook.md) | ZODB's `register` hook sees the first change of each object. A lock there does not prevent a conflict on an object that cannot merge, because the writer keeps its stale snapshot. | ADR 0044 and ADR 0045. |
| [log-replay.md](log-replay.md) | The probe records of a B5 run give the start of each transaction of its kept store, so a replay rebuilds the first-attempt failures of the run. | The replay is `bench/replay` (B6). |
| [queued-indexing.md](queued-indexing.md) | The catalog is the source of the conflicts of saves. The processor of the indexing queue of CMFCore writes each catalog object of a save. | ADR 0047. |
| [concurrent-creations.md](concurrent-creations.md) | The conflicts of concurrent creations are splits of four shared BTrees whose keys grow. A merge of the node and its split buckets removes most of them. | ADR 0048. |
| [catalog-metadata.md](catalog-metadata.md) | Tier 1 merges the metadata buckets of a catalog as ZODB's resolver does when it models `DateTime`, `Missing.Value` and the i18n `Message`. | The heap of tier 1 models these values. |
| [split-merge-replaced-bucket.md](split-merge-replaced-bucket.md) | Each merge of rule 3 replaced a bucket, and 18 of 28 conflicts were inserts into the replaced bucket. A rule 3 that keeps the bucket removes them. | ADR 0049. |
| [folder-order.md](folder-order.md) | A creation writes six records of its folder. A merge that appends the ids of the transaction to the order and renumbers their positions keeps the order and `pos` in agreement. | ADR 0052. |
| [server-write-volume.md](server-write-volume.md) | Most of the gap of 20× in B5 is in the measure. At the system call, `zoe serve` writes 5.7 to 6.6 times the bytes of FileStorage, because redb rewrites pages. | B5 counts the bytes at the system call. The human left the write volume as it is. |
| [write-log.md](write-log.md) | A write-ahead log with a background checkpoint makes the finish of `update` faster, with conditions. | The human decided that ZOE writes no log engine of its own. A write path with a log needs a backend whose engine has one. |
| [zope-sessions.md](zope-sessions.md) | No path of Plone 6.1 and no maintained add-on writes a Zope session on a standard site. | ADR 0043. The human deferred the session backend. |

## Known upstream issues

Each finding gives a fault of a dependency of ZOE on CPython 3.14t, its cause and its fix.

| Finding | Issue |
| --- | --- |
| [persistent-inline-values.md](../findings/persistent-inline-values.md) | persistent loses the state of an object. |
| [zodbpickle-free-threading.md](../findings/zodbpickle-free-threading.md) | zodbpickle turns the GIL on at `import ZODB`. |
| [btrees-free-threading.md](../findings/btrees-free-threading.md) | BTrees turns the GIL on in a ZODB application. |
