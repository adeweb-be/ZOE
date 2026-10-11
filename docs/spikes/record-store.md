# Spike: redb versus an append-only log for the record store

Card ZOE-010, requirements N4 and N5. The open after a crash also touches N6 and N7.

**Verdict.** Keep redb with its default commit, and do not build the append-only log now. On the commit path of the walking skeleton, every kind makes one commit per fsync, so none reaches N5: N5 needs group commit. With a best-case group commit, every kind reaches N5 for small transactions (one or 10 objects of 1 KB, or one of 100 KB) with 64 writers: for one 1 KB object, 5,191 commits/s with redb, 2,819 with quick repair and 6,265 with the log. For larger transactions, redb's CPU time per record limits the batch: 719 commits/s for 100 objects of 1 KB. Counted at `write`, the log writes 1.00 to 1.13 bytes per pickle byte, and redb 1.3 to 2.1 for 100 KB objects and 5.8 to 39 for 1 KB objects. Quick repair makes the open after a crash fast at every size (30 ms against 6.6 s on a 2.0 GB file, cold), but every commit then writes redb's whole allocator state, about 128 bytes per MB of file: 4.3 MB per one-object commit on a 34 GB store, which made 14 commits/s on the disk against 92 with the default commit. So leave quick repair off. A default store then takes longer to open after a crash as it grows, which N7 does not allow above about 0.6 GB with a cold page cache. The decisions for the human are at the end.

## Question

On the commit path of the walking skeleton, how many bytes does each record store write per byte committed, how many fsyncs does it make per commit, and how many commits per second does it reach on a device whose fsync takes 10 ms? The stores are redb with its default commit, redb with quick repair, and an append-only log with an in-memory index. What do the numbers mean for N4 and N5? How long does each store take to open after a crash, and which redb commit setting should the store use?

## Method

Units: KB, MB and GB are binary in the whole finding. 1 KB is 1,024 bytes, 1 MB is 1,024 KB, 1 GB is 1,024 MB.

1. **Backends.** The redb backend of the walking skeleton, on redb 4.3.0, with its on-disk format unchanged, in two settings. *redb* is the default commit: one phase, one fsync. *redb-quick-repair* calls `WriteTransaction::set_quick_repair(true)` on every commit: redb then commits in two phases, with two fsyncs, and saves its allocator state in each commit. *log* is an append-only log with an in-memory index, written for this spike (`crates/zoe-core/src/log_backend.rs` on the card branch). The log is one file. Each entry is a kind byte, the key of the row, then the frame of ADR 0020, so both backends store the same frames. A commit appends its records, then its transaction row, in one write, and syncs once. The transaction row is the commit marker. Open scans the whole log, checks every checksum, rebuilds the index `oid → [(tid, offset, length)]`, and truncates a torn tail. The log has no pack and no persisted index. In the `MemVfs` crash simulation, every crash image of two log commits opens and keeps every acknowledged commit. The same simulation passes for a redb store with quick repair on, for one commit.
2. **One commit path.** Every kind runs through `Store`: `tpc_begin`, `store`, `vote`, `finish`. Three hidden entry points on the card branch make this possible: `Store::open_with_quick_repair`, `Store::from_backend`, and `Store::backend`, which the group-commit estimate uses. The `Backend` and `WriteTxn` signatures do not change. The walking-skeleton commit path has no group commit: `finish` holds the commit sequencer for the whole backend commit, fsync included.
3. **Workload.** In each cell, `writers` threads each own `objects_per_txn` oids. A setup commit creates them, outside the measurement. Each measured transaction then writes a new version of every object of its writer, with pickles of `object_size` bytes. A cell runs at least 100 commits and at least 3 per writer, but at most 1 GB of pickles, and never fewer than one commit per writer. So the cells of 64 writers with 100 objects of 100 KB run 104 commits.
4. **Device.** The store files live in memory (`RamVfs`). A metering wrapper counts the bytes that the backend passes to `write`, the calls to `set_len` and the syncs. Each sync sleeps 10 ms: this emulates a device whose fsync costs 10 ms. A write costs only a memory copy, so the commit rates do not include the write bandwidth of a real device. Write amplification is the bytes passed to `write` ÷ the pickle bytes committed. These are not the bytes that reach the device. A log commit of one 1 KB object appends about 1.1 KB, but on a real file each fdatasync writes at least the whole 4 KB page that holds the new bytes, plus a commit of the ext4 journal for the new file size. redb writes whole pages in place. So at the device, the gap between redb and the log is much smaller than the ratio of the counted bytes, for example 34 against 1.13.
5. **Group-commit estimate.** For the cells with 16 and 64 writers, `measure_batched` skips the `Store` and writes one transaction per writer in one backend write transaction, with one commit, for up to 25 batches. This is the best case of a group commit: every batch is full, and nothing waits for a lock or a wake-up. The commit latency is the time of the batch.
6. **Scattered updates.** In the cells above, the objects of a writer are neighbours in the oid order, in a small store. `measure_scattered` first fills a store with 10,000, 100,000 or 1,000,000 records of 1 KB. Then one writer commits 100 transactions that update 1, 10 or 100 of these records, at equal distances in the oid order, so that a B-tree commit touches one path per object.
7. **Big stores on the disk.** `measure_big_store` fills a redb store on the disk with 1 MB pickles, 100 per transaction, to 1, 4 and 16 GB of pickles, and closes it. Then it reopens the store with each redb setting and counts 50 commits of one new 1 KB object, from one writer, without group commit. The fsyncs are the real fsyncs of the disk.
8. **Open after a crash.** A writer process commits transactions of 100 new 1 KB objects to a store on the disk, with real fsyncs, until the store holds 10,000, 100,000 or 1,000,000 records. It goes on committing, and the parent kills it with SIGKILL 3 to 7 ms later, so the kill lands in a commit. Each measurement opens a fresh copy of that directory, synced first. A cold open first drops the files from the page cache (`dd iflag=nocache`; `fincore` showed that no page stayed). A warm open finds them in the page cache. Every open checks that the last tid that the writer announced is in the store. After the crash open, the store closes cleanly, and two more opens, warm then cold, give the time of a normal restart. Each kind runs in its own process, so that the memory growth of an open is not hidden by memory that an earlier open freed.
9. **Repeats.** The commit cells ran 5 times, the kinds taking turns in each cell. The scattered updates ran 3 times. The opens ran 5 times on the images of 10,000 and 100,000 records and 3 times on those of 1,000,000. The big stores ran once. The tables give the median and, when it is wider than the precision, the range.
10. **Machine.** A 22-thread laptop CPU, 32 GB of memory, Linux 7.0, a 1 TB NVMe SSD with ext4 on LVM. Other jobs ran on the machine at the same time. The load average was 3 to 7 during the commit cells, 7.6 to 13.5 during the big-store run, and up to 20 during the opens. The emulated fsync dominates most commit rates, so the noise shows most in the open times, in the cells of 100 KB objects, and in the single run of the big stores: its default commits made 108, 81 and 92 commits/s on the disk at the three sizes, with no trend. Write amplification does not depend on the noise.
11. **Commands**, in a release build of the card branch: `zoe-bench record-store commits --repeat 5`, `zoe-bench record-store scattered --records 10000,100000,1000000`, `zoe-bench record-store big --dir <dir> --gigabytes 1,4,16`, and `zoe-bench record-store crash --dir <dir> --kinds <kind> --records 10000,100000`, then `--records 1000000 --repeat 3`. The acceptance test runs `measure` with fewer commits per cell.

## Results

### The commit path of the store

One line per cell and backend kind. The values are the median of 5 repeats, with the range when it is wider than the precision. Write amplification is the bytes passed to `write` ÷ the pickle bytes. The commit latency runs from `tpc_begin` to the return of `finish`.

| Object | Objects per txn | Writers | Backend | Commits | Write amplification | KB written per commit | fsyncs per commit | Commits/s | p50 commit ms | p99 commit ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 KB | 1 | 1 | redb | 100 | 34.43 | 34.4 | 1.00 | 96.6 (95.4–96.8) | 10.3 (10.2–10.5) | 10.8 (10.7–10.9) |
| 1 KB | 1 | 1 | redb-quick-repair | 100 | 46.74 | 46.7 | 2.00 | 48.4 (47.8–48.7) | 20.6 (20.5–20.9) | 21.3 (20.8–21.7) |
| 1 KB | 1 | 1 | log | 100 | 1.13 | 1.1 | 1.00 | 98.8 (98.3–99.1) | 10.1 | 10.4 (10.2–10.4) |
| 1 KB | 1 | 16 | redb | 100 | 36.95 (36.87–37.11) | 37.0 (36.9–37.1) | 1.00 | 96.3 (95.4–96.8) | 10.6 (10.4–154.1) | 907.9 (706.4–1,017.6) |
| 1 KB | 1 | 16 | redb-quick-repair | 100 | 49.30 (49.23–49.38) | 49.3 (49.2–49.4) | 2.00 | 48.3 (48.1–48.4) | 21.0 (20.9–330.5) | 1,572.3 (1,263.9–1,887.7) |
| 1 KB | 1 | 16 | log | 100 | 1.13 | 1.1 | 1.00 | 98.6 (98.4–99.1) | 10.1 (10.1–10.2) | 995.6 (969.5–1,006.0) |
| 1 KB | 1 | 64 | redb | 192 | 39.02 (38.67–39.12) | 39.0 (38.7–39.1) | 1.00 (1.00–1.01) | 96.2 (95.3–96.5) | 10.8 (10.6–298.2) | 1,975.6 (1,948.1–1,993.9) |
| 1 KB | 1 | 64 | redb-quick-repair | 192 | 51.25 (50.92–51.42) | 51.2 (50.9–51.4) | 2.01 | 48.0 (47.9–48.2) | 395.5 (21.1–977.1) | 3,903.7 (3,683.8–3,963.8) |
| 1 KB | 1 | 64 | log | 192 | 1.13 | 1.1 | 1.00 | 98.7 (98.6–98.8) | 10.1 (10.1–10.2) | 1,925.1 (1,913.5–1,934.5) |
| 1 KB | 10 | 1 | redb | 100 | 10.14 | 101.4 | 1.02 | 92.8 (92.2–93.1) | 10.5 (10.4–10.6) | 21.5 (20.8–21.8) |
| 1 KB | 10 | 1 | redb-quick-repair | 100 | 11.37 | 113.7 | 2.03 | 47.1 (47.0–47.4) | 20.9 (20.7–20.9) | 31.4 (31.0–32.1) |
| 1 KB | 10 | 1 | log | 100 | 1.06 | 10.6 | 1.00 | 98.4 (97.8–98.7) | 10.1 (10.1–10.2) | 10.4 (10.3–10.4) |
| 1 KB | 10 | 16 | redb | 100 | 9.12 (9.01–9.24) | 91.2 (90.1–92.4) | 1.02 | 92.5 (92.0–92.9) | 104.2 (10.7–168.1) | 819.8 (649.9–982.5) |
| 1 KB | 10 | 16 | redb-quick-repair | 100 | 10.33 (10.32–10.43) | 103.3 (103.2–104.3) | 2.02 (2.02–2.03) | 47.3 (47.0–47.4) | 243.1 (21.4–332.4) | 1,600.5 (1,285.8–1,925.5) |
| 1 KB | 10 | 16 | log | 100 | 1.06 | 10.6 | 1.00 | 98.7 (98.4–98.8) | 10.1 (10.1–10.4) | 940.6 (773.3–1,003.3) |
| 1 KB | 10 | 64 | redb | 192 | 8.79 (8.72–8.84) | 87.9 (87.2–88.4) | 1.02 | 93.0 (92.8–93.1) | 677.7 (390.6–681.9) | 2,024.2 (1,944.3–2,046.7) |
| 1 KB | 10 | 64 | redb-quick-repair | 192 | 10.01 (9.99–10.09) | 100.1 (99.9–100.9) | 2.02 | 47.5 (47.4–47.7) | 1,331.8 (889.6–1,338.7) | 3,905.9 (3,555.2–3,960.8) |
| 1 KB | 10 | 64 | log | 192 | 1.06 | 10.6 | 1.00 | 98.4 (98.3–98.6) | 233.0 (10.4–507.3) | 1,910.6 (1,860.8–1,931.3) |
| 1 KB | 100 | 1 | redb | 100 | 7.69 | 768.7 | 1.05 | 77.0 (74.5–78.6) | 12.2 (12.0–12.5) | 28.4 (23.3–29.5) |
| 1 KB | 100 | 1 | redb-quick-repair | 100 | 7.81 | 781.0 | 2.05 | 42.4 (42.1–42.6) | 22.7 (22.7–22.9) | 38.3 (34.6–39.9) |
| 1 KB | 100 | 1 | log | 100 | 1.05 | 104.9 | 1.00 | 96.9 (96.5–97.4) | 10.3 (10.2–10.4) | 10.6 (10.5–11.2) |
| 1 KB | 100 | 16 | redb | 100 | 5.92 (5.91–6.00) | 591.6 (591.2–599.6) | 1.03 | 81.6 (78.7–82.6) | 188.4 (186.7–200.8) | 215.5 (209.0–412.2) |
| 1 KB | 100 | 16 | redb-quick-repair | 100 | 6.03 (6.03–6.04) | 603.4 (603.3–604.1) | 2.03 | 44.2 (43.2–44.7) | 354.6 (352.5–363.0) | 382.1 (375.9–406.9) |
| 1 KB | 100 | 16 | log | 100 | 1.05 | 104.9 | 1.00 | 97.2 (96.6–97.8) | 164.5 (163.3–165.3) | 474.8 (432.7–776.5) |
| 1 KB | 100 | 64 | redb | 192 | 5.78 (5.65–5.82) | 578.1 (565.2–581.9) | 1.01 | 81.8 (80.5–82.6) | 759.4 (743.2–781.8) | 812.9 (804.4–1,584.0) |
| 1 KB | 100 | 64 | redb-quick-repair | 192 | 5.95 (5.84–5.95) | 595.1 (584.5–595.3) | 2.01 | 44.4 (44.4–44.6) | 1,410.0 (1,397.6–1,419.8) | 1,476.1 (1,468.1–2,857.7) |
| 1 KB | 100 | 64 | log | 192 | 1.05 | 104.9 | 1.00 | 97.3 (96.8–97.5) | 656.8 (655.5–658.4) | 1,306.1 (1,239.1–1,849.4) |
| 100 KB | 1 | 1 | redb | 100 | 1.59 | 158.5 | 1.04 | 90.0 (88.1–90.7) | 10.6 (10.5–10.9) | 23.0 (21.6–23.7) |
| 100 KB | 1 | 1 | redb-quick-repair | 100 | 1.71 | 170.8 | 2.04 | 46.8 (46.5–47.4) | 20.8 (20.6–21.0) | 33.5 (31.3–34.9) |
| 100 KB | 1 | 1 | log | 100 | 1.00 | 100.1 | 1.00 | 97.4 (96.9–97.9) | 10.2 | 11.1 (10.4–11.5) |
| 100 KB | 1 | 16 | redb | 100 | 1.60 | 159.7 | 1.02 | 92.8 (92.5–93.4) | 147.1 (11.0–166.8) | 826.2 (620.7–981.1) |
| 100 KB | 1 | 16 | redb-quick-repair | 100 | 1.72 | 172.0 | 2.02 | 47.3 (47.3–47.4) | 334.3 (332.3–335.0) | 1,282.0 (820.8–1,398.6) |
| 100 KB | 1 | 16 | log | 100 | 1.00 | 100.1 | 1.00 | 97.9 (97.4–98.0) | 10.3 (10.3–162.8) | 917.4 (623.3–1,015.9) |
| 100 KB | 1 | 64 | redb | 192 | 1.64 | 163.6 | 1.01 | 93.0 (92.8–93.4) | 612.7 (342.1–671.8) | 1,941.9 (1,883.4–1,977.9) |
| 100 KB | 1 | 64 | redb-quick-repair | 192 | 1.76 | 175.9 | 2.01 | 47.3 (47.0–47.5) | 1,291.3 (1,197.7–1,333.5) | 3,794.1 (2,668.4–4,005.1) |
| 100 KB | 1 | 64 | log | 192 | 1.00 | 100.1 | 1.00 | 97.6 (97.5–97.6) | 652.8 (173.1–656.8) | 1,842.3 (1,753.6–1,945.5) |
| 100 KB | 10 | 1 | redb | 100 | 1.34 | 1,337.2 | 1.06 | 71.7 (69.9–72.4) | 13.1 (12.8–13.3) | 27.7 (25.1–31.7) |
| 100 KB | 10 | 1 | redb-quick-repair | 100 | 1.36 | 1,356.5 | 2.06 | 41.2 (40.9–41.7) | 23.3 (23.1–23.6) | 38.9 (37.2–39.5) |
| 100 KB | 10 | 1 | log | 100 | 1.00 | 1,000.6 | 1.00 | 85.8 (84.5–87.7) | 11.5 (11.0–11.6) | 13.0 (12.7–13.2) |
| 100 KB | 10 | 16 | redb | 100 | 1.32 | 1,317.5 (1,317.3–1,317.6) | 1.03 | 72.4 (70.1–75.1) | 203.5 (200.8–215.2) | 270.7 (240.4–447.1) |
| 100 KB | 10 | 16 | redb-quick-repair | 100 | 1.34 | 1,340.7 | 2.03 | 42.9 (41.9–43.1) | 356.4 (353.0–373.3) | 405.1 (400.7–416.1) |
| 100 KB | 10 | 16 | log | 100 | 1.00 | 1,000.6 | 1.00 | 89.9 (85.2–90.6) | 177.0 (176.0–186.1) | 346.1 (178.8–366.1) |
| 100 KB | 10 | 64 | redb | 192 | 1.32 | 1,318.2 (1,318.1–1,318.3) | 1.01 | 79.3 (76.2–79.9) | 797.4 (792.2–837.7) | 894.0 (850.7–1,608.8) |
| 100 KB | 10 | 64 | redb-quick-repair | 192 | 1.36 | 1,363.9 | 2.01 | 43.9 (42.3–44.5) | 1,451.3 (1,426.6–1,515.7) | 1,518.0 (1,485.3–2,548.3) |
| 100 KB | 10 | 64 | log | 192 | 1.00 | 1,000.6 | 1.00 | 90.3 (85.7–91.0) | 706.7 (696.5–727.1) | 781.2 (710.7–1,411.2) |
| 100 KB | 100 | 1 | redb | 100 | 1.31 | 13,147.1 | 1.07 | 29.7 (28.8–30.1) | 32.4 (31.9–34.0) | 55.5 (53.9–56.4) |
| 100 KB | 100 | 1 | redb-quick-repair | 100 | 1.33 | 13,282.3 | 2.07 | 21.4 (21.2–21.6) | 45.4 (45.3–46.3) | 66.5 (63.2–72.8) |
| 100 KB | 100 | 1 | log | 100 | 1.00 | 10,004.9 | 1.00 | 42.6 (41.4–46.8) | 23.3 (21.2–23.6) | 28.3 (25.3–33.7) |
| 100 KB | 100 | 16 | redb | 100 | 1.42 (1.41–1.43) | 14,239.8 (14,142.1–14,331.0) | 1.03 | 29.3 (28.3–30.0) | 549.0 (538.8–568.7) | 670.7 (616.3–685.2) |
| 100 KB | 100 | 16 | redb-quick-repair | 100 | 1.44 (1.43–1.45) | 14,446.9 (14,290.8–14,488.5) | 2.03 | 21.9 (21.7–22.2) | 737.1 (707.8–745.5) | 849.4 (800.3–919.6) |
| 100 KB | 100 | 16 | log | 100 | 1.00 | 10,004.9 | 1.00 | 43.5 (42.3–45.1) | 358.8 (341.7–380.7) | 427.5 (400.7–443.8) |
| 100 KB | 100 | 64 | redb | 104 | 2.06 (2.02–2.09) | 20,648.1 (20,184.7–20,887.0) | 1.02 | 24.6 (24.1–25.0) | 1,903.3 (1,865.6–2,005.8) | 2,852.2 (2,774.3–2,895.9) |
| 100 KB | 100 | 64 | redb-quick-repair | 104 | 2.08 (2.06–2.13) | 20,755.5 (20,564.5–21,323.9) | 2.02 | 19.2 (19.0–20.0) | 2,506.8 (2,466.3–2,579.4) | 3,496.9 (3,392.3–3,586.2) |
| 100 KB | 100 | 64 | log | 104 | 1.00 | 10,004.9 | 1.00 | 40.7 (39.9–41.6) | 1,373.4 (1,358.5–1,441.0) | 1,716.7 (1,667.5–1,745.1) |

- **Bytes.** The log writes the pickle, a 49-byte entry head (kind byte, key, frame header) per record, and one transaction row per commit, so its write amplification is 1.00 to 1.13 in every cell. redb writes whole pages: every 4 KB page that the commit changed in the `records`, `txns` and `meta` tables and in redb's own system tables. For 1 KB objects that is 34 to 39 KB for one object, 88 to 101 KB for 10, and 578 to 769 KB for 100. For 100 KB objects, redb writes 1.31 to 1.64 times the pickles, and 2.06 in one cell (see below). All these bytes are counted at `write` (Method, item 4).
- **Quick repair** adds one fsync and 12 to 17 KB to every commit in the cells of 1 KB objects and of single 100 KB objects. In the cells of 10 and 100 objects of 100 KB, where the file is bigger, it adds 19 to 207 KB.
- **fsyncs.** The log makes exactly one fsync per commit. redb makes one, plus extra syncs when it grows its file: 1.01 to 1.07 per commit. In most cells, the extra syncs equal the calls to `set_len`. Quick repair makes two, plus the same growth syncs.
- **Commit rate.** On this path, the rate is at most one commit per fsync whatever the number of writers, because `finish` holds the commit sequencer through the fsync. With one 1 KB object, redb reaches 96 commits/s, the log 99 and quick repair 48. The CPU time of bigger transactions comes on top: with 100 objects of 1 KB, redb reaches 77 to 82 commits/s and the log 97; with 100 objects of 100 KB (10 MB per commit), redb reaches 25 to 30, quick repair 19 to 22, and the log 41 to 43.
- **Latency.** In many cells with 16 and 64 writers, the 99th percentile is close to the length of the whole run (about 1 s with 16 writers and 2 s with 64), while the median is often a single fsync. The commit sequencer is a plain mutex. The likely cause is that a writer that has just committed often takes it again before the writers that wait; the spike did not show it directly. Group commit needs a queue in arrival order.
- **One cell is out of line.** With 64 writers of 100 objects of 100 KB, redb wrote 2.06 times the pickles, against 1.31 with one writer and 1.42 with 16. At its end, that cell holds about 1.6 GB of pickles (setup and measured commits), more on disk, and more than redb's 1 GB page cache. But the 16-writer cell, with about 1.1 GB of pickles, is also above the cache and writes 1.42, so the cache alone does not explain 2.06. The spike did not find the cause.

### Group-commit estimate

One backend commit for one transaction of each writer: the best case of a group commit.

| Object | Objects per txn | Writers | Backend | Commits | Write amplification | KB written per commit | fsyncs per commit | Commits/s | p50 commit ms | p99 commit ms |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| 1 KB | 1 | 16 | redb | 400 | 8.13 | 8.1 | 0.06 | 1,495.4 (1,487.7–1,496.6) | 10.7 (10.6–10.7) | 11.4 (11.3–11.8) |
| 1 KB | 1 | 16 | redb-quick-repair | 400 | 8.90 | 8.9 | 0.13 | 749.6 (738.0–759.6) | 20.9 (20.7–21.2) | 31.3 (31.0–32.2) |
| 1 KB | 1 | 16 | log | 400 | 1.13 | 1.1 | 0.06 | 1,579.2 (1,577.7–1,585.4) | 10.1 | 10.2 (10.1–10.4) |
| 1 KB | 1 | 64 | redb | 1,600 | 6.86 | 6.9 | 0.02 | 5,191.1 (5,062.7–5,284.5) | 11.5 (11.2–11.7) | 22.9 (22.0–23.7) |
| 1 KB | 1 | 64 | redb-quick-repair | 1,600 | 7.05 | 7.1 | 0.03 | 2,819.1 (2,788.9–2,873.6) | 21.7 (21.4–22.3) | 33.4 (32.2–34.0) |
| 1 KB | 1 | 64 | log | 1,600 | 1.13 | 1.1 | 0.02 | 6,265.3 (6,246.9–6,275.9) | 10.2 | 10.4 (10.3–10.5) |
| 1 KB | 10 | 16 | redb | 400 | 6.51 | 65.1 | 0.07 | 1,144.5 (1,096.1–1,158.9) | 12.2 (12.1–13.0) | 24.7 (24.4–28.9) |
| 1 KB | 10 | 16 | redb-quick-repair | 400 | 6.59 | 65.9 | 0.14 | 659.3 (646.3–665.4) | 22.7 (22.6–23.1) | 34.8 (34.5–35.5) |
| 1 KB | 10 | 16 | log | 400 | 1.06 | 10.6 | 0.06 | 1,565.2 (1,545.7–1,566.7) | 10.2 (10.2–10.4) | 10.4 (10.3–10.6) |
| 1 KB | 10 | 64 | redb | 1,600 | 6.38 | 63.8 | 0.02 | 3,064.9 (2,783.2–3,159.9) | 19.0 (18.8–22.2) | 36.9 (36.4–40.2) |
| 1 KB | 10 | 64 | redb-quick-repair | 1,600 | 6.40 | 64.0 | 0.04 | 1,951.4 (1,931.7–1,987.5) | 30.4 (30.0–31.3) | 51.0 (47.3–52.6) |
| 1 KB | 10 | 64 | log | 1,600 | 1.06 | 10.6 | 0.02 | 6,002.6 (5,890.7–6,028.0) | 10.7 (10.6–10.9) | 11.0 (10.9–11.7) |
| 1 KB | 100 | 16 | redb | 400 | 6.36 | 635.6 | 0.07 | 525.5 (488.5–543.4) | 31.4 (30.5–34.1) | 49.0 (47.5–59.9) |
| 1 KB | 100 | 16 | redb-quick-repair | 400 | 6.37 | 636.7 | 0.14 | 375.8 (348.1–381.3) | 43.0 (40.3–46.7) | 66.8 (61.3–74.8) |
| 1 KB | 100 | 16 | log | 400 | 1.05 | 104.9 | 0.06 | 1,406.9 (1,374.1–1,422.9) | 11.3 (11.2–11.6) | 11.9 (11.7–12.2) |
| 1 KB | 100 | 64 | redb | 1,600 | 6.34 | 634.3 | 0.02 | 718.7 (672.3–795.9) | 88.9 (83.0–94.2) | 162.7 (134.2–181.0) |
| 1 KB | 100 | 64 | redb-quick-repair | 1,600 | 6.35 | 635.0 | 0.03 | 653.2 (618.5–680.0) | 99.7 (92.1–107.0) | 153.1 (141.3–160.4) |
| 1 KB | 100 | 64 | log | 1,600 | 1.05 | 104.9 | 0.02 | 4,128.0 (4,059.8–4,288.7) | 15.4 (15.0–15.9) | 16.7 (15.7–18.0) |
| 100 KB | 1 | 16 | redb | 400 | 1.31 | 130.8 | 0.07 | 1,162.0 (1,152.3–1,170.9) | 11.8 (11.7–12.0) | 26.3 (25.7–27.5) |
| 100 KB | 1 | 16 | redb-quick-repair | 400 | 1.32 | 131.7 | 0.14 | 658.6 (654.3–664.6) | 22.2 (22.2–22.4) | 37.2 (36.3–37.6) |
| 100 KB | 1 | 16 | log | 400 | 1.00 | 100.1 | 0.06 | 1,418.3 (1,363.2–1,456.6) | 11.2 (11.0–11.3) | 12.0 (11.4–13.1) |
| 100 KB | 1 | 64 | redb | 1,600 | 1.29 | 129.4 | 0.02 | 3,334.6 (3,197.3–3,384.3) | 16.4 (16.3–17.1) | 41.3 (38.4–45.4) |
| 100 KB | 1 | 64 | redb-quick-repair | 1,600 | 1.30 | 129.8 | 0.03 | 2,179.7 (2,162.9–2,217.5) | 26.6 (26.1–26.8) | 54.2 (52.6–56.4) |
| 100 KB | 1 | 64 | log | 1,600 | 1.00 | 100.1 | 0.02 | 4,460.7 (4,140.9–4,538.3) | 14.3 (14.1–15.4) | 16.5 (15.5–19.4) |
| 100 KB | 10 | 16 | redb | 400 | 1.29 | 1,290.0 | 0.07 | 470.7 (428.7–476.1) | 28.9 (26.9–31.8) | 122.4 (118.5–135.0) |
| 100 KB | 10 | 16 | redb-quick-repair | 400 | 1.29 | 1,293.5 | 0.14 | 352.0 (345.3–363.4) | 42.7 (40.9–43.8) | 101.1 (94.1–137.0) |
| 100 KB | 10 | 16 | log | 400 | 1.00 | 1,000.6 | 0.06 | 806.5 (785.0–811.6) | 19.8 (19.4–20.4) | 22.8 (22.3–23.9) |
| 100 KB | 10 | 64 | redb | 1,024 | 1.29 | 1,286.0 | 0.02 | 540.7 (518.3–585.1) | 111.2 (87.6–125.2) | 204.9 (171.0–228.0) |
| 100 KB | 10 | 64 | redb-quick-repair | 1,024 | 1.29 | 1,288.4 | 0.04 | 503.4 (478.1–561.8) | 117.9 (105.1–130.4) | 208.7 (198.5–255.0) |
| 100 KB | 10 | 64 | log | 1,024 | 1.00 | 1,000.6 | 0.02 | 1,171.7 (1,155.4–1,339.7) | 57.9 (42.6–59.3) | 66.9 (62.2–73.8) |
| 100 KB | 100 | 16 | redb | 96 | 1.28 | 12,828.0 | 0.09 | 62.9 (59.6–73.2) | 255.7 (223.5–276.9) | 311.8 (269.6–320.5) |
| 100 KB | 100 | 16 | redb-quick-repair | 96 | 1.28 | 12,838.5 | 0.16 | 61.6 (60.1–64.8) | 261.9 (258.2–268.2) | 317.0 (280.6–337.9) |
| 100 KB | 100 | 16 | log | 96 | 1.00 | 10,004.9 | 0.06 | 122.0 (112.6–144.5) | 133.5 (88.8–143.2) | 146.3 (137.7–171.7) |
| 100 KB | 100 | 64 | redb | 64 | 2.56 | 25,627.1 | 0.03 | 54.4 (53.5–55.5) | 1,176.6 (1,154.2–1,196.2) | 1,176.6 (1,154.2–1,196.2) |
| 100 KB | 100 | 64 | redb-quick-repair | 64 | 2.56 | 25,624.7 | 0.05 | 52.9 (44.4–54.5) | 1,210.6 (1,174.7–1,441.1) | 1,210.6 (1,174.7–1,441.1) |
| 100 KB | 100 | 64 | log | 64 | 1.00 | 10,004.9 | 0.02 | 81.9 (70.5–90.1) | 781.1 (710.5–907.8) | 781.1 (710.5–907.8) |

- **Small transactions reach N5.** With 64 writers, every kind passes 1,000 commits/s for one or 10 objects of 1 KB and for one object of 100 KB. With 16 writers, a batch holds at most 16 commits, so the rate cannot exceed 16 commits per fsync time: with one 1 KB object, redb and the log reach about 1,500 commits/s, quick repair 750.
- **Larger transactions do not, with redb.** With 64 writers and 100 objects of 1 KB, redb reaches 719 commits/s and quick repair 653, against 4,128 for the log. With 10 objects of 100 KB: redb 541, quick repair 503, the log 1,172.
- **redb's CPU time per record limits large batches.** With 100 objects of 1 KB per transaction, a batch of 64 transactions took 89 ms, of which 10 ms is the fsync: about 12 µs of CPU per record. The log took 15 ms per batch. redb then reaches 72,000 objects/s, the log 413,000.
- A batch shares the pages of redb: its write amplification falls to 6.3 to 8.1 for 1 KB objects (6.4 to 8.9 with quick repair). It stays above 6, probably because each update inserts a new `(oid, tid)` key next to the older versions of the same oid, and so rewrites the 4 KB leaf that holds them.

### Scattered updates on a filled store

One writer, objects of 1 KB spread over the store, 100 commits, 3 repeats.

| Records in the store | Objects per txn | Backend | Write amplification | KB written per commit | fsyncs per commit | Commits/s |
| --- | --- | --- | --- | --- | --- | --- |
| 10,000 | 1 | redb | 40.87 | 40.9 | 1.00 | 96.6 (95.2–96.9) |
| 10,000 | 1 | redb-quick-repair | 53.19 | 53.2 | 2.00 | 48.5 |
| 10,000 | 1 | log | 1.13 | 1.1 | 1.00 | 99.2 (99.2–99.3) |
| 10,000 | 10 | redb | 13.09 | 130.9 | 1.00 | 93.1 (88.6–95.4) |
| 10,000 | 10 | redb-quick-repair | 14.32 | 143.2 | 2.00 | 47.8 (47.8–48.0) |
| 10,000 | 10 | log | 1.06 | 10.6 | 1.00 | 99.0 (98.7–99.0) |
| 10,000 | 100 | redb | 9.59 | 958.9 | 1.02 | 79.8 (79.4–80.0) |
| 10,000 | 100 | redb-quick-repair | 9.72 | 971.6 | 2.02 | 42.9 (42.7–43.7) |
| 10,000 | 100 | log | 1.05 | 104.9 | 1.00 | 96.9 (96.4–97.3) |
| 100,000 | 1 | redb | 46.31 | 46.3 | 1.00 | 96.1 (96.0–97.5) |
| 100,000 | 1 | redb-quick-repair | 86.62 | 86.6 | 2.00 | 48.2 (48.1–48.5) |
| 100,000 | 1 | log | 1.13 | 1.1 | 1.00 | 99.1 (99.0–99.1) |
| 100,000 | 10 | redb | 17.23 | 172.3 | 1.00 | 94.6 (91.3–94.8) |
| 100,000 | 10 | redb-quick-repair | 21.26 | 212.6 | 2.00 | 47.6 (46.9–47.9) |
| 100,000 | 10 | log | 1.06 | 10.6 | 1.00 | 98.7 (98.6–98.9) |
| 100,000 | 100 | redb | 10.80 | 1,080.3 | 1.00 | 78.7 (76.7–80.2) |
| 100,000 | 100 | redb-quick-repair | 11.21 | 1,120.6 | 2.00 | 43.0 (41.8–43.5) |
| 100,000 | 100 | log | 1.05 | 104.9 | 1.00 | 96.7 (96.5–96.9) |
| 1,000,000 | 1 | redb | 50.31 | 50.3 | 1.00 | 94.8 (76.1–97.2) |
| 1,000,000 | 1 | redb-quick-repair | 314.62 | 314.6 | 2.00 | 47.4 (47.1–47.5) |
| 1,000,000 | 1 | log | 1.13 | 1.1 | 1.00 | 99.0 (98.8–99.2) |
| 1,000,000 | 10 | redb | 18.04 | 180.4 | 1.00 | 93.5 (93.2–94.8) |
| 1,000,000 | 10 | redb-quick-repair | 44.47 | 444.7 | 2.00 | 46.8 (46.6–46.9) |
| 1,000,000 | 10 | log | 1.06 | 10.6 | 1.00 | 98.6 (98.4–98.8) |
| 1,000,000 | 100 | redb | 14.40 | 1,440.4 | 1.00 | 75.8 (67.0–76.6) |
| 1,000,000 | 100 | redb-quick-repair | 17.05 | 1,704.7 | 2.00 | 39.8 (39.4–40.2) |
| 1,000,000 | 100 | log | 1.05 | 104.9 | 1.00 | 97.6 (97.6–97.7) |

- redb writes more as the store grows, because the B-tree gets deeper and the updated objects share fewer pages: 41 KB for a one-object commit in a store of 10,000 records, 50 KB in a store of 1,000,000. Updates of 100 spread objects write 9.6 to 14.4 times their pickles, against 5.8 to 7.7 for 100 neighbouring objects in a small store.
- Quick repair writes much more as the store grows: 53 KB, 87 KB, then 315 KB for a one-object commit. The difference with the default commit is the allocator state, which grows with the file.

### Big stores on the disk

One-object commits of 1 KB on a redb store filled with 1 MB pickles, from one writer, without group commit. One run; the commit rates use the real fsync of the disk, under load.

| Pickles | Store file, MB | Backend | KB written per commit | fsyncs per commit | Commits/s on the disk |
| --- | --- | --- | --- | --- | --- |
| 1 GB | 2,562 | redb | 37.4 | 1.00 | 108.4 |
| 1 GB | 2,562 | redb-quick-repair | 304.6 | 2.00 | 96.8 |
| 4 GB | 8,203 | redb | 39.9 | 1.00 | 80.7 |
| 4 GB | 8,203 | redb-quick-repair | 1,210.9 | 2.02 | 52.5 |
| 16 GB | 34,836 | redb | 44.2 | 1.00 | 92.5 |
| 16 GB | 34,836 | redb-quick-repair | 4,409.8 | 2.02 | 14.0 |

- redb's quick repair writes its whole allocator state in every commit: `try_save_allocator_state` in redb 4.3.0 writes the state of every region. The extra bytes grow with the file: 0.10 to 0.14 KB per MB of file, and 128 bytes per MB on the 34 GB store. On the 34 GB store, the commit rate on the disk fell from 92 to 14 commits/s. On the 2.5 GB store it fell from 108 to 97 commits/s, about 11%.
- The default commit writes 37 to 44 KB per one-object commit, whatever the size.
- The redb files here hold 2.0 to 2.5 times the pickle bytes, because redb rounds each allocation up to a power-of-two number of pages: a 1 MB pickle with its frame takes 2 MB. The store of 1 KB records in the crash test is also about twice its pickles (2,056 MB for 1,000,000 records, against 1,024 MB for the log), but for another reason, likely leaves that splits leave half full.

### Open after a crash

| Backend | Records | Store MB | Crash open, warm ms | Crash open, cold ms | Clean open, warm ms | Clean open, cold ms | Memory growth at open, MB |
| --- | --- | --- | --- | --- | --- | --- | --- |
| redb | 10,000 | 16.1 | 43.9 (41.8–53.8) | 93.9 (82.4–113.8) | 13.6 (11.5–25.4) | 8.6 (3.6–13.1) | 14.6 |
| redb-quick-repair | 10,000 | 16.1 | 6.5 (5.9–7.5) | 11.8 (11.0–12.9) | 8.8 (7.0–10.5) | 4.3 (3.6–5.1) | 0.8 |
| log | 10,000 | 10.3 | 6.0 (5.4–6.3) | 30.2 (28.8–35.9) | 9.4 (8.5–12.3) | 29.0 (28.5–32.0) | 4.5 |
| redb | 100,000 | 257.0 | 143.8 (136.4–165.2) | 579.7 (524.3–700.5) | 9.5 (6.1–10.6) | 14.0 (3.7–22.7) | 123.4 |
| redb-quick-repair | 100,000 | 257.0 | 12.9 (4.5–16.3) | 40.0 (36.7–120.9) | 7.7 (6.5–15.2) | 4.8 (3.9–14.1) | 0.0 |
| log | 100,000 | 102.5 | 62.3 (51.8–89.0) | 255.2 (236.8–284.1) | 51.6 (49.1–96.7) | 248.0 (237.1–282.3) | 12.7 |
| redb | 1,000,000 | 2,056.0 | 1,919.9 (1,848.6–2,033.9) | 6,617.7 (6,593.5–6,799.2) | 29.7 (7.0–32.6) | 25.6 (12.8–43.3) | 556.0 |
| redb-quick-repair | 1,000,000 | 2,056.0 | 14.7 (9.2–35.7) | 30.0 (23.2–81.9) | 6.3 (6.3–37.4) | 5.1 (4.3–47.0) | 1.2 |
| log | 1,000,000 | 1,024.2 | 482.1 (457.8–691.9) | 2,349.2 (2,102.0–2,603.5) | 493.6 (470.8–502.6) | 2,231.9 (2,154.0–2,681.0) | 173.4 |

The records are the count when the writer said it was ready; the store also holds the few transactions that it committed before the kill. Every open found the announced tid. An earlier run, with the three kinds in one process and a lower load, gave crash opens of the default redb store of 20 and 138 ms warm and 62 and 627 ms cold for 10,000 and 100,000 records: the same scale.

- **redb, default commit.** After a crash, redb finds no allocator state of the last commit. Before the open returns, `Database::new` in redb 4.3.0 walks the trees of the last commit and checks their checksums, and falls back to the previous commit slot if that commit is torn. Then it rebuilds the allocator from every page that the trees reach, and writes a new commit. redb counts three full scans. The time grows with the file: 6.6 s cold and 1.9 s warm for 2.0 GB, that is 3.3 s per GB cold and 0.96 s per GB warm. The open also fills redb's page cache (556 MB at 2.0 GB, bounded by the 1 GB cache). After a clean close, the open takes 9 to 30 ms at every measured size.
- **redb, quick repair.** The open loads the allocator state of the last commit: 12 to 40 ms cold and 6 to 15 ms warm, at every measured size.
- **log.** Every open scans the whole log, after a crash and after a clean close alike: 2.3 s cold and 0.48 s warm for 1.0 GB. After a crash, the open also truncates a torn tail with `set_len` and `sync_data`. The index made the memory grow by 173 MB for 1,000,000 records, about 180 bytes per record.

## What the numbers mean for N4 to N7

- **N4** (throughput against ZEO 6 and RelStorage) is a zodbshootout comparison that this spike cannot settle. At these commit rates, redb's bytes do not limit the throughput: 1,000 one-object commits per second at 34 to 50 KB each is 34 to 49 MB/s at `write`, far below what the disk writes. Two costs of redb can matter. First, its CPU time per record in a large commit: the zodbshootout baseline of `bench/README.md` puts 100 objects in every transaction, and in the batched estimate redb reaches about 72,000 objects/s against 413,000 for the log. Second, its bytes on a device with a bandwidth cap, and the wear of the device. The counted bytes are 34 to 50 times the pickle bytes for one-object commits of 1 KB, but at the device the gap is much smaller, because each small append of the log also costs a whole page and a journal commit (Method, item 4). The update scenario of zodbshootout, at concurrency 16 and 64, is the test that should decide whether redb's write path is the bottleneck, as the specification says.
- **N5** (50 writers, 10 ms fsync, 1,000 commits/s) fails with every kind on the walking-skeleton path: one commit per fsync gives about 100 commits/s, 50 with quick repair. N5 needs group commit, whatever the backend. With a best-case group commit, every kind passes for small transactions: 50 writers give at most 50 commits per batch, so the ceiling is about 5,000 commits/s with one fsync per batch and 2,500 with two. For larger transactions, redb's CPU time per record keeps it below 1,000 commits/s (719 for 100 objects of 1 KB, 541 for 10 objects of 100 KB), and the log stays above. The group commit also needs a fair queue, because the current sequencer lets some writers wait for the whole run.
- **N6** (memory bounded, independent of the store size). The log's in-memory index took about 180 bytes per record. For 100 million records of 1 KB, with one version per oid and the spike's simple index, this extrapolates to about 17 GB. A compact index would take less, but any index that keeps every oid in memory grows with the store, which N6 does not allow. redb's memory at open is bounded by its page cache: 556 MB at 2.0 GB, 1 GB at most with the default setting.
- **N7** (ready in 2 s on a 100 GB store, no index rebuild). After a clean stop, both redb settings opened in 4 to 30 ms, measured up to 2.0 GB only. After a crash, a store with the default commit opened at 3.3 s per GB cold and 0.96 s per GB warm, so it fails N7 above about 0.6 GB cold and about 2 GB warm. A linear extrapolation gives at least 5.5 minutes, cold, for 100 GB; the measured growth is faster than linear (see the risks). With quick repair, the crash open took 12 to 40 ms cold at every measured size. The log scans everything at every open, 2.3 s per GB cold, so it fails N7 above about 0.9 GB even after a clean stop, unless it persists its index.

## Quick repair: the recommendation

**Leave quick repair off.** Quick repair makes the open after a crash fast and constant, but every commit then writes the allocator state of the whole file, about 128 bytes per MB, and makes a second fsync. That is cheap on a small store: 12 KB per commit on a 16 MB store, and 0.3 MB on 2.5 GB, where the commit rate on the disk fell only about 11% (108 to 97 commits/s). It is expensive on a big one: 4.3 MB per commit and 14 commits/s on 34 GB, and about 12.5 MB per commit on 100 GB by a linear extrapolation. The big-store run used one writer and no group commit. A group commit writes the allocator state once per batch, so the claim that N4 and N5 cannot afford quick repair on stores of tens of GB is an inference from these runs, not a measurement with group commit. A store with few writers fills few batches and pays the cost on almost every commit.

The default commit costs time only after a crash: the open walks the file, at 3.3 s per GB with a cold page cache on this machine. What this means for N7 and for the no-repair rule is for the human to decide (see the end).

## The log backend

The log writes the fewest bytes and, in the batched estimate, gives the most commits for transactions of many objects. As built here, it does not meet N6 or N7: it scans the whole file at every open, and its index lives in memory. To compete, it would need a persisted index with a scan of the tail only, an index that does not grow in memory with the store, pack (which copies the live records and so has its own write amplification), and its own backup, verify and index tools. In the batched estimate, redb's CPU time per record limits large batches, where the log keeps its rate; zodbshootout decides whether this matters for N4. None of the numbers here shows yet that redb's write path is the bottleneck, so the recommendation is not to build the log now.

## Crash recovery and the no-repair rule

Three texts say that a crash leaves nothing to repair: principle 4 of the specification ("crash recovery without a repair step"), the `Durable` state of the two-phase commit ("No repair step exists because there is nothing half-written to find"), and the zoe-core invariant ("Opening never repairs a store"). Both backends do work at open after a crash:

- **redb with the default commit** checks the checksums of the last commit and falls back to the previous commit slot if that commit is torn, rebuilds the allocator (three full scans in all), and writes a new commit before the open returns. No record changes, and no ZOE code runs.
- **redb with quick repair** loads the allocator state of the last commit, which two-phase commit keeps valid, and skips the walk.
- **The log** truncates a torn tail with `set_len` and `sync_data`. Only an unacknowledged commit can be in that tail.

## Risks and limits

- **Bytes at `write`.** The meter counts the bytes that the backend passes to `write`, not the bytes that reach the device (Method, item 4). The ratios of counted bytes, such as 34 against 1.13, overstate the gap at the device.
- **The emulated device** charges only the fsync. A real device also charges write bandwidth, which counts against redb's larger writes and against the megabytes of quick repair on big stores.
- **The group-commit estimate** is a best case: full batches, no wake-up, no lock wait.
- **Store sizes.** The commit cells run on small stores (up to about 1.6 GB of pickles). The scattered and big-store runs show that redb's cost per commit grows with the store; the commit cells do not include that growth.
- **Reads** (`load`, `loadBefore`) were not measured.
- **Space.** The redb file holds about twice the pickle bytes. The log has no pack, so it grows with every version.
- **Crashes.** The crash images come from SIGKILL, which keeps the page cache. The open times after a power loss, with the page cache lost, were not measured; the cold opens come closest. The crash simulation with `MemVfs` covers the durability of the log and of quick repair, not their open time.
- **Noise.** The machine ran other jobs (Method, item 10). The open times vary by up to two times between repeats, and the big stores ran once.
- **Versions.** The numbers are for redb 4.3.0. The whole allocator state per quick-repair commit is in its current code and can change in a later version.
- **Extrapolations.** The 100 GB figures are linear extrapolations: the open after a crash, the allocator state per quick-repair commit, and the log's index. The repair time was measured from 16 MB to 2.0 GB, and the allocator state from 16 MB to 34 GB. The measured crash open grows faster than linearly: from 257 MB to 2.0 GB, the file grows 8 times, the cold open 11.4 times and the warm open 13.4 times, likely because the file outgrows redb's 1 GB cache. So the 100 GB open time is a lower bound.
- **The log backend** is spike code: the unit tests and the crash simulation cover it, nothing more.

## Decisions for the human

1. **The backend.** Keep redb. Build the log only if the zodbshootout update scenario shows that redb's write path, its CPU time per record first, is the bottleneck.
2. **The quick-repair setting.** Recommended off: the default commit.
3. **N7 after a crash.** With the default commit, the open after a crash grows with the store. The options:
   - accept it, and say in N7 that the 2 s apply after a clean stop;
   - turn quick repair on for small stores only, as a setting (on the 2.5 GB store it cost about 11% of the commit rate on the disk);
   - ask redb upstream for an allocator state saved by region, so that quick repair writes only the regions that a commit changed, which would make it cheap at every size (the spike did not check whether upstream would take it);
   - make an empty commit with quick repair when the store is idle; this helps only a crash while the store is idle, because the last commit before the crash decides whether the open walks the file;
   - in Tier 2, promote a follower instead of waiting for the open of the crashed store.
4. **The no-repair rule.** Decide whether redb's open after a crash (checksum walk, fallback to the previous slot, allocator rebuild, new commit) and the log's tail truncation break principle 4, the `Durable` paragraph and the zoe-core invariant.
