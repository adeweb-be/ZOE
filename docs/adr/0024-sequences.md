# 0024. Sequences: one row per name, reserved in durable blocks of 100

Status: accepted

## Context

F29 asks for sequences: `storage.sequence(name).next()` gives values that increase per store, and the allocation stays outside the lock path of the two-phase commit. Plone allocates ids in its own transactions today: intids, the `_nextwid` of the ZCTextIndex lexicon and the `Length` counters. Two editors who create content at the same time write the same counter object, and one of them gets a `ConflictError`.

A value must never come twice, also after a crash. If each value were durable on its own, each `next` would cost one fsync. If no value were durable, a crash could give a value again. The data model has no key space for this state, so the sequences need one, and that is a format change.

The human decided the trade-off: values are reserved in durable blocks of 100, and a crash can leave a gap of up to 100 values. Values stay unique and strictly increasing per store, as RelStorage's oid allocation does.

## Decision

**The key space.** The `sequences` key space is a new redb table.

- The key is the name of the sequence, as UTF-8 text. Any string is a name, the empty string included.
- The value is a frame (ADR 0020) with no flags, codec 0, branch id 0, previous serial 0 and no typed-field slots.
- The payload is 8 bytes: the end of the reserved values, as a little-endian `u64`. The end is the first value that the store has not reserved. So every value that the store gave, or can give before the next reservation, is below the end.

The value of a row is 40 bytes:

| Offset | Size | Field | Value |
| --- | --- | --- | --- |
| 0 | 1 | frame version | 1 |
| 1 | 1 | flags | 0 |
| 2 | 1 | codec | 0 |
| 3 | 1 | reserved | 0 |
| 4 | 4 | branch id | 0 |
| 8 | 8 | previous serial | 0 |
| 16 | 4 | payload length | 8 |
| 20 | 2 | length of the typed-field slots | 0 |
| 22 | 2 | reserved | 0 |
| 24 | 8 | checksum | xxh3-64, seed 0, over the UTF-8 bytes of the name, bytes 0 to 23, then the payload |
| 32 | 8 | payload | the end, little-endian |

The checksum covers the name, so a row stored under another name fails it. A damaged row is a `CorruptSequence` error with the name, never a value. A row with a codec or a flag that the build does not know is a `Format` error, as for every frame. Eight bytes do not compress, so the writer always uses codec 0.

`Backend::sequence_end` reads the end of a name. `WriteTxn::put_sequence_end` stages a new end. These are two new required methods. No existing signature changes.

**Allocation.** In an open store, each name has a block in memory, the values from `next` up to `end`, behind a mutex of its own. The first use of a name reads the stored end, or takes 1 when the name has no row, and starts there with an empty block. `next()` takes the mutex, returns `next` and adds 1 to it. So the first value of a new sequence is 1, and the values of a name are consecutive across the threads of one open store.

**Reservation.** When the block is empty, `next()` reserves a new block while it holds the mutex of the name:

1. The new block starts at `next` and ends at `next + 100`.
2. A backend write transaction of its own stages the new end in the row of the name, and nothing else: no record, no transaction row, no `meta` row.
3. The transaction commits. redb syncs the file before the commit returns.
4. Only then does `next()` give the first value of the block.

A reservation that fails gives no value, and the next call tries the same block again. When no value below `u64::MAX` is left, `next()` fails with `Protocol`, as `new_oid` does.

**Outside the two-phase commit.** An allocation takes no per-OID lock and does not take the commit sequencer. It is not part of a transaction: an abort does not give its values back, and a value can be used in any transaction or in none. A reservation waits only for the mutex of its name and for the single writer of redb, so it can wait for a commit that is writing. An allocation from inside a block waits for nothing but the mutex of the name.

**Pack.** Pack never reads, changes or deletes the `sequences` key space. A row is not an object version: it has no oid and no tid, and no object refers to it. Its end must also survive every pack, else a later value could repeat one given before.

**Existing stores.** A new store creates the `sequences` table with the other tables. A store written before this table has none until its first reservation creates it, and until then it reads as a store without sequences. The format name stays `ZOE format 1`.

## Why a crash never gives a value twice

`next()` gives a value only after a durable row holds an end above that value. A reservation starts at the next value of the name, so the end that it writes is above every value given before. A failed reservation leaves the next value as it is, so its retry writes the same end, and the stored end never decreases. A reopened store starts each name at its stored end. So the first value after a reopen is above every value given before it, whatever the crash point.

Each reopen skips at most 100 values of a name. Take the last reservation of the name that reached the disk. It started at `s` and wrote `s + 100`. If the store gave values of that block, the last one is at least `s`, so the reopen skips at most 99 values. If it gave none, the reopen skips the whole block, `s` to `s + 99`: 100 values. A clean close writes nothing, so a clean reopen also skips the rest of the block.

The crash simulation checks this. The acceptance test gives 120 values, so two reservations run. Then it opens the store on each of the 1027 crash images of the run. On every image the first value is above the last value given, with a gap of at most 100.

## Why blocks of 100

- One fsync for every 100 values, not for each value. A content import that creates thousands of objects costs tens of reservations.
- A crash or a reopen costs at most 100 values of a 64-bit space.
- Intids, `_nextwid` and the like need values that are unique and increasing, not dense.

## Alternatives

- **A durable write for each value.** The values would be dense after a crash. Each `next` would cost one fsync and would wait for every commit that is writing.
- **A key in `meta` for each name.** `meta` holds fixed text keys and raw numbers, with no frame and no checksum. Names come from the application, so `meta` would become an open key space, and a damaged value would read as a number.
- **A persistent object in the store, such as a `Length`.** It goes through the two-phase commit, and that is the conflict that F29 removes.
- **The exact next value written at close.** A clean reopen would skip nothing. But a crash still skips values, so a caller must accept gaps in any case, and the close would need a write that can fail.
- **Larger blocks.** Fewer writes, but larger gaps. The human chose 100.

## Consequences

- An id allocation never conflicts and never waits for the locks of a transaction.
- Values are unique and strictly increasing per store and per name, but not dense: each crash or reopen can skip up to 100 values of each name that was used.
- The values of an aborted transaction are lost.
- A store holds one 40-byte row per name, plus the name. Names stay in the store, because nothing deletes a row.
- A copy through `iterator` and `restore`, as `zodbconvert` does, does not copy the sequences: the copy starts each name at 1. A copy that must keep the values given before has to copy the `sequences` rows too. No tool does that yet.
- Every backend implements `sequence_end` and `put_sequence_end`.
