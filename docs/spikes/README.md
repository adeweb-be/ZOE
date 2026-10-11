# Spikes

A spike is a card that explores an unknown and ends in a written finding, with no merged code. Each finding is one file in this directory, named after the question it answers. A finding gives the question, the method, the numbers and the verdict.

| Finding | Question |
| --- | --- |
| [free-threading-upstream.md](free-threading-upstream.md) | Can persistent, BTrees and zodbpickle declare that they need no GIL, and what must change first? |
| [pickle-round-trip.md](pickle-round-trip.md) | Can zoe-pickle decode a ZODB record into a tree and encode it back to the same bytes, and which constructs must stay opaque? |
| [record-store.md](record-store.md) | Does redb or an append-only log with an in-memory index write fewer bytes and fewer fsyncs per commit, and which redb commit setting should the store use? |
| [zeo-wire.md](zeo-wire.md) | What do a stock ZEO 6 server and an unmodified `ZEO.ClientStorage` send each other over msgpack, and where does the specification differ? |
