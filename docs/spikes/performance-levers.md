# Finding: where the core spends its time on a load and a commit, and three levers that pay

Exploration on the branch `claude/performance-optimization-igkin7`, requirements N4 and N3. Three levers are built on the branch, with their tests; the others are measured and left as proposals.

**Verdict.** On `main`, the core pays zstd twice where it should pay it once or not at all: every record gets a new zstd context on each compression and each decompression, and the vote decompresses the current record of every written oid only to compare a tid that its key already holds. Three changes, with the stored bytes byte for byte the same and every test of `zoe-core` green:

1. **One zstd context per thread** (`format/codec.rs`, a red zone file; no data-model change). The human asked for it on #361 and #364.
2. **A vote that reads serials, not records** (`commit/mod.rs`, `backend.rs`, `redb_backend.rs`): `Backend::current_serials` reads the keys of the records and decodes nothing. Only a conflict that the caller can resolve loads its committed record.
3. **The next serial of a `load_before` from the key** (`redb_backend.rs`, `format/mod.rs`): the next record is still verified (its checksum), no longer decompressed. `format::verify_frame` checks a frame without decoding its payload.

On the machine below, with 1 KB records as zodbshootout writes them, the vote of an `update` of 100 objects goes from 2.3 ms to 0.11 ms, the finish of an `add` from 4.9 ms to 2.0 ms, the finish of an `update` from 6.5 ms to 3.7 ms, and a load of a 1 KB record from 25 µs to 3.4 µs in the core. The numbers below give each term.

## Question

Which terms of a load and of a commit in `zoe-core` are waste, which lever moves each, and what does each lever give on a machine of this class?

## Method

**Machine.** A cloud virtual machine with 4 vCPUs (Intel Xeon at 2.10 GHz, no cpufreq driver), 15.7 GB of memory, Linux 6.18, the store on the root disk, a virtual block device with ext4. It is not the reference machine of the harness: a sync of the database file costs 0.8 to 2.3 ms here, against 0.3 ms on the NVMe of the phase 3 spike, and a new zstd context costs about 18 µs here, against the 2 µs that ZOE-173 measured. So the gains below are larger than they will be on the reference machine, and the terms are the same.

**Builds.** The stable toolchain that `devenv.nix` pins (rustc 1.99), release profile. `main` is b7a38ee; the branch adds the three levers on top of it.

**Benchmarks**, all in `crates/zoe-core/tests`, each ignored and run in release, each in-process without a server:

- `finish_time.rs` (of ZOE-173), with the vote now timed too and `ZOE_FINISH_SIZE` for 100 KB records: `add` and `update` of 100 records in each transaction, compression off and zstd at level 3, 200 transactions of 1 KB and 40 of 100 KB, median and 90th percentile in µs. A record is half text that zstd shrinks, half pseudo-random bytes that it does not, as a zodbshootout record.
- `load_time.rs` (new): `load_before` of a 1 KB and of a 100 KB record whose pages are in the cache, compression off and zstd: the current record of an object with one version, the newest of two, and the older of two (a reader at an earlier tid, with a next serial to give). 10,000 loads at 1 KB, 300 at 100 KB.
- A scratch benchmark of libzstd alone (zstd 1.5.7 through the `zstd` crate): one record, a new context against a kept one, at the levels 3, 1, −1 and −3, on 1 KB and 100 KB of words.
- A scratch benchmark of redb alone: the cost of `begin_read` and `open_table` against a lookup on an open table.
- `zoe-bench load` against `zoe serve`, 16 clients, 1 KB records, with `--write-ratio 1.0` (each transaction reads 10 objects and writes them back) and with `--write-ratio 0`, two rounds of each on both builds.

The spans of N17 did not run. Each number is one run; the finish numbers of two runs of the same build differ by up to 10 %, so a difference under that is noise.

## Results

### The finish at 1 KB, in µs (median, then p90)

| scenario | compression | build | vote | finish | sync of the file | finish less sync | bytes written |
| --- | --- | --- | --- | --- | --- | --- | --- |
| add | none | main | 69 / 86 | 1,805 / 2,209 | 1,012 / 1,304 | 770 / 977 | 189,432 |
| add | none | branch | 87 / 105 | 2,001 / 2,422 | 974 / 1,291 | 1,045 / 1,178 | 189,432 |
| update | none | main | 229 / 287 | 3,480 / 5,782 | 2,214 / 3,089 | 1,347 / 2,766 | 641,323 |
| update | none | branch | 114 / 151 | 3,266 / 4,893 | 2,083 / 2,909 | 1,318 / 1,993 | 641,323 |
| add | zstd | main | 83 / 98 | 4,864 / 5,334 | 856 / 1,207 | 4,052 / 4,274 | 118,571 |
| add | zstd | branch | 61 / 87 | 2,019 / 2,697 | 741 / 935 | 1,243 / 1,906 | 118,571 |
| update | zstd | main | 2,333 / 2,474 | 6,467 / 8,665 | 2,077 / 3,395 | 4,616 / 5,117 | 492,945 |
| update | zstd | branch | 113 / 147 | 3,694 / 4,618 | 1,677 / 2,081 | 1,944 / 2,674 | 492,945 |

- With zstd, the finish less its sync falls from 4.1 ms to 1.2 ms on `add` and from 4.6 ms to 1.9 ms on `update`: the 100 compressions of a transaction cost 2.7 ms with a new context each, 0.9 ms with a kept one. The bytes written are the same, so the frames are the same.
- The vote of `update` falls from 2.3 ms to 0.11 ms with zstd (100 decompressions with a new context each, then none), and from 0.23 ms to 0.11 ms without compression (100 copies of the current record, then none). The vote of `add` reads nothing in both builds.
- The sync of the file is the floor of this disk: 0.7 to 1.0 ms for the 29 to 47 pages of an `add`, 1.7 to 2.2 ms for the 121 to 157 pages of an `update`. It is what ZOE-184 (the write-ahead log spike) wants to move.

### The finish at 100 KB, in µs (median, then p90), 40 transactions

| scenario | compression | build | vote | finish | sync of the file | finish less sync | bytes written |
| --- | --- | --- | --- | --- | --- | --- | --- |
| add | none | main | 96 / 130 | 132,826 / 148,801 | 14,712 / 16,057 | 118,292 / 132,965 | 13,159,641 |
| add | none | branch | 117 / 148 | 153,299 / 181,096 | 14,729 / 17,686 | 133,947 / 165,219 | 13,159,641 |
| update | none | main | 8,947 / 52,209 | 33,342 / 101,314 | 14,787 / 16,831 | 18,650 / 85,787 | 13,166,400 |
| update | none | branch | 233 / 276 | 43,507 / 138,133 | 14,510 / 17,786 | 28,826 / 123,449 | 13,166,400 |
| add | zstd | main | 74 / 114 | 19,362 / 25,801 | 8,465 / 9,492 | 10,455 / 17,489 | 6,606,041 |
| add | zstd | branch | 92 / 118 | 19,928 / 22,138 | 8,702 / 9,956 | 11,081 / 13,335 | 6,606,041 |
| update | zstd | main | 6,867 / 11,704 | 21,729 / 24,545 | 9,099 / 10,898 | 12,815 / 14,031 | 6,612,800 |
| update | zstd | branch | 194 / 255 | 18,940 / 44,437 | 8,737 / 9,461 | 10,258 / 35,459 | 6,612,800 |

- The vote of an `update` of 100 records of 100 KB falls from 6.9 ms to 0.19 ms with zstd: 100 decompressions of 100 KB, each with a new context, for nothing. Without compression it falls from 8.9 ms to 0.23 ms: 100 copies of 100 KB.
- The finish at 100 KB is the disk of this machine: without compression it writes 13 MB in 113 writes, and the writes alone take 56 to 65 ms, with a wide spread; with zstd it writes 6.6 MB and the finish takes 19 to 22 ms. So on this disk zstd makes the 100 KB commit 6 times faster, which says nothing about the NVMe of the reference machine. The finish numbers of the two builds differ by less than their spread.
- With zstd, the finish less its sync is 10 to 13 ms on both builds: it holds the 100 compressions (about 7 ms for these half-random records, from the difference with the scratch numbers) and the inserts of 6.6 MB. A kept context moves 2 ms of it at most, so lever 1 pays at 1 KB, and ZOE-172 is what moves the 100 KB commit.

### A load in the core, in µs (median, then p90)

| record | compression | build | current, one version | newest of two | older of two |
| --- | --- | --- | --- | --- | --- |
| 1 KB | none | main | 2.9 / 3.2 | 3.0 / 3.4 | 3.5 / 3.7 |
| 1 KB | none | branch | 2.0 / 2.2 | 2.0 / 2.3 | 2.3 / 2.6 |
| 1 KB | zstd | main | 25.1 / 26.6 | 24.7 / 26.2 | 33.4 / 44.8 |
| 1 KB | zstd | branch | 3.4 / 3.7 | 3.4 / 3.8 | 3.7 / 4.0 |
| 100 KB | none | main | 17.7 / 28.6 | 20.9 / 28.1 | 35.7 / 43.1 |
| 100 KB | none | branch | 13.0 / 26.0 | 12.4 / 22.3 | 29.1 / 36.1 |
| 100 KB | zstd | main | 48.5 / 56.6 | 49.0 / 60.9 | 94.7 / 109.9 |
| 100 KB | zstd | branch | 35.7 / 42.7 | 36.4 / 40.7 | 44.6 / 51.2 |

- At 1 KB with zstd, a load costs 3.4 µs in place of 25: the new decompression context was 90 % of it. The core's share of a cold load through the server (the phase 3 spike measured about 70 µs for a call of its client at 1 KB, on its machine) falls by about 20 µs on this one.
- At 100 KB with zstd, the decompression of the half-random record takes about 20 µs of the 36; the new context took 13 µs more.
- The older of two with zstd at 100 KB falls from 95 µs to 45: the next record is verified (xxh3 over its stored bytes, 9 µs for a 50 KB payload) and not decompressed. A reader whose snapshot is behind the current record pays this on every load of an object that changed since.

### libzstd alone, one record, in µs per call

| record | level | encode, new context | encode, kept context | decode, new context | decode, kept context | ratio |
| --- | --- | --- | --- | --- | --- | --- |
| 1 KB | 3 | 27.0 | 8.9 | 19.2 | 5.1 | 3.00 |
| 1 KB | 1 | 24.8 | 7.9 | 18.6 | 3.7 | 2.84 |
| 1 KB | −1 | 25.4 | 6.3 | 22.2 | 2.3 | 2.39 |
| 1 KB | −3 | 21.0 | 5.1 | 20.9 | 1.9 | 2.23 |
| 100 KB | 3 | 400 | 380 | 129 | 107 | 5.05 |
| 100 KB | 1 | 339 | 383 | 258 | 206 | 4.48 |
| 100 KB | −1 | 480 | 430 | 146 | 155 | 4.36 |
| 100 KB | −3 | 373 | 356 | 155 | 125 | 4.29 |

The records are words from a small vocabulary, which zstd shrinks 3 to 5 times; the records of zodbshootout shrink about 2 times, so their encode is cheaper. A kept context saves 18 µs on each encode and 14 µs on each decode of a 1 KB record, and 20 µs on a 100 KB one. The level moves little at 100 KB on this data, and at 1 KB it buys at most 2 µs of encode for 6 to 20 % more bytes. The level is already an option of the store (`Options::compression`), so no card is needed for it.

### redb alone, in µs per call

| call | cost |
| --- | --- |
| a range lookup on an open table | 0.93 |
| `begin_read`, then `open_table`, then the range lookup | 1.68 |
| `begin_read` alone | 0.29 |
| `open_table` alone | 0.48 |

A load opens a read transaction and the table `records` on each call: 0.75 µs of the 2 to 3.4 µs of a 1 KB load in the core. A read transaction kept open and refreshed when the last tid changes would save it. The server round trip is 20 µs or more, so this lever is small at the gate; it matters for the in-process storage and for N3.

### zoe serve end to end, zoe-bench load, 16 clients, 1 KB records

Each transaction reads 10 of 2,000 objects (Zipf 0.8). With `--write-ratio 1.0` it writes them back, 30 transactions per client; with `--write-ratio 0`, 200 per client. Two rounds of each on each build, in turn.

| run | build | loads per second | commits per second | load p50 | load p99 | conflicts per 1,000 commits |
| --- | --- | --- | --- | --- | --- | --- |
| reads | main | 31,449 and 30,230 | | 479 and 494 µs | 1,149 and 1,238 µs | |
| reads | branch | 39,669 and 40,476 | | 369 and 364 µs | 1,061 and 1,010 µs | |
| writes | main | 13,969 and 13,737 | 220 and 231 | 258 and 255 µs | 1,010 and 1,046 µs | 5,344 and 4,942 |
| writes | branch | 18,395 and 18,829 | 307 and 313 | 275 and 277 µs | 1,047 and 1,159 µs | 4,985 and 5,023 |

- Reads: 30 % more loads per second, and a p50 that falls by 110 µs: the 20 µs of the decompression context, times the 16 clients that share 4 cores.
- Writes: 36 % more commits per second. The run is a hot-spot run (five conflicts per commit, each retried), so the vote and the finish decide it: the vote of a transaction that writes back 10 objects no longer decompresses 10 records, and the finish compresses them with a kept context.

## The levers

### Taken on the branch

1. **One zstd context per thread.** `codec.rs` keeps a `Compressor` with its level and a `Decompressor` in a `thread_local`. The tests check that a kept context writes the bytes of a new one at several levels, and that a refused payload leaves the context usable. The server runs the core on the blocking threads of tokio, which end after a short idle time, so the contexts stay bounded and go when the threads go. A context holds a workspace sized to the largest record it saw (about 1 MB at level 3 for a record of 100 KB or more).
2. **A vote that reads serials.** `Backend::current_serials` has a default on `load_current`, and the redb backend reads the keys only. `check_serials` loads the committed record of a stale written oid only when the caller can resolve it (`vote_resolving`, the serial is not zero, the pickle is not a deletion), one load per conflict. A corrupt current record no longer fails the vote of a transaction that writes the oid: the frame is read when the record is loaded or verified, not when it is superseded. This is the vote of the ZOE-077 prototype, which the spike measured and `main` never took.
3. **The next serial from the key.** `load_before_next` verifies the frame of the next record with `format::verify_frame` and takes its tid from the key. The test `every_read_path_verifies_the_checksum` still passes: a corrupt next record is still `CorruptRecord`.

The changes touch `crates/zoe-core/src/format/` (codec.rs, mod.rs), a red zone, with no change of the data model: the stored frames are the same bytes, and `bit_flips`, `compression` and the format tests are green. The PR needs the human's approval.

### Measured and left as cards

4. **Compression off the leader of a group commit (ZOE-172, #361).** With a kept context, 100 records of 1 KB cost the leader 0.9 ms and 100 records of 100 KB cost it the number in the 100 KB table above, in series, for every member of its batch. The human chose the pool design on #361; the numbers above give its bound on this machine: at 1 KB the compression is now 45 % of the finish less its sync, at 100 KB it is most of it.
5. **A read transaction kept open in the backend.** 0.75 µs of a load, see the redb table. It needs a refresh rule (the last tid) and a per-thread or mutex-guarded `ReadTransaction`, which pins pages until the refresh. Worth a card for N3 and the in-process storage, not for the gate.
6. **The second copy of a reply in `zoe serve` (ZOE-175, #364).** `message::reply_of` copies the record into the msgpack reply, then `frame::write` copies the reply again into a frame with its length. The second copy is a `memcpy` of the reply and an allocation per call: about 10 µs for 100 KB on this machine, under 1 µs at 1 KB. The card already names it.
7. **The copy into `bytes` in the in-process storage.** `zoe._zoe.Storage.loadBefore` builds the `bytes` from the `Vec` of the core: one copy of the record. The native client avoids it with `PyBytes::new_with_writer`; the core would need to decode into a buffer of the caller.
8. **The level of zstd.** Already an option. On this data, level 1 or −1 buys little; see the libzstd table.

## Limits

- One machine, a virtual one, whose sync and context costs are not those of the reference machine. The terms hold; the ratios do not transfer.
- The core benchmarks run in one thread. A finish at concurrency 16 batches 16 members on the leader, so the gain of levers 1 and 2 there is per member, and lever 4 is what moves the leader.
- No span of the server was measured. The end-to-end numbers come from `zoe-bench`, whose client is `zoe-client` and not a ZODB connection.
- The vote change leaves a corrupt current record undetected at the vote. `zoe verify`, a load and the bit-flip harness still find it.

## Tests run

- `cargo test -p zoe-core`: every binary green, the bit-flip and crash tests included. `cargo clippy --workspace --all-targets -- -D warnings` and `cargo fmt --all --check` pass.
- `cargo test -p zoe-server`: 54 binaries green. The six tests of `tls_client_ca.rs` did not run: they call `openssl req` with `-not_before`, which the OpenSSL 3.0 of this machine does not have, and devenv pins a newer one. They do not reach the changed code.
- The Python tests on CPython 3.13: `test_zoe_storage.py` (the storage suites of ZODB), `acceptance/test_storage.py`, `acceptance/test_conflict_resolution.py` and `test_zoe_iteration.py`: 127 passed, 8 skipped. The other interpreters and the rest of `just test` did not run here.
