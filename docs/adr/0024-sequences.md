# 0024. Sequences: one row per name, reserved in durable blocks of 100

Status: accepted. The checksum input of a sequence row starts with the tag 5 of `sequences` (ADR 0020, section 3).

## Context

F29 asks for sequences: `storage.sequence(name).next()` gives values that increase per store, outside the lock path of the two-phase commit. Plone allocates ids in its own transactions (intids, the `_nextwid` of the ZCTextIndex lexicon, `Length` counters), so two editors who create content at the same time conflict on one counter.

A value must never come twice, also after a crash. A durable write for each value costs one fsync per `next`. No durable write lets a crash give a value again. The human chose durable blocks of 100: each crash or reopen can skip up to 100 values, and values stay unique and strictly increasing per store, as in the oid allocation of RelStorage. Intids and `_nextwid` need unique, increasing values, not dense ones.

## Decision

**The key space.** `sequences` is a redb table with one row per name. The key is the name as UTF-8 text; any string is a name, the empty one included. The value is a 40-byte frame (ADR 0020) with codec 0, no flags, branch id 0, previous serial 0 and no typed-field slots. Its 8-byte payload is the end: the first value that the store has not reserved, a little-endian `u64`. The checksum covers the name, so a row under another name fails it. A damaged row gives `CorruptSequence` with the name, never a value. The layout: specification, "Storage core", "Data model" (row `sequences`) and "Record framing: checksums, compression, encryption". `Backend::sequence_end` reads an end and `WriteTxn::put_sequence_end` stages one.

**Allocation.** In an open store, each name has a block in memory, from `next` up to `end`, behind a mutex of its own. The first use reads the stored end, or 1 when the name has no row. `next()` returns `next` and adds 1. So a new sequence starts at 1, and its values are consecutive across the threads of one open store.

**Reservation.** When the block is empty, `next()` reserves the block from `next` to `next + 100` while it holds the mutex of the name. A backend write transaction of its own stages the new end alone, with no record, no transaction row and no `meta` row, and commits with a sync. Only then does `next()` give a value of the block. A failed reservation gives no value, and the next call tries the same block again. When no value below `u64::MAX` is left, `next()` fails with `Protocol`, as `new_oid` does.

**Outside the two-phase commit.** An allocation takes no per-OID lock and not the commit sequencer. It is not part of a transaction: an abort keeps its values. A reservation waits only for the mutex of its name and the single writer of redb.

**Pack.** Pack never reads, changes or deletes a sequence row: a row is not an object version, and its end must survive every pack.

**Existing stores.** A store without the table has no sequence until its first reservation creates the table. The table needs no new format name.

**Why no value comes twice.** `next()` gives a value only after a durable row holds an end above it, the stored end never decreases, and a reopen starts each name at its stored end. Each reopen skips at most 100 values, but the skips of reopens in a row add up: a session that reserves a block and gives none of it adds the whole block to the gap. The acceptance test, `crates/zoe-core/tests/sequences.rs`, opens the store on each crash image of a run of 120 values and checks that the first value is above the last one given, with a gap of at most 100.

## Consequences

- An id allocation never conflicts and never waits for the locks of a transaction.
- One `next()` in 100 of each name waits for the single writer of redb, so for a commit fsync or a pack batch, and the other callers of that name wait with it.
- Values are unique and strictly increasing per store and per name, not dense.
- The values of an aborted transaction are lost.
- Nothing deletes a row, so each name stays in the store.
- A copy through `iterator` and `restore`, as `zodbconvert` does, starts each name at 1. The dumps of `zoe backup` keep the end of each sequence (ADR 0030).

## Alternatives

- **A durable write for each value.** One fsync per `next`, and a wait for each commit that writes.
- **A key in `meta` for each name.** `meta` holds fixed keys with no frame and no checksum; names from the application would make it an open key space.
- **A persistent object, such as a `Length`.** It goes through the two-phase commit, which is the conflict that F29 removes.
- **The exact next value written at close.** A crash still skips values, and the close needs a write that can fail.
- **Larger blocks.** Fewer writes, larger gaps.
