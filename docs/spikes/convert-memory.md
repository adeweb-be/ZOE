# What holds the memory of a large conversion

Question: what holds the memory of `zoe convert --from-data-fs` above the page cache of redb, on a source of several million records, and does it grow with the records? (F14, N6)

Status: the code analysis is done. The measured rows of the large synthetic source and the heap profile still need a run on the reference machine. The section "Measurements still to take" lists them. The verdict below is provisional until then.

## Evidence before the spike

A conversion of a production copy of 6,064,442 records peaked at 3,376,392 KiB of resident memory, about 2.2 GiB above the default page cache of redb (1 GiB). Two smaller synthetic sources peaked near the size of their store. No run took a heap profile, and `/usr/bin/time -v` gives only the peak, so these runs do not tell which pass held the peak.

## Peaks

| source | records | store MiB | page cache MiB | peak RSS MiB |
| --- | --- | --- | --- | --- |
| synthetic, small, on tmpfs (before the spike) | 106,969 | — | 1024 | 28.2 |
| synthetic, medium, on tmpfs (before the spike) | 1,069,733 | 257 | 1024 | 239.8 |
| production copy, on ext4 (before the spike) | 6,064,442 | 5722 | 1024 | 3297.3 |
| synthetic, large, default page cache | to measure | to measure | 1024 | to measure |
| synthetic, large, small page cache | to measure | to measure | 64 | to measure |
| synthetic, large, one large transaction, default page cache | to measure | to measure | 1024 | to measure |

The rows "before the spike" come from the card. The origin gave the store of the production copy as "6.0 GB": the table takes it as 6.0 × 10^9 bytes. The large synthetic source has 7,000,000 records, more than the production copy, and a store larger than 1024 MiB (see "Method").

Machine of the measured rows: to describe on the reference machine (CPU model and cores, memory, filesystem of the source and of the store, kernel, glibc version).

## Method

Code analysis. The spike read the path of `zoe convert --from-data-fs` from the command to redb 4.3: `crates/zoe-server/src/cli/convert.rs`, `crates/zoe-convert/src/datafs.rs` and `filestorage.rs`, the restore transaction, the staging area and the group commit of `zoe-core`, the redb backend and the transaction iterator, and the page cache of redb (`tree_store/page_store/cached_file.rs`, `lru_cache.rs`, `page_manager.rs`, `buddy_allocator.rs`). For each structure it names the pass that holds it and what its size grows with.

Measurement, on the reference machine. Two prototypes stay on the card branch, at commit `d4f6108`, and do not merge:

- `crates/zoe-convert/examples/synthetic_data_fs.rs` writes a synthetic `Data.fs` from a seeded generator, without production data. Arguments: the path, the count of transactions, the records of each transaction, the records of one large transaction in the middle of the file (0 for none), and one blob record in N records (0 for none). The pickles are mostly 200 bytes to 2 KiB, one in 64 is 8 to 64 KiB, one in 4,096 is 128 to 512 KiB; half of each pickle compresses. About one record in four writes an oid of an earlier transaction again, so the store has history. The generator writes no back pointer, so the cache of chain ends of the reader stays empty; its bound is in the table below.
- `crates/zoe-convert/examples/convert_memory.rs` runs `zoe_convert::from_datafs` as the command does, on a store that `Store::open_with_cache_size` opens with a page cache of the given size in MiB. A thread reads `VmRSS` and `VmHWM` from `/proc/self/status` each second and logs them with the size of the store file. While the store file grows, the copy pass runs; after it, the verification pass runs.

The runs:

```text
cargo build --release -p zoe-convert --examples
E=target/release/examples
$E/synthetic_data_fs SRC/Data.fs 140000 50 0 1000            # 7,000,000 records, 7,000 blobs
/usr/bin/time -v $E/convert_memory SRC/Data.fs STORE-A 1024 SRC/Data.fs.blobs 2> a.log
/usr/bin/time -v $E/convert_memory SRC/Data.fs STORE-B 64   SRC/Data.fs.blobs 2> b.log
heaptrack $E/convert_memory SRC/Data.fs STORE-H 1024 SRC/Data.fs.blobs
$E/synthetic_data_fs SRC/Large.fs 140000 50 500000 1000      # the same, with one transaction of 500,000 records
/usr/bin/time -v $E/convert_memory SRC/Large.fs STORE-C 1024 SRC/Large.fs.blobs 2> c.log
```

The peak RSS of each row comes from `/usr/bin/time -v` ("Maximum resident set size"). The heap profiler is heaptrack: its peak heap consumption is the live heap, and the gap between that peak and the peak RSS is memory that the allocator keeps. heaptrack also gives the allocation sites of the peak, so it names the structure and its pass. heaptrack is not in `devenv.nix`; the reference machine needs it. The generator writes about 13 GB of `Data.fs`; the store takes about half of that.

## Where the memory goes

From the code. The measured rows confirm or correct each line.

### Copy pass

| Part | Size | Grows with |
| --- | --- | --- |
| Page cache of redb: read cache and write buffer together | at most the cache size (1 GiB by default); the write buffer at most half of it | the size of the store, up to the cache size, then flat |
| Index of the page cache: a hash-map entry and a queue slot for each cached page, and the header of each `Arc<[u8]>` page | about 60 to 90 bytes for each cached page: about 2 % of the cache with 4 KiB pages, about 20 MiB at 1 GiB | the cache size |
| Reader of the `Data.fs`: a 64 KiB buffer, one record, and at most 65,536 chain ends | about 6 MiB at most, plus the largest record | nothing: fixed |
| Staging area of one restore transaction: the pickles up to `staging-memory`, the others spilled to a file; an index entry for each staged oid | at most 16 MiB of pickles, plus tens of bytes for each record of the transaction | the records of the largest transaction (the index only) |
| Finish of one transaction: one frame and one new zstd compression context at a time; the dirty pages of the redb write transaction (inside the write buffer above) and the set of pages that it allocated | the set: about 16 bytes for each page that the transaction writes | the size of the largest transaction |
| Blobs: an 8 KiB copy buffer, the pending blobs of one transaction | small | the blobs of the largest transaction |
| Allocator state of redb: the buddy bitmaps of the file | a few bits for each 4 KiB page, about 0.1 to 0.2 MiB for each GiB of store | the size of the store, but negligible |
| State of the store: lock table, commit queue, pack notes, sequences | the lock table holds only the oids of the running transaction; no pack runs, so the pack notes are empty | nothing |

The trace of the medium source matches the first line: its store (257 MiB) is smaller than the cache, the cache keeps every page that the copy writes (a flushed page goes into the read cache), and the RSS follows the store file (240 MiB of RSS at 257 MiB of store).

### Resume

The copy pass skips each transaction up to the last tid of the store. It reads only the header of such a transaction and moves to the next one, so it holds no record. The last tid comes from an atomic counter of the store. The resume adds nothing to the copy pass.

### Verification pass

| Part | Size | Grows with |
| --- | --- | --- |
| Page cache of redb | as in the copy pass: it is full at the end of a copy of a large source | the cache size |
| Reader of the `Data.fs` | as in the copy pass | nothing |
| Transaction of the file: `verify` collects every `DataRecord` of the transaction, each with its whole pickle | the pickle bytes of the transaction | the size of the largest transaction |
| Transaction of the store: one `TxnInfo` with every record, each pickle decompressed (`Backend::txn_records`), with one new zstd decompression context for each record; the iterator keeps a slice of at most 1,024 tids | the pickle bytes of the transaction again, plus 8 KiB | the size of the largest transaction |
| `compare_txn`: two `BTreeMap<Oid, Option<&[u8]>>` that borrow the pickles | about 50 bytes for each record, twice | the records of the largest transaction |
| Blob comparison: two 64 KiB buffers for a sampled blob | 128 KiB | nothing |
| Problems: at most 100 lines and a count | small | nothing |

So the verification pass holds about twice the pickle bytes of the largest transaction above the page cache. The copy pass keeps the pickles of a transaction below `staging-memory`; the verification pass has no such bound.

### The allocator

The command uses the system allocator, glibc malloc, on one thread: no thread of `zoe-core` takes part in a conversion. The cache pages are `Arc<[u8]>` blocks of 4 KiB times a power of two. They live long and leave the cache in a near-LRU order. Between them come short-lived blocks of many sizes: a zstd context for each record, the frames, the decompressed pickles, and the records of a transaction in the verification. glibc serves a block of 128 KiB or more from `mmap` only until it frees such a block; then it raises its threshold to the size of that block (up to 32 MiB), and later blocks of that size come from the heap. A free block between two live cache pages goes back to the system only when it is at the top of the heap. So once the cache is full and pages leave it, the RSS can stay above the live heap. This gap does not show in a run whose store is smaller than the cache, because no page leaves the cache: the medium source peaked below its store.

### What can explain the 2.2 GiB of the production copy

The code names two parts that the synthetic sources before the spike did not exercise, and no part that grows with the count of records or of transactions:

1. A large transaction. A Plone site can hold one, for example the creation of the site or a rebuild of the catalog. The verification pass holds it twice: a transaction of about 1.1 GiB of pickles gives about 2.2 GiB.
2. Fragmentation of the heap of glibc after the cache is full, as described above.

The production run cannot tell them apart: it took no heap profile and no trace. The large synthetic source tells them apart. Run A against run B separates the cache from the rest. heaptrack on run A gives the gap between the live heap and the RSS: a large gap is fragmentation. Run C against run A gives the cost of one large transaction, expected at about twice its pickle bytes in the verification pass.

### Measurements still to take

On the reference machine, with the prototypes above:

- runs A and B: the peak RSS with the default page cache and with 64 MiB, and the `/proc` trace of each pass;
- heaptrack on run A: the peak heap, the peak RSS, and the allocation sites of the peak;
- run C: the peak with one transaction of 500,000 records;
- from the production copy, on the site and out of the repository: the pickle bytes and the records of its largest transaction. This tells whether part 1 explains its peak.

Expected from the code, without fragmentation: run A at about 1024 + 30 MiB, run B at about 64 + 10 MiB, and run C at run A plus about twice the pickle bytes of the large transaction (about 0.9 GiB of pickles, so about 1.8 GiB more).

## Verdict

Provisional, from the code: no structure of `zoe convert --from-data-fs` grows with the count of records or of transactions. The memory of the copy pass and of the resume is bounded by the page cache plus a fixed amount, plus an index of tens of bytes for each record of the largest transaction. The verification pass is not bounded by the configuration: it holds the pickles of the largest transaction twice. That breaks N6 for a source with a large transaction.

If the measured rows confirm it, one build card bounds it: the verification of a conversion compares a transaction without holding its pickles, for example with a hash of each record of the file and a read of the store record by record, so that it holds a fixed amount and a few bytes for each record of the transaction. If heaptrack shows a gap between the live heap and the RSS after the cache is full, a second build card bounds the fragmentation, for example by reusing one zstd context in each pass instead of a new one for each record. The planner writes these cards from the measured finding. This verdict becomes final when the measured rows are in the table.
