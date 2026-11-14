# 0060. The native text index: the text deltas, ZOE format 4, tantivy and ZoeTextIndex

Status: accepted. Card ZOE-284, a spike (finding: `docs/spikes/text-index.md`; prototype: branch `spike/ZOE-284`). This record gives the design of the native text index of F18 and of its delta record type. It records the decisions Q8, Q9 and Q15 of the plan of phase 6 (#597) and the decision N1 (#601). It changes the on-disk format and pack, two red zones. It adds the protocol extension `text`, a public Python API (`ZoeTextIndex` and the text methods of the storages), two subcommands of `zoe`, and the version 3 of a dump (an amendment of ADR 0030). Section 17 gives its names, its errors and its settings.

## Context

A ZCTextIndex keeps word ids, not text, in its trees: the lexicon, `_wordinfo`, `_docweight`, `_docwords`, and two `Length` counters. A save of one document rewrites a bucket of `_wordinfo` for each distinct word of each of its three texts. These buckets are the first cause of the saves that fail (`docs/spikes/posting-dicts.md`, ADR 0042). The finding measures one save on `catalog-1m`: ZCTextIndex writes 458 records and 390 KB of pickles. Every two saves of different documents write 74 records in common on average.

F18 asks for a text index with BM25, stemmers, folding, stop words, phrases, prefixes, fuzzy matching and boosts. It asks for a query under 20 ms at p99 (G4), and for a compatibility switch that gives the results of ZCTextIndex, identical on the Plone suites (G1). The specification makes the deltas the source of truth and the tantivy directory a cache: "Durability: the deltas are records in the ZOE transaction, so the tantivy directory beside the store is a cache that `zoe catalog rebuild-text` recreates without Plone."

The human decided:

- Q8: the text deltas are a new record type in the store, with a history per tid. This is a format change, with a new format name, its migration tool, and the changes to pack, convert, verify, scrub and the bit-flip harness, in build cards of their own before the card that writes the deltas.
- Q9: a snapshot outside the mirror window starts from the current index, drops the documents changed after the snapshot, and matches them again against their text at the snapshot, read from the delta history. The matched set is exact at any snapshot, and the scores use the statistics of the current corpus. In the transaction that indexes, the client sends the staged texts with the query. The server indexes them in a small overlay in memory that replaces the old versions of those documents.
- Q15: `zoe catalog rebuild-text` runs on a store folder that no process holds, and the naming table lists it.
- N1: a `DemoStorage` over a ZOE storage keeps the deltas that a transaction stages in an overlay in its memory, as `zoe.kv` keeps the namespace writes (ADR 0047, section 9). `ZoeTextIndex` answers there from a text index in the process.

### Terms

This record uses the terms of ADR 0059: mirror window, transaction overlay, test storage. It adds these:

- **Text delta**: the text of one document in one text index of one catalog, at one tid. It is a row of `text_deltas`, not a record: it has no oid.
- **Unindex**: a text delta with an empty payload. The document leaves the index.
- **Clear delta**: a row of `text_deltas` that empties one text index of one catalog at one tid. "The deltas" names the text deltas and the clear deltas together.
- **Dead delta**: at a snapshot, a text delta before the newest clear delta of its index before that snapshot. No read at that snapshot or at a later snapshot sees it.
- **Live text**: the text of a document at a snapshot: the newest text delta of its key before the snapshot, when that delta is not dead and not an unindex.
- **Replaced document**: a document of the tantivy index that one query takes out of the result of the index.
- **Text overlay**: the texts that one query scores beside the tantivy index, in place of the replaced documents. It is not the overlay of `zoe.kv`, and not the transaction overlay.

## Decision

### Decisions of the human

The human delegated the decisions of this record, and its acceptance, to the orchestrator. The seven points of section 17 stay as the human decided them (#601). The orchestrator decided:

1. A clear of an index is a clear delta in `text_deltas`, not a generation in the key (section 1). `ZoeTextIndex.clear()` and `Catalog.delIndex` stage it (section 14).
2. Pack keeps the transaction row of each delta that it keeps. Verify accepts a missing previous delta at or before `pack_watermark` (sections 4 and 7).
3. The vote reserves the keys of the text deltas of its transaction, and checks each text delta against the deltas committed after the snapshot of its transaction. An equal text is dropped, another text is a conflict. The finish never fails for a text delta (sections 5 and 8).
4. The call `text_query` of the extension `text` answers a text clause without the mirror, with the `resultset` of the other indexes as its filter (section 12). `ZoeTextIndex` needs the backend `catalog` (section 14).
5. The text clause refuses a snapshot at or before `pack_watermark`. Pack caps its tid at the tid of each live text index (sections 4 and 13).
6. A change of Q9: in the compatibility mode, the statistics are those at the snapshot, so the scores are those of ZCTextIndex at every snapshot that the text clause serves. Q9 let the scores use the statistics of the current corpus. The reason: the mechanism of a snapshot runs at nearly every query under writes, so with those statistics the scores of the compatibility mode would differ from ZCTextIndex inside the mirror window too. The enhanced mode keeps the statistics of the current index, as Q9 decided (section 13).
7. `k1` and `b` are fixed at 1.2 and 0.75, the constants of tantivy 0.27 (section 11).
8. ZOE-297 adds a replay of the text queries of the Plone suites on a ZOE store (section 11).
9. A change of N1: the base of a test storage answers its queries, with the deltas of the test storage as staged texts (section 9). N1 said that a text index in the process, built from the deltas of the overlay and of the base, answers. The reason: one mechanism, the text overlay of section 13, then serves the transaction and the test storage, and a base behind a server gives its statistics with its reply. The test storage holds no text index of its own.

### 1. The text deltas

Two key spaces hold the deltas. The numbers of the keys are big-endian. The frame header is little-endian, as for every frame.

| Key space | Key | Value |
| --- | --- | --- |
| `text_deltas`, a text delta | the oid of the catalog (8 bytes); the length of the index id (1 byte, 1 to 255); the index id (UTF-8); the rid (4 bytes, the sign bit flipped, so the byte order is the order of the rids); the tid (8 bytes) | a frame with checksum tag 9. Its previous serial is the tid of the previous text delta of the same key without its tid, or 0. Its payload is the length of the language (1 byte), the language (0 to 35 bytes of ASCII: a BCP 47 tag, or empty), then the text (UTF-8). An unindex has an empty payload and codec 0. The codec of another text delta follows the setting `compression`, as for a record. |
| `text_deltas`, a clear delta | the oid of the catalog (8 bytes); the byte 0; the length of the index id (1 byte); the index id; the tid (8 bytes) | a frame with checksum tag 9 and an empty payload, codec 0. Its previous serial is the tid of the previous clear delta of the same index, or 0. |
| `text_txn` | the tid (8 bytes), then the key of the delta without its tid | empty, not a frame, so no checksum tag |

- The catalog is the `Products.ZCatalog.Catalog.Catalog` object, the object that the mirror registers (ADR 0059). The rid is the rid of ZCatalog, a 32-bit signed int.
- No index id has the length 0. So the byte 0 tells a clear delta from a text delta, and the clear deltas of a catalog sort before its text deltas. A scan of the text deltas of one key never meets a clear delta.
- A read of a live text at a snapshot takes the newest clear delta of the index before the snapshot. Then it makes one reverse range scan of the key from the snapshot, as `loadBefore` does. The store keeps the clear deltas of each index in memory: they are few. A text delta before that clear is dead. A text delta at the tid of the clear is live: its transaction staged it after the clear (section 8).
- A scan of `text_txn` over a tid range gives the documents and the clears of the range.
- The tag 9 is the next free tag (`format::KeySpace`). The tag 6 stays reserved for `vectors`.
- A delta takes no per-OID lock. The vote reserves the key of each text delta and checks the text delta against the deltas committed after the snapshot of its transaction (section 8).
- A delta counts in `max-record-size`, `staging-memory` and `max-transaction-size` as a pickle does. A delta over a limit fails with `TextTooLarge`, as `KvTooLarge` fails for a namespace write, and stages nothing.
- An empty index id or one longer than 255 bytes, a language longer than 35 bytes or not ASCII, and a rid outside the signed 32-bit range fail with `InvalidText`. The call stages nothing.
- A transaction with only deltas commits as a transaction with only namespace writes: a tid, and a transaction row with an oid count of 0.

The finding gives the bytes: 3,105 bytes for the three deltas of one save of `catalog-1m`, with the keys of both key spaces. The records of ZCTextIndex take about 420 KB for the same save.

### 2. ZOE format 4 and its migration tool

`ZOE format 4` names the tables with `text_deltas` and `text_txn`, and the log with the op of section 3. An open refuses a store of format 3 with `Format`, and the message names the tool. The tool is a subcommand of `zoe`: `zoe migrate STORE`. It runs on a store folder that no process holds. It opens the store with the rules of format 3 and folds the log. Then it writes the row `format` and creates the two tables in one backend write with a sync. A crash before that write leaves a store of format 3. A crash after it leaves a store of format 4. On a store of format 4, the tool changes nothing and exits with 0. It refuses any other format with `Format` and exits with 1. The naming table lists it. `prepare` of the B5 fixtures and of `catalog-1m` runs it (ZOE-293), so no fixture is built again.

### 3. The write-ahead log and the unfolded table

- An entry of the log gets the op kind 9, a delta: the key length as a `u16`, the key of `text_deltas`, the length as a `u32`, then the frame, byte for byte. The ops of the deltas of a commit come after its namespace writes.
- The tid of a delta is the tid of the transaction row before it. A key with another tid fails the open with `Format`.
- The fold writes the row of `text_deltas` and the key of `text_txn`, which it derives from the key.
- A direct batch writes both key spaces in its one backend write.
- The estimate of a batch (ADR 0056, section 10) counts each delta: the length of its key, the length of its payload, and 64 bytes for its op.
- The unfolded table holds the deltas of the backlog by key. A read of a live text reads the table first, then the tables, as a load does. A scan of `text_txn` reads the deltas of the backlog in the table and merges them with the scan of redb.

### 4. Pack

Pack at T applies these rules to the deltas:

1. Pack keeps every delta after T.
2. A text delta at or before T that is dead at T goes: it is before the newest clear delta of its index at or before T. That clear delta goes too, with every clear delta of the index before it, after the dead text deltas.
3. Of the other text deltas at or before T of one key, pack keeps the newest and removes the others.
4. When that newest text delta is an unindex, pack removes it too.
5. With `pack-gc` true, the deltas at or before T of a catalog that the mark does not reach go too. The mark is unchanged, since a delta refers to nothing.

With `pack-gc` false, pack has no mark, and rules 1 to 4 apply. Pack removes the key of each removed delta in `text_txn`. It removes the deltas in batches of its own, before the batches of the records. The batches remove the text deltas in key order, and the clear deltas in the last batch of the deltas. So a clear delta goes only after every dead text delta that it hides. A stop or a crash between two batches always leaves the clear delta, and no dead text delta comes back. Before each batch, it marks what the noted records of the finishes refer to, as for a batch of records. It notes, in a set in memory, the tids of the deltas that it keeps. A transaction row before T stays while it keeps a record or a delta. So a transaction of deltas only, and an edit whose records pack removed, keep their rows while a delta of theirs stays. A delta whose frame fails its checksum stops the pack, as a record does. A dry run counts the deltas that pack would remove.

Pack caps T at the tid of each text index that the process holds open: the tid up to which a live index holds the deltas, or the tid from which a rebuild reads them. So pack never removes an unindex that a live index has not applied yet. A text index in `text/` of a closed store needs no cap: its open rebuilds it when its tid is before the pack watermark (section 10).

Pack never changes the live text of a document at T or later. The text clause refuses a snapshot at or before `pack_watermark` (section 13): there, the deltas that pack removed would give wrong texts.

### 5. Undo

- The undo of T reads, for each text delta of T, the text of its key as its own transaction sees it: a text that the transaction staged, else the newest committed one.
- When that text is the text of the delta of T, the undo stages the live text of the key before T, or an unindex when there is none. In this comparison, no live text equals an unindex.
- Otherwise the undo fails with `Undo` (`UndoError` in Python): text has no merge.
- When a clear delta of the index committed after T, the undo of a text delta of T fails with `Undo`, also for an unindex. So an undo never puts a document back into a cleared index.
- So several undos in one transaction undo one over the other, as for records: the undo of a later transaction first, then the undo of an earlier one.
- An undo of a transaction that holds a clear delta fails with `Undo`.
- The `before` of each text delta that an undo stages is the tid before which the undo read the key. So check 3 of section 8 applies to it: a commit of the key after that read fails the vote with `TextConflict`. The reservation of section 8 covers the keys of the undo, and the finish checks no key of it.
- The catalog records of the same undo go through the tiers of the vote.

### 6. The iterator, `zoe convert` and the dumps

- The ZODB iterator gives records only. A transaction with deltas gives its records, and a transaction with only deltas gives no record, as a transaction with only namespace writes.
- A copy through the iterator leaves the deltas out, with no error: zodbconvert, `copyTransactionsFrom`, and the stream of a follower of deployment tier 2. The follower is not built; its card carries the deltas. The catalog of such a copy needs each `ZoeTextIndex` reindexed, as the queue of queued indexing needs a rebuild (ADR 0047).
- `zoe convert` to a `Data.fs` drops the deltas, and its report counts them: FileStorage has no place for them. A site that leaves ZOE first puts ZCTextIndex back (section 14). `zoe convert` from a `Data.fs` writes no delta. The site installs `ZoeTextIndex` on the converted store, which indexes again.
- A dump gets the version `0x03` (an amendment of ADR 0030). Three entries follow the records of a transaction:

| Entry | Tag | Fields after the tag |
| --- | --- | --- |
| text delta | `X` (`0x58`) | the oid of the catalog, 8 bytes; the index id, the language and the text, as byte strings of UTF-8; the rid, 4 bytes |
| unindex | `Y` (`0x59`) | the oid of the catalog, 8 bytes; the index id, as a byte string; the rid, 4 bytes |
| clear delta | `C` (`0x43`) | the oid of the catalog, 8 bytes; the index id, as a byte string |

- The rules of these entries: they come after the records of their transaction, in the key order of `text_deltas`, so the clear deltas of a catalog come before its text deltas. The rid is a signed 32-bit big-endian int, not the form of the key. The transaction entry counts its records only, and the next `T` or `E` ends its deltas. A transaction with only deltas has a `T` entry with no record.
- A build reads only the version that it writes (ADR 0030). So `zoe restore` reads only the dumps of version 3. `zoe backup` refuses a backup directory that holds a dump of version 2, and asks for a new full dump in a new directory. No store in production needs the version 2.
- `zoe restore` stages the deltas with `Txn::restore_text` and `Txn::restore_clear` in the restore transaction, in the order of the dump.
- `zoe verify --backup` compares the deltas as it compares the records. Each delta of the dumps must be the same in the store, but a delta that a pack removed at or before the pack watermark is no difference. The live text of each key at the last tid of the dumps must be the same.
- The tantivy directory is not in a dump. The open after a restore builds it (section 10).

### 7. `zoe verify`, `zoe scrub` and the bit-flip harness

- `zoe verify` reads every row of `text_deltas`. It checks the frame and that the key parses. It checks that the previous serial is the tid of the previous delta of the key, or of the index for a clear delta. A missing previous delta at or before `pack_watermark` passes: pack removed it. It checks that a transaction row exists at the tid. It checks that each delta has its key in `text_txn`, and each key of `text_txn` its delta. A finding names the catalog, the index, the rid and the tid, never the text.
- A read of a delta whose frame fails is `CorruptText`, with the catalog, the index, the rid and the tid: a distinct error, as `CorruptRecord`.
- `zoe scrub` is not built. Its card reads `text_deltas` as it reads `records`.
- The bit-flip harness flips bytes of the rows of `text_deltas` as it does for records. It checks that every read through the core gives `CorruptText`.

### 8. The write path

The core (ZOE-293):

- `Txn::text_set(catalog: Oid, index: &str, rid: i32, language: &str, text: &str, before: Tid)` stages a text delta. `Txn::text_unindex(catalog: Oid, index: &str, rid: i32, before: Tid)` stages an unindex. `before` is the snapshot of the transaction: the tid that its Connection loads before, as the tid of `catalog_query` (ADR 0059, section 8).
- `Txn::text_clear(catalog: Oid, index: &str)` drops the text deltas of that index that the transaction staged before it, and stages a clear delta. A text delta staged after it commits at the same tid and stays live.
- They stage between the begin and the vote, from the thread that drives the transaction, with the rules of `Txn::kv_set`: a later write of a key in the transaction replaces the staged one, and an abort drops it.
- A restore transaction refuses them. It takes `Txn::restore_text(catalog, index, rid, language, text: Option<&str>)` and `Txn::restore_clear(catalog, index)`, which restore a delta at the tid of the transaction and check nothing.
- `Store::text_before(catalog, index, rid, tid)` gives the language and the live text before `tid`, or none. `tid` is exclusive, as the tid of `loadBefore`. `Store::text_changed(after, last)` scans `text_txn`: it gives the key of each delta of the tids after `after` and at or before `last`, in tid order, then in key order.

The vote reserves the keys of the text deltas of its transaction:

- The store keeps a set of the reserved keys under a mutex. The mutex is held with no other lock taken under it.
- The vote reserves every key of its staged text deltas before it runs the checks below. A key that another transaction reserved fails the vote at once with `TextConflict` (`ConflictError` in Python, so Zope retries the request).
- The finish releases the reservation after the commit is visible to reads, or when the commit fails. An abort releases it.
- So no commit of a reserved key is in flight. The check of the vote sees every commit of the key that can come before its own, and no two members of one commit batch write one key.
- A reservation is not a per-OID lock, and the lock hand-off of ADR 0044 does not pass it. A vote that takes a hand-off from a commit in flight and stages a key of that commit fails at once with `TextConflict`. It does not wait for the commit of the holder.
- A vote refuses a transaction whose `before` is at or before `pack_watermark` with `TextConflict`, when the transaction stages a text delta. Pack removed the history that the checks read, and a retry gets a new snapshot. Pack needs no other cap for it.

The checks of a text delta, with the snapshot `before` of its transaction:

1. When the transaction also stages a clear delta of the index, the delta is written. No check runs.
2. When the language and the text equal the live text at the snapshot, the delta is dropped. An unindex of a document with no live text is dropped too. ZCTextIndex writes nothing in both cases.
3. When a text delta of the key, or a clear delta of its index, committed at or after `before`: the newest committed text delta of the key, with the same language and text, drops the staged delta. Anything else fails the transaction with `TextConflict` (`ConflictError` in Python, with the oid of the catalog). This is the conflict that "Step 3" of the specification names: the same rid and index in two concurrent transactions.
4. Otherwise the delta is written.

- The vote runs checks 2 and 3, with reads outside the commit sequencer. The finish runs no check, and it never fails for a text delta: `tpc_finish` must not fail.
- The finish reads the previous serial under the commit sequencer: the newest text delta of the key, from the unfolded table, then the tables.
- A clear delta takes no check. It replaces the whole index, as `ZCTextIndex.clear()` replaces the trees of its index. A text delta committed between the snapshot of the clearing transaction and its clear is dead. An entry written to the old trees of a ZCTextIndex is lost in the same way.

The wire (ZOE-295): the protocol extension `text`, with the calls `text_set(catalog, index, rid, language, text, before)`, `text_unindex(catalog, index, rid, before)` and `text_clear(catalog, index)`. They have no reply, and they stage into the open transaction in its lane. An error fails the vote, as for `kv_set`. The calls `text_query` and `text_info` of section 12 run on the read pool.

Python (ZOE-295):

- `zoe.Storage.text_set(catalog, index, rid, language, text, before, transaction)`, `text_unindex(catalog, index, rid, before, transaction)` and `text_clear(catalog, index, transaction)`.
- `zoe.Storage.text_query(catalog, tid, clause, filter=None)`, `text_info(catalog, index, tid, rid=None)` and `text_supported()`.
- The same methods on `zoe.zeo.ClientStorage` in its native mode.
- `catalog`, `before` and `tid` are 8 bytes, `index`, `language` and `text` are `str`, `rid` is an `int`.
- `InvalidText` raises `ValueError`, `TextTooLarge` `StorageError`, `TextConflict` `ConflictError`, `CorruptText` `StorageSystemError`, and `Undo` `UndoError`.
- `ZoeTextIndex` keeps the deltas of a transaction in a data manager until the commit, one for each key, with savepoints, as `zoe.kv` keeps its writes. It calls these methods in the commit, with the snapshot of its Connection as `before`.

A storage without the write path: a stock ZEO client, the asyncio mode of `zoe.zeo.ClientStorage`, FileStorage. The profile step refuses it with `Unsupported`. `ZoeTextIndex.index_object` on such a storage raises `Unsupported`, so the transaction fails, and no instance writes content whose text no delta holds. A search on such a storage raises `Unsupported` too.

### 9. A test storage (N1)

- On a test storage, the deltas of a commit go to memory that belongs to the `DemoStorage`, as the namespace writes of `zoe.kv` do (ADR 0047, section 9). They never reach the base, and they go with the `DemoStorage`.
- The `DemoStorage` keeps the newest text delta of each key and the clear deltas of each index. It checks nothing: a test runs one transaction at a time. A stacked `DemoStorage` keeps its own deltas over those of the one below, and a delta above replaces the same key below.
- A query on a test storage goes to its base, a ZOE storage with the extension `text`, in the process or behind a server, at the last tid of the base. The client sends the texts of each `DemoStorage` of the stack, then the staged texts of the transaction, as the staged texts of the clause (section 12).
- On a test storage, the clause carries the settings of the index (`mode`, `language` and the boosts): the base cannot read the namespace `zoe.text` of the overlay of `zoe.kv`.
- The clause lists the index in `cl` when the base holds no record of the catalog, when the base has no text index for the catalog, and when a `DemoStorage` holds a clear delta of the index. The base has no text index for the catalog when its registration key or its settings are only in the overlay of `zoe.kv`. The base then answers from the staged texts alone, with the settings of the clause. A site that a test layer creates is such a catalog.
- So the base gives the statistics and the scores, in the process or in the reply of the server. The test storage holds no text index. With a base in the process, the text index of the base is in the process.
- The Plone suites run on an empty `zoe.Storage` base. There each query runs on the text overlay alone, not on tantivy. So the replay of section 11 runs on a ZOE store.
- A test storage whose texts pass the cap of a call (section 12) fails its searches with `Unavailable`.

### 10. The index and its directory

- One tantivy index serves each registered catalog (ADR 0059, section 1) whose settings in `zoe.text` name a text index. It is in `text/<oid>/` of the store directory, `<oid>` as 16 lowercase hexadecimal digits. The store holds the directory: only the process that holds the lock of the store opens a writer. The payload of each tantivy commit holds the tid up to which the index holds the deltas.
- The index follows the commits after their publication, in tid order, in a thread of its own, as the mirror does (ADR 0059, section 5).
- The index is a cache. At the open, these start a rebuild from the deltas in the background: a missing directory; one that tantivy cannot open; a file whose checksum fails `Index::validate_checksum`, which the open runs in the background; a tid before the pack watermark or after the last tid of the store; and a change of the settings `mode` or `language` (section 14). Otherwise the open reads the deltas after the tid of the index through `text_txn` and applies them.
- A clear delta that the index meets starts a rebuild of the index of its catalog from the live texts.
- The writer commits the deltas behind the tid in batches: every 500 ms, or every 10,000 deltas. These are fixed values, not settings. `zoe_text_commit_lag_seconds` is the age of the oldest committed delta that the index does not hold yet, and 0 when it holds all.
- A query that needs the index while its open or a rebuild runs waits for it, at most `catalog-wait` (ADR 0059, default 1 s). Then it fails with `Unavailable`. A query on a catalog with no index fails with `Unavailable` at once. ZCTextIndex is not there to take the query. `Unavailable` is `zoe.storage.Unavailable`, a `StorageError`.
- A delta that fails its checksum stops the index at that delta. The store logs it at ERROR, with the catalog, the index, the rid and the tid. Each text query of the catalog then fails with `CorruptText`, and a rebuild meets the same delta. The index never serves a guessed text.
- A read-only open never writes `text/`. It uses an index that the open would not rebuild, and section 13 serves the commits after its tid. Otherwise its text queries fail with `Unavailable`.
- tantivy does not check its files on read. A file damaged after the open gives wrong results until a rebuild. The index is a cache: N13 covers the deltas, the source of truth, and `zoe catalog rebuild-text` rebuilds the cache.
- A copy of a closed store copies `text/` with the store, and the copy opens it. A copy of an open store can copy a torn directory: the open of the copy rebuilds it. A backup holds no `text/`. A fork (phase 7) answers from the index of its base at the fork tid, with the texts of the fork as staged texts, as section 9 does.
- The directory of a catalog that is no longer registered stays, and the open ignores it.
- The reader uses the manual reload policy: it starts no thread. The writer has one indexing thread and a memory budget of 256 MB, and starts 6 threads. A rebuild in the store uses that writer. Its time and its peak with one thread were not measured. The build of the finding took 196 to 243 s and peaked at 3.0 to 3.3 GB with 4 threads.
- The index keeps in memory the rid of each document, the document of each rid, and in the compatibility mode the number of words of each document of each index: 12 MB, and 4 MB for each index, for 1,000,000 documents (section 12).
- N6 counts the text index of each catalog as a term of its bound: the budget of the writer and the maps in memory. The page cache holds the mapped files, as for the tables.
- The extension starts the threads at the open of a store that holds a registered catalog, never at import.

### 11. The two modes

- The enhanced mode is the default of `ZoeTextIndex`. It has the analyzer of the site language: a Snowball stemmer, ASCII folding, and the stop words of the language. It has the BM25 of tantivy, phrases, prefixes, fuzzy matching (off by default), and the boosts `Title^3`, `Description^1.5`, `SearchableText^1` for a query on `SearchableText`. One tantivy field serves each text index. With plone.app.multilingual, one field serves each language of the site, chosen from the language of the delta.
- `k1` is 1.2 and `b` is 0.75: the constants of tantivy 0.27. A tunable value would need a weight of its own, with its own bounds for block-max WAND. F18 names no need for one.
- The score of the enhanced mode is an `f32`. The text clause gives it as an integer: the score times 1,024, rounded to the nearest integer, as `OkapiIndex` scales its own. Both paths order by that integer, then by the rid descending, so the native path and the Python path give one order. The collector keeps the first hits by that integer. Its pruning threshold is the lowest `f32` that rounds to the integer of the last hit that it keeps.
- Both modes index, beside the words of each field, a field of the pairs of words in a row. A phrase of two words reads the posting list of one pair. A longer phrase intersects its pairs, then checks the positions of the documents that hold them all. On the texts of `catalog-1m`, whose 2,000 words are all frequent, a phrase of two words read through the positions alone is the slowest query by far (finding, section 6).
- The compatibility mode has the words of the lexicon `plone_lexicon` of Plone 6.1, the Okapi score of `OkapiIndex` in its integer form, and the order of `Catalog.search`. tantivy gives the term dictionary, the postings with their frequencies and positions, and the deletes. ZOE keeps the number of words of each document in a fast field, and computes the score itself: the norms of tantivy round the length, and its BM25 has another `idf`.
- The finding shows the results identical to ZCTextIndex on every query of the comparison and on the 111 queries on `catalog-1m`. It needs the transliteration table of the Unidecode release that Plone 6.1 pins: the splitter of Plone transliterates with Unidecode the words that start with a Latin letter, and the tables of the Rust crates differ from it. ZOE generates its table from that release, Unidecode 1.4.0. It changes the table only with a release of Plone that changes the pin.
- In the compatibility mode, the scores are those of ZCTextIndex at every snapshot that the text clause serves (section 13).
- ZOE-297 adds a replay for G1. The text queries that the probe of ZOE-292 records in the Plone suites run in the compatibility mode on a ZOE store that holds the documents of the suites. They run against ZCTextIndex on the same documents, with commits during the replay. So G1 covers the executor on tantivy, the mechanism of a snapshot and the wire, which the suites on a test storage do not reach (section 9).
- The risks for G1: Python's `re` and `unicodedata` follow the Unicode version of the interpreter (15.0 on 3.12, 15.1 on 3.13, 16.0 on 3.14), and the tables of the Rust crates follow one version. A character assigned between them can split in another way. `numObjects` and `indexSize` in a transaction do not count the staged texts (section 14). The documents of the suites, in natural language, were not compared yet.

### 12. The query translator, the text clause and `text_query`

`ZoeTextIndex` parses a query with the `QueryParser` and the lexicon pipeline of ZCTextIndex, in Python. So the syntax (`AND`, `OR`, `NOT`, parentheses, quoted phrases, `*` and `?`) and the errors stay those of ZCTextIndex. It sends a plan of the parse tree, and the server runs it in the mode of the index. A glob that starts with `*` or `?` raises `QueryError` in the client, as ZCTextIndex raises it.

The text clause is a clause of a plan of `catalog_query` (ADR 0059, section 8). The reply of `catalog_query` gets the scores and an error form:

```text
clause  = ... | {"k": "t", "i": index, "q": node, "lg": [language...] | nil,
                 "cl": [index...], "st": [staged...], "set": settings | nil}
settings = {"mode": "enhanced" | "compat", "language": [language...], "boosts": {index: float}}
node    = {"n": "and", "items": [node...], "nots": [node...]}
        | {"n": "or", "items": [node...]}
        | {"n": "atom", "w": [word...]}
        | {"n": "phrase", "w": [word...]}
        | {"n": "glob", "p": pattern}
staged  = [index, rid, language, text | nil]
reply   = {"tid": tid, "r": rids, "sc": scores | nil, "n": len, "a": actual_result_count,
           "rows": [[cell...] | nil...] | nil, "paths": [path...] | nil}
        | {"tid": tid, "f": reason, "why": text}
        | {"tid": tid, "e": error, "why": text}
scores  = bin: the score of each rid of "r", as a signed 32-bit big-endian int
error   = "unavailable" | "packed" | "cleared" | "corrupt" | "too_large"
```

- `w` holds the words after the lexicon: each element of the pipeline of the lexicon of the index, with `process_post_glob` when it has one. The compatibility mode uses them as they are. The enhanced mode analyses each word again with its analyzer.
- `p` holds the pattern before the lexicon, as the user wrote it. The server folds it with the analysis of the mode.
- In the enhanced mode, a term matches a glob when it matches the pattern with the literal parts folded. A pattern of one literal, alone or with a final `*`, also matches its analysed literal as an exact term, never as a prefix. So `running*`, the glob of the search form of Plone, finds the stem `run` and the terms that start with `running`, and not `runway`: ZCTextIndex finds only the words that start with `running`. A glob scores 1 in each field that it matches, times the weight of the field, and its exact stem adds its BM25 score. So a document of the stem ranks above a document that only matches the prefix, and the hits of the stem keep their BM25 order.
- `st` holds the staged texts of the indexes that the query reads: the index of the clause, and with the boosts `Title` and `Description`. It has one entry for each key. A `nil` text is an unindex.
- `cl` lists the indexes that the transaction cleared, and on a test storage the indexes of section 9. The server answers for them from the staged texts alone.
- `set` holds the settings of the index on a test storage (section 9), and `nil` elsewhere: the server then reads `zoe.text`.
- `lg` lists the fields that the query reads, from the `Language` clause of the catalog query. The client sends it only when the catalog has an index `Language`, and `nil` otherwise: ZCatalog ignores a key that has no index. It maps each language of the clause to a field of the index as a delta goes to its field: the same tag, else the same primary subtag, else the first language of the settings, so the neutral language `""` maps to the first field. `nil` reads every field of the index. `lg` only narrows the fields: the index `Language` still filters the rows, so the hit set is the hit set of the Python path.
- `sc` comes when the plan has a text clause and no sort. An error comes from the text clause (sections 10 and 13). `unavailable` and `too_large` raise `zoe.storage.Unavailable`, `packed` and `cleared` raise `ReadConflictError`, and `corrupt` raises `StorageSystemError`.

The mirror runs the other clauses of the plan first and gives the bitmap of their rids with the text clause. The result follows `Catalog.search`:

- With `sort_on` unset, the result is the count of all the hits in the filter, and the first `b_start + b_size` hits by score, with their scores. The order is the score descending, then the rid descending, as `IIBucket.byValue(0)`. Each other clause whose result is a set adds 1 to the score, as `weightedIntersection` adds it, so `data_record_score_` stays that of ZCatalog. With no `b_size`, the result is every hit in that order: `Catalog.search` uses `sort_limit` only for a sort.
- With `sort_on` set, the result is the hits in the filter as a set, with no score: `Catalog.search` does not use the scores for a sort. The mirror orders them with its sort array, as for any other clause. For a large result it walks the rows in the order of the sort and stops at the limit. For a small one it takes the best rows of the result.

`catalog_query` refuses a query whole with a reason of ADR 0059, section 7: `stale`, `not_ready`, `not_registered`, `index`, `too_large`, `lag`, `outside_window`, and the other reasons. Then `Catalog.search` runs in Python and calls `ZoeTextIndex.query_index` with its `resultset`. The index sends `text_query`, which answers without the mirror:

```text
text_query(catalog, tid, clause, filter)
clause  = the text clause above, without "k"
filter  = nil | rids
reply   = {"tid": tid, "r": rids, "sc": scores} | {"tid": tid, "e": error, "why": text}
text_info(catalog, index, tid, rid)
reply   = {"tid": tid, "docs": count, "words": count, "lang": language | nil, "text": text | nil}
        | {"tid": tid, "e": error, "why": text}
```

- `filter` holds the rids of the `resultset`, sorted, or `nil` when `Catalog.search` gives none.
- `r` holds every hit in the filter, in rid order, and `sc` the integer score of each. The client makes an `IIBucket` of them. Then `Catalog.search` intersects, sorts and slices as for ZCTextIndex.
- `rids` and `tid` have the forms of ADR 0059, section 8. `tid` is exclusive.
- `text_info` gives the documents of the index at the snapshot, the distinct words of the index at its tid, and the live text of `rid` at the snapshot when `rid` is not `nil`.
- A call holds at most 50,000 msgpack values and 16 MiB (ADR 0059, section 8). A filter that passes the cap is not sent: the client intersects the reply with the `resultset` itself. Staged texts that pass the cap fail the search with `Unavailable`. With the boosts, a transaction reaches the cap at about 3,000 saved documents.
- A reply holds at most 16 MiB too. A hit of `text_query` takes 8 bytes, its rid and its score, so the cap holds about 2,000,000 hits. A larger result fails the search with the error `too_large`, which raises `Unavailable`.
- A clause that does not have the form of this section raises `ValueError`.

The text index keeps the maps of section 10 in memory. It builds them at the open and changes them with each reload of its reader. The filter applies inside the search, and the smaller side drives it:

- A filter that holds fewer than one document of the index in 32 drives: its documents seek the postings. In the enhanced mode they are a document set with a score of 0 that leads the intersection of tantivy.
- A query whose bound of hits is less than a quarter of the filter drives: the text clause looks up the rid of each hit in the bitmap of the filter. The bound comes from the document frequencies of the words: a word gives its frequency, a phrase the least of its words, `AND` the least of its items, `OR` the sum. A glob in the enhanced mode and fuzzy matching have no bound.
- Otherwise the filter becomes a bitset of the documents of each segment. In the enhanced mode, the collector checks it in the callback of the pruning of tantivy (block-max WAND). So the pruning keeps working, and its threshold rises with the hits of the filter only. A second pass on the same weight counts the hits in the filter with no score. A query with no bound has one pass that scores every hit. In the compatibility mode, each hit gets its score, the bitset decides with no branch whether the hit stays, and a heap keeps the first hits.

The finding (section 8) measures this design on `catalog-1m`. On the 12 terms of the plan, the query set of G4, every variant stays under 20 ms at p99. With the filter of a listing, a selective filter, a sort, or both, the worst is 6.7 ms in the enhanced mode, 6.1 ms in the compatibility mode, 9.7 ms with the boosts, and 14.1 ms with fuzzy matching. Alone, the compatibility mode takes 13.2 ms. A check of each hit after the scores missed the target: 34 to 72 ms at p99 in the first round, with another spike on the machine, and 19.0 to 26.4 ms with a filter in the round of section 8. The rids, the order, the counts and the integer scores of the compatibility mode are those of the check after the scores on every query.

### 13. Consistency: the two mechanisms of Q9

Both mechanisms replace some documents of the index, for one query, by other versions of their texts. A replaced document leaves the result of the index, and the text overlay answers for its other version. The overlay is direct: it splits the texts of the queried index and scores the words of the query in each document. It builds no tantivy segment, whose build costs 1.1 ms with no document.

- The snapshot. When the index holds commits at or after the tid of the clause, the documents that changed in them, from a scan of `text_txn`, get their live text at the snapshot from `text_deltas`. When the index lacks commits before the snapshot, the documents that changed in them get their live text at the snapshot too. A document deleted after the snapshot is one of them. So the mechanism also gives back a document that the index already dropped. The matched set is exact at every snapshot that the clause serves.
- The transaction. The client sends the texts that its transaction staged, in `st`. They replace the documents of their keys, after the mechanism of the snapshot. An index in `cl` replaces all its documents: the answer comes from the staged texts alone.
- The statistics. In the compatibility mode, N, the total length and the document frequency of each word of the query are those of the index, corrected by the replaced documents: less their versions in the index, plus their versions in the overlay. So the scores are those of ZCTextIndex at the snapshot, with the staged texts, at every snapshot that the clause serves. In the enhanced mode, the scores use the statistics of the current index, as Q9 decided.
- The refusals. The clause refuses a snapshot at or before `pack_watermark`, with the error `packed`: pack removed the deltas that would give its texts. It refuses a snapshot before a clear delta that the index holds, with the error `cleared`: the index no longer holds the documents before the clear. Both raise `ReadConflictError` in Python, so Zope retries the request at a new snapshot. A clear delta before the snapshot that the index does not hold yet means a rebuild (section 10): the query waits for it.
- The cost grows with the replaced documents: about 0.06 ms for each document, with its reads (finding, section 5). In a transaction that indexes, 1 to 10 staged documents add 1.5 to 3.2 ms. At a snapshot behind the index, the replaced documents are those of the commit lag of the index and of the age of the snapshot. Only pack bounds that age: 1,000 changed documents cost about 88 ms, and 10,000 about 0.6 s. No timeout stops the call: it holds its thread of the read pool to its end. The correction of the statistics was not measured.

The finding gives the cost of each mechanism as a function of the documents.

### 14. `ZoeTextIndex`

- A ZCatalog index class of `zoe.plone`, meta type `ZoeTextIndex`. Its settings: `indexed_attrs`, `mode` (`enhanced` or `compat`), `language` (with plone.app.multilingual, the languages of the fields), and the boosts. The settings of each index of a catalog are in the durable namespace `zoe.text` of the store, under the oid of the catalog, so `zoe catalog rebuild-text` reads them without Plone.
- A change of `mode` or `language` starts a rebuild of the index of the catalog. A change of `indexed_attrs` changes the texts that the index stages: the site reindexes the index, as for ZCTextIndex. A change of the boosts applies to the next query.
- `ZoeTextIndex` needs the backend `catalog` of `zoe.plone` in each instance. The registration of the catalog (ADR 0059, section 1) gives the text index, and the hook of `Catalog.delIndex` stages the clear. In an instance without the backend, or on a storage without the extension `text`, each method of `ZoeTextIndex` that writes or searches raises `Unsupported`. `uninstall()` of the backend `catalog` refuses while a catalog of the process holds a `ZoeTextIndex`.
- `index_object` stages the text of each indexed attribute, joined as ZCTextIndex joins them. An attribute that gives no text stages nothing, so the entry stays, as in ZCTextIndex. `unindex_object` stages an unindex.
- `clear()` stages a clear delta. `Catalog.clear()` calls it, from `manage_catalogClear`, `clearFindAndRebuild` and `refreshCatalog`, and `manage_clearIndex` calls it too. The hook of `Catalog.delIndex` calls it for a `ZoeTextIndex`, so an index added again with the same id starts empty. After `Catalog.clear()` ZCatalog gives new random rids, and a rid that comes again finds only dead deltas.
- `query_index` and `_apply_index` send the text clause: in a plan of `catalog_query` through the hook of ADR 0059, else with `text_query` (section 12).
- `numObjects`, `indexSize` and `getEntryForObject` read the server with `text_info`. `getEntryForObject` splits the live text with the lexicon of the index. In a transaction, `numObjects` and `indexSize` do not count the staged texts; `getEntryForObject` reads the text that the transaction staged for the rid first.
- The profile step `zoe.plone:text` refuses with `Unsupported` an instance without the backend `catalog` and a storage without the extension `text`. It replaces the three ZCTextIndex indexes of `portal_catalog`, `SearchableText`, `Title` and `Description`, with `ZoeTextIndex` indexes of the same ids. Then it indexes the catalog in batches, which writes the deltas.
- The way back is the step `zoe.plone:text-uninstall`. It stages a clear delta for each `ZoeTextIndex`, puts the ZCTextIndex indexes back and reindexes them in Python.

### 15. `zoe catalog rebuild-text`

- `zoe catalog rebuild-text STORE [--catalog OID]` runs on a store folder that no process holds: it takes the lock of the store and refuses a held one.
- It reads the live text of each key of each catalog at the last tid, and the settings of `zoe.text`. It writes a new tantivy directory `text/<oid>.new` with 4 indexing threads: about 4 minutes and a peak of about 3.3 GB for `catalog-1m` (finding, section 6).
- Then it renames `text/<oid>` to `text/<oid>.old`, renames `text/<oid>.new` to `text/<oid>`, syncs `text/`, and removes `text/<oid>.old`. Two renames are necessary: rename(2) does not replace a directory that holds files.
- An open removes a `.new` or `.old` directory that a crash left. With no `text/<oid>`, the open rebuilds the index (section 10).
- It writes nothing in the tables. It prints the documents and the time of each catalog.
- The naming table lists the subcommand `catalog` with `rebuild-text`, and `migrate`.

### 16. The cards

The split of the plan stands:

- ZOE-293: the key spaces, the format name, the migration tool, the op of the log, the staging, the checks and the commit of the deltas in the core (`Txn::text_set`, `text_unindex`, `text_clear`, `Store::text_before`, `text_changed`), verify and the bit-flip harness.
- ZOE-294: pack and its cap, undo, the dumps and `zoe convert`.
- ZOE-295: the wire calls and the Python methods of the storages, the index of each catalog, its directory, the lag metric, the text clause, `text_query` and `text_info`.
- ZOE-296: `ZoeTextIndex`, the translator, the deltas of a test storage, the hook of `Catalog.delIndex` and the profile steps. The settings namespace `zoe.text` goes with it.
- ZOE-297: the compatibility mode, its transliteration table and the replay of section 11.
- ZOE-298: `zoe catalog rebuild-text`, with its read of `zoe.text`.

### 17. Names, errors and settings

1. The migration tool is `zoe migrate` (section 2).
2. The protocol extension is `text`, with the calls `text_set` and `text_unindex` (section 8).
3. On an instance without the write path, `index_object` raises `Unsupported` (section 8).
4. A query during a rebuild waits up to its timeout, then fails with `Unavailable` (section 10).
5. The transliteration table of the compatibility mode is generated from Unidecode 1.4.0, the pin of Plone 6.1, and changes only with a release of Plone that changes the pin (section 11).
6. The settings of `ZoeTextIndex` are in the durable namespace `zoe.text` (section 14).
7. tantivy is behind the cargo feature `text` of `zoe-catalog`, on in `zoe serve` and in the wheel. It adds 9.4 MB to a stripped release binary. A build without the feature has no text index.

The other names of this record: the calls `text_clear`, `text_query` and `text_info` of the extension `text`; the errors `InvalidText`, `TextTooLarge`, `TextConflict` and `CorruptText` of the core; `zoe.storage.Unavailable` in Python. The timeout of item 4 is `catalog-wait` (ADR 0059). This record adds no setting: the commit interval of the writer (500 ms or 10,000 deltas) and its budget (256 MB) are fixed.

## Consequences

- The build cards change the specification, the reference and the crate files. This record does not. The changes:
  - "Data model": the rows `text_deltas` and `text_txn`; in the row `txns`, pack keeps a row that keeps a delta; in `meta`, `ZOE format 4`; the paragraph of `wal/`, with the op 9 and the format 4 that names the tables and the log.
  - "Record framing": the tag 9 of `text_deltas` and the error `CorruptText`.
  - The naming table: the subcommands `migrate` and `catalog` (with `rebuild-text`) in the row of the CLI; the row of the on-disk format, which still says `ZOE format 2`, gets `ZOE format 4`; the row of the protocol extensions gets `text`.
  - N19: a store opens after its migration tool, in place of "a phase 1 store opens unchanged".
  - N13 and its gate: the bit-flip harness covers the deltas; the files of tantivy are a cache outside N13.
  - N6 and "Memory and startup": the text index of each catalog is a term of the bound.
  - "Pack", "Undo" and "The write-ahead log (ADR 0056)": the rules of sections 3 to 5.
  - "`zoe backup` and `zoe restore`: dumps": the version 3 and its entries; "a restore reads only the dumps of version 2" becomes version 3; the refusal of a directory of version 2; the deltas in `zoe verify --backup`.
  - "Migration from `Data.fs`" and the copies: `zoe convert` drops the deltas; a copy through the iterator leaves them out.
  - "Native BM25 text index": `k1` and `b` fixed in place of "tunable"; the bullet "Consistency", which falls back to a Python index, gets the mechanisms and the refusals of section 13; the scores of the compatibility mode at every snapshot that the text clause serves (#597, item 7).
  - "Extension negotiation", the method table of "Server mode" and `docs/reference/protocol.md`: `text` and its calls.
  - The table of the Python surface and `docs/reference/python-api.md`: the text methods of the storages and `zoe.storage.Unavailable`.
  - The text of the backends of `zoe.plone`: `ZoeTextIndex` needs the backend `catalog`, and the profile steps.
  - "Testing of the catalog": the replay of section 11 for G1, and the query set of `just bench catalog text` for G4.
  - `crates/zoe-core/CLAUDE.md`: the format name, the tag 9, the deltas in a commit batch and in a direct batch, the reservations and the checks of section 8, the mutex of the reservations in the lock order, and the invariant of pack on the rows of the transactions.
  - ADR 0059, section 8 (an amendment): the text clause in a plan of `catalog_query`, and the field `sc` and the error form `e` in its reply.
  - `crates/zoe-catalog/CLAUDE.md`: the invariants "a query ... takes the Python path, whole" and "a snapshot older than the mirror window takes the Python path" get: on that path, a `ZoeTextIndex` asks the text index with `text_query`.
  - `CONTEXT.md` gets the terms text delta, clear delta and text overlay with this record.
- A save of a document writes three deltas in place of the buckets of three ZCTextIndex indexes. The text indexes leave the conflicts of saves. Two concurrent transactions that write one document with different texts conflict on its delta, also when they share no record.
- A text delta never fails a finish. A vote that stages a key of a transaction in flight fails at once with `TextConflict`, also after a lock hand-off, and Zope retries the request.
- A store of format 3 needs the migration tool. The fixtures move with `prepare`. A backup directory of version 2 needs a new full dump.
- The tantivy directory is a cache. Losing it costs a rebuild: about 4 minutes for `catalog-1m` with `zoe catalog rebuild-text`. A rebuild inside a store uses one thread, and its time was not measured. During a rebuild, text searches wait `catalog-wait`, then fail with `Unavailable`. A clear starts such a rebuild: `clearFindAndRebuild`, `manage_catalogClear`, `manage_clearIndex`, `delIndex` and the uninstall step.
- A site on FileStorage or on a stock ZEO client cannot use `ZoeTextIndex`. Each instance of a site with `ZoeTextIndex` needs the backend `catalog`.
- A copy through the iterator leaves the deltas out. Its catalog needs each `ZoeTextIndex` reindexed.
- Snippets, the `@search` extension of plone.restapi and the labelled query set stay out of phase 6.
- On the 12 terms of the plan, G4 holds in every variant, with a sort or with `b_size`. Over the 111 queries of the finding, the compatibility mode misses it: 27.6 ms at p99 alone and 23.1 ms with the filter, from the phrases of two frequent words (up to 30 ms). Fuzzy matching takes 20.7 ms at p99 alone over the 111 queries. It is off by default.
- A text query with no `sort_on` and no `b_size` gets every hit in the order of the score. For a word in 77 % of the entries with the filter of a listing, about 380,000 hits, that takes about 25 ms in the compatibility mode and 35 ms in the enhanced mode (finding, section 8). The search view of Plone sends `b_size`.
- `just bench catalog text` (ZOE-299) runs the 12 text terms of the plan of `catalog-1m` in the five variants of the finding, in both modes, through the wire. It proves G4. The finding did not measure an index of many segments with deletes, the change of the maps at each reload of the reader, the wire, the parse in Python, concurrent queries, the text overlay with the filter, the correction of the statistics, the check of the files at the open, or the path of `text_query`, which scores every hit with no pruning.
- A filter that becomes a bitset costs 1.2 ms for 495,943 rids. A query whose hits have no bound pays it even when its hits are few: a glob of the search form of Plone in the enhanced mode loses up to 1.9 ms at p50.
- The order of the documents of the index does not follow the rids, and does not need to. That order would save 0.3 ms of the 1.2 ms of a broad filter, and updates and merges would break it.
- tantivy does not check its files on read. A file damaged after the open gives wrong results until a rebuild.

## Alternatives

- Text in the slots of the catalog records, or in a durable namespace: no history per tid, so no exact snapshot. Q8 chose a record type.
- The text index answers outside the mirror window from ZCTextIndex: `ZoeTextIndex` replaces ZCTextIndex, so no Python index is left to answer. Q9 chose the delta history, and `text_query` answers when `catalog_query` refuses.
- A generation in the key of each text delta, raised by a clear: each key grows. A read at a snapshot still needs the generation at that snapshot, so a history per tid of the generation, which is what a clear delta is.
- An unindex for each rid at a clear: one row for each document of the index, in place of one row.
- A repeat of check 3 at the finish, under the commit sequencer, in place of the reservations: `tpc_finish` must not fail, and the lock hand-off of ADR 0044 makes the race between the vote and the finish common.
- A per-OID lock on each key of a text delta: the lock manager locks oids, and a key of a delta has none. The reservations are a set of their own.
- The scorer of tantivy for the compatibility mode: its `idf`, its rounded norms and its `f32` scores give other scores and other ties.
- `k1` and `b` as settings: a weight of their own, with bounds of their own for block-max WAND, for no need that F18 names.
- A Rust port of the parser of ZCTextIndex: the parser in Python is the reference, and the plan keeps it.
