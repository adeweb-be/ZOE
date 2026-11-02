# 0060. The native text index: the text deltas, ZOE format 4, tantivy and ZoeTextIndex

Status: proposed. Card ZOE-284; the finding is `docs/spikes/text-index.md`. This record gives the design of the native text index of F18 and of its delta record type. It records the decisions Q8, Q9 and Q15 of the plan of phase 6 and the decision N1. It changes the on-disk format and pack, two red zones. It adds a protocol extension, a public Python API (`ZoeTextIndex`, the write path of the deltas) and two subcommands of `zoe`. The section "Open points" lists what the human decides before ZOE-293 starts.

## Context

A ZCTextIndex keeps word ids, not text, in its trees: the lexicon, `_wordinfo`, `_docweight`, `_docwords`, and two `Length` counters. A save of one document rewrites a bucket of `_wordinfo` for each distinct word of each of its three texts, and these buckets are the first cause of the saves that fail (`docs/spikes/posting-dicts.md`, ADR 0042). The finding measures one save on `catalog-1m`: ZCTextIndex writes 458 records and 390 KB of pickles, and every two saves of different documents write 74 records in common on average.

F18 asks for a text index with BM25, stemmers, folding, stop words, phrases, prefixes, fuzzy matching and boosts; a query under 20 ms at p99 (G4); and a compatibility switch that gives the results of ZCTextIndex, identical on the Plone suites (G1). The specification makes the deltas the source of truth and the tantivy directory a cache: "Durability: the deltas are records in the ZOE transaction, so the tantivy directory beside the store is a cache that `zoe catalog rebuild-text` recreates without Plone."

The human decided:

- Q8: the text deltas are a new record type in the store, with a history per tid: a format change, with a new format name, its migration tool, and the changes to pack, convert, verify, scrub and the bit-flip harness, in build cards of their own before the card that writes the deltas.
- Q9: a snapshot outside the window starts from the current index, drops the documents changed after the snapshot, and matches them again against their text at the snapshot, read from the delta history; the matched set is exact at any snapshot, and the scores use the statistics of the current corpus. In the transaction that indexes, the client sends the staged texts with the query, and the server indexes them in a small overlay in memory that replaces the old versions of those documents.
- Q15: `zoe catalog rebuild-text` runs on a store folder that no process holds, and the naming table lists it.
- N1: a `DemoStorage` over a ZOE storage keeps the deltas that a transaction stages in an overlay in its memory, as `zoe.kv` keeps the namespace writes (ADR 0047, section 9), and `ZoeTextIndex` answers there from a text index in the process.

## Decision

### 1. The text deltas

A text delta is the text of one document in one text index of one catalog, at one tid. Two key spaces hold them.

| Key space | Key | Value |
| --- | --- | --- |
| `text_deltas` | the oid of the catalog (8 bytes); the length of the index id (1 byte); the index id (1 to 255 bytes of UTF-8); the rid (4 bytes, the sign bit flipped, so that the byte order is the order of the rids); the tid (8 bytes). Numbers big-endian. | a frame with checksum tag 9. Its previous serial is the tid of the previous delta of the same key without its tid, or 0. Its payload is the length of the language (1 byte), the language (0 to 35 bytes of ASCII, a BCP 47 tag or empty), then the text (UTF-8). An empty payload is an unindex. The codec follows the setting `compression`, as for a record. |
| `text_txn` | the tid (8 bytes), then the key of the delta without its tid | empty, not a frame, so no checksum tag |

- The catalog is the `Products.ZCatalog.Catalog.Catalog` object, the object that the mirror registers (ADR 0059). The rid is the rid of ZCatalog, a 32-bit signed int.
- A read of the text of a document at a snapshot is one reverse range scan from the key at the snapshot, as `loadBefore`. A scan of `text_txn` over a tid range gives the documents that changed in it.
- The tag 9 is the next free tag (`format::KeySpace`); 6 stays reserved for `vectors`.
- A delta takes no lock, and the vote checks nothing for it: of two transactions that write one key, the later tid wins, as for a namespace write (ADR 0043). Two saves of one document already conflict on the content object and on the records of the other indexes.
- A delta counts in `max-record-size`, `staging-memory` and `max-transaction-size` as a pickle does.
- A transaction with only deltas commits as a transaction with only namespace writes: a tid, and a transaction row with an oid count of 0.

The finding gives the bytes: 3,105 bytes for the three deltas of one save of `catalog-1m`, with the keys of both key spaces, against about 420 KB for the records of ZCTextIndex.

### 2. ZOE format 4 and its migration tool

`ZOE format 4` names the tables with `text_deltas` and `text_txn`, and the log with the op of section 3. An open refuses a store of format 3 with `Format`, and the message names the tool. The tool is a subcommand of `zoe`: `zoe migrate STORE` (**open point 1**: its name). It runs on a store folder that no process holds. It opens the store with the rules of format 3, folds the log, then writes the row `format` and creates the two tables in one backend write with a sync. A crash before that write leaves a store of format 3; after it, a store of format 4. It refuses any other format. The naming table lists it, and `prepare` of the B5 fixtures and of `catalog-1m` runs it (ZOE-293), so no fixture is built again. The specification says that a store opens after its migration tool, in place of N19 "a phase 1 store opens unchanged".

### 3. The write-ahead log and the unfolded table

An entry of the log gets op kind 9, a text delta: the key length as a `u16`, the key of `text_deltas`, the length as a `u32`, then the frame, byte for byte. Its tid is the tid of the transaction row before it. The fold writes the row of `text_deltas` and the key of `text_txn`, which it derives from the key. The unfolded table holds the deltas of the backlog by key. A read of a text reads the table first, then the tables, as a load does: one lookup. A scan of `text_txn` reads the deltas of the backlog in the table and merges them with the scan of redb.

### 4. Pack

Pack at T keeps every delta after T. Of the deltas at or before T of one key, it keeps the newest, except an unindex, and removes the others with their keys in `text_txn`. It removes an unindex at or before T, when it is the newest of its key, with every delta before it. With `pack-gc` true, the deltas at or before T of a catalog that the mark does not reach go too; the mark is unchanged, since a delta refers to nothing. With `pack-gc` false, there is no mark, and only the first two rules apply. The deltas go in the batches of the pack, in key order, after the records of the batch. A delta whose frame fails its checksum stops the pack, as a record does.

The text index does not depend on pack: pack never changes the text of a document at or after T. A reader at a snapshot before T is already refused by the pack watermark.

### 5. Undo

The undo of T stages, for each delta of T, the delta of its key before T, or an unindex when there is none. When a transaction after T wrote the same key, the undo fails with `UndoError`: text has no merge. The catalog records of the same undo already go through the tiers of the vote.

### 6. The iterator, `zoe convert` and the dumps

- The ZODB iterator gives records only. A transaction with deltas gives its records, and a transaction with only deltas gives no record, as a transaction with only namespace writes.
- `zoe convert` to a `Data.fs` drops the deltas, and its report counts them: FileStorage has no place for them. A site that leaves ZOE first puts ZCTextIndex back (section 14). `zoe convert` from a `Data.fs` writes no delta; the site installs `ZoeTextIndex` on the converted store, which indexes again.
- A dump gets version `0x03` (an amendment of ADR 0030). After the records of a transaction come its deltas, in key order: the entry `X` (`0x58`): the oid of the catalog, 8 bytes; the index id, the language and the text, as byte strings; the rid, 4 bytes; and the entry `Y` (`0x59`), an unindex: the catalog, the index id and the rid. The transaction entry counts its records only. `zoe restore` stages them with `Txn::restore_text` in the restore transaction. The tantivy directory is not in a dump: the open after a restore builds it (section 10).

### 7. `zoe verify`, `zoe scrub` and the bit-flip harness

- `zoe verify` reads every row of `text_deltas`. It checks the frame, that the key parses, that the previous serial is the tid of the previous delta of the key, that a transaction row exists at the tid, and that each delta has its key in `text_txn` and each key of `text_txn` its delta. A finding names the catalog, the index, the rid and the tid, never the text.
- A read of a delta whose frame fails is `CorruptText` with the catalog, the index, the rid and the tid: a distinct error, as `CorruptRecord`.
- `zoe scrub` is not built. Its card reads `text_deltas` as it reads `records`.
- The bit-flip harness flips bytes of the rows of `text_deltas` as it does for records, and checks that every read through the core gives `CorruptText`.

### 8. The write path

- Core: `Txn::text_set(catalog: Oid, index: &str, rid: i32, language: &str, text: &str)` and `Txn::text_unindex(catalog, index, rid)` stage a delta between the begin and the vote, from the thread that drives the transaction, with the rules of `Txn::kv_set`: a later write of a key in the transaction replaces the staged one, and an abort drops it. A restore transaction refuses them and takes `Txn::restore_text`, which restores a delta at the tid of the transaction. `Store::text_at(catalog, index, rid, tid)` reads a text at a snapshot, and `Store::text_changed(catalog, after, last)` scans `text_txn`.
- Wire: the protocol extension `text` (**open point 2**: its name), with the calls `text_set(catalog, index, rid, language, text)` and `text_unindex(catalog, index, rid)`. They have no reply, stage into the open transaction in its lane, and an error fails the vote, as `kv_set`.
- Python: `zoe.Storage.text_set(catalog, index, rid, text, language, transaction)` and `zoe.Storage.text_unindex(catalog, index, rid, transaction)`, and the same two methods on `zoe.zeo.ClientStorage` in its native mode. `ZoeTextIndex` keeps the deltas of a transaction in a data manager until the commit, as `zoe.kv` keeps its writes, and calls these methods in the commit.
- A storage without the write path: a stock ZEO client, the asyncio mode of `zoe.zeo.ClientStorage`, FileStorage. The profile step refuses it with `Unsupported`. `ZoeTextIndex.index_object` on such a storage raises `Unsupported`, so the transaction fails and no instance writes content whose text no delta holds (**open point 3**).

### 9. A `DemoStorage` (N1)

On a `DemoStorage` whose chain of bases ends at a ZOE storage, the deltas of a commit go to an overlay in memory that belongs to the `DemoStorage`, as the namespace writes of `zoe.kv` (ADR 0047, section 9): they never reach the base, and they go with the `DemoStorage`. The overlay is a text index in the process, kept up to date at each finish of the `DemoStorage`: an index in memory of the documents that the overlay holds, over the index of the base at the tid of the base of the `DemoStorage`, with the mechanism of section 13. A query on the `DemoStorage` runs on the index of the base without the documents of the overlay, then on the overlay with the statistics of both. A stacked `DemoStorage` adds its overlay over the one below. So the Plone suites and the test layers of a site run with `ZoeTextIndex`.

### 10. The index and its directory

- One tantivy index for each registered catalog (ADR 0059), in `text/<oid>/` of the store directory, `<oid>` as 16 lowercase hexadecimal digits. The store holds the directory: only the process that holds the lock of the store opens a writer. The payload of each tantivy commit holds the tid up to which the index holds the deltas.
- The index is a cache. At the open, a missing directory, one that tantivy cannot open, a tid before the pack watermark or after the last tid of the store, or settings that changed (section 14) start a rebuild from the deltas in the background. Otherwise the open reads the deltas after the tid of the index through `text_txn` and applies them.
- A writer commits the deltas behind the tid in batches: every `text-commit-interval` (500 ms) or every 10,000 deltas. `zoe_text_commit_lag_seconds` is the age of the oldest committed delta that the index does not hold yet, 0 when it holds all.
- A query while a rebuild runs waits for it, up to the timeout of the query, then fails with `Unavailable` (**open point 4**). ZCTextIndex is not there to take the query.
- A copy of a closed store copies `text/` with the store, and the copy opens it. A copy of an open store can copy a torn directory: the open of the copy rebuilds it. A backup holds no `text/`. A fork (phase 7) answers from the index of its base at the fork tid with the overlay of section 9.
- The prototype opens its reader with the manual reload policy: it starts no thread. A writer with one indexing thread starts 6 threads. The extension starts them at the open of a store that holds a registered catalog, never at import.

### 11. The two modes

- The enhanced mode, the default of `ZoeTextIndex`: the analyzer of the site language (a Snowball stemmer, ASCII folding, the stop words of the language), the BM25 of tantivy (`k1` 1.2, `b` 0.75), phrases, prefixes, fuzzy matching (off by default), and the boosts `Title^3`, `Description^1.5`, `SearchableText^1` for a query on `SearchableText`. One tantivy field for each text index; with plone.app.multilingual, one field for each language of the site, chosen from the language of the delta.
- Both modes index, beside the words of each field, a field of the pairs of words in a row. A phrase of two words reads the posting list of one pair; a longer phrase intersects its pairs, then checks the positions of the documents that hold them all. On the texts of `catalog-1m`, whose 2,000 words are all frequent, a phrase of two words read through the positions alone is the slowest query by far (finding, section 6).
- The compatibility mode: the words of the lexicon `plone_lexicon` of Plone 6.1, the Okapi score of `OkapiIndex` in its integer form, and the order of `Catalog.search`. tantivy gives the term dictionary, the postings with their frequencies and positions, and the deletes. ZOE keeps beside it the number of words of each document in a fast field, and computes the score itself: the norms of tantivy round the length, and its BM25 has another `idf`. The finding shows the results identical to ZCTextIndex on every query of the comparison, with the transliteration table of the Unidecode release that Plone 6.1 pins: the splitter of Plone transliterates the words that start with a Latin letter with Unidecode, and the tables of the Rust crates differ from it. ZOE generates its table from that release (**open point 5**).
- The scores of the compatibility mode are those of ZCTextIndex inside the window. Outside it, Q9 gives the statistics of the current corpus.

### 12. The query translator and the text clause

`ZoeTextIndex` parses a query with the `QueryParser` and the lexicon pipeline of ZCTextIndex, in Python, so the syntax (`AND`, `OR`, `NOT`, parentheses, quoted phrases, `*` and `?`) and the errors stay those of ZCTextIndex. It sends a plan of the parse tree: `and` with `items` and `nots`, `or`, `atom` with its words, `phrase` with its words, `glob` with its pattern. The server runs the plan in the mode of the index. A glob that starts with `*` or `?` raises `QueryError` in the client, as ZCTextIndex raises it.

The text clause of a plan of `catalog_query` (ADR 0059) holds the index id, the plan, and the staged texts of section 13. Its result is the rids with their scores, which the mirror intersects with the other clauses. The mirror keeps the map from a rid to its tantivy document. When the other clauses give fewer rids than the text clause, their bitmap is a document set of the tantivy query, so the text clause reads the postings of those documents only and the pruning of tantivy keeps working; with `sort_on` set, the text clause gives its hits without a score and the mirror sorts them with its sort array. The finding measures a filter checked on each hit after the scores: it misses G4 on the words of the top band. With `sort_on` unset, the order is the score descending, then the rid descending, as `IIBucket.byValue(0)`. Each other clause whose result is a set adds 1 to the score, as `weightedIntersection` adds it in `Catalog.search`, so `data_record_score_` stays that of ZCatalog.

### 13. Consistency: the two mechanisms of Q9

Both mechanisms replace some documents of the index, for one query, by other versions of their texts. A replaced document leaves the result of the index, and an overlay in memory answers for its new version, with the statistics of the index. The overlay is direct: it splits the texts of the queried index and scores the words of the query in each document, with no tantivy segment, whose build costs 1 to 2.5 ms before the first document.

- A snapshot that the index does not hold: when the tid of the index is after the snapshot, the documents that changed after the snapshot, from a scan of `text_txn`, get their text at the snapshot from `text_deltas`. A document deleted after the snapshot is one of them: so the same mechanism removes the miss of a document that the index already dropped, inside the window as outside it. When the tid of the index is before the snapshot, the documents that changed between the two get their text at the snapshot. The matched set is exact at any snapshot. The scores use the statistics of the current index.
- The transaction that indexes: the client sends, with the query, the texts that its transaction staged: a list of `(index, rid, language, text)`, `text` null for an unindex. The overlay holds them. In the compatibility mode the statistics are those of the index with the overlay in place of the replaced documents, so the scores are those that ZCTextIndex gives inside the transaction.

The finding gives the cost of each as a function of the documents.

### 14. `ZoeTextIndex`

- A ZCatalog index class of `zoe.plone`, meta type `ZoeTextIndex`. Its settings: `indexed_attrs`, `mode` (`enhanced` or `compat`), `language`, the boosts, `k1` and `b`. The settings of each index of a catalog are kept in the durable namespace `zoe.text` of the store, under the oid of the catalog, so that `zoe catalog rebuild-text` reads them without Plone (**open point 6**).
- `index_object` stages the text of each indexed attribute, joined as ZCTextIndex joins them; an attribute that gives no text stages nothing, so the entry stays, as in ZCTextIndex. `unindex_object` stages an unindex. `_apply_index` sends the text clause. `numObjects`, `indexSize` and `getEntryForObject` read the index of the server.
- The profile step `zoe.plone:text` replaces the three ZCTextIndex indexes of `portal_catalog`, `SearchableText`, `Title` and `Description`, with `ZoeTextIndex` indexes of the same ids, then indexes the catalog in batches, which writes the deltas. The way back is the step `zoe.plone:text-uninstall`: it stages an unindex for each document of each `ZoeTextIndex`, puts the ZCTextIndex indexes back and reindexes them in Python.

### 15. `zoe catalog rebuild-text`

`zoe catalog rebuild-text STORE [--catalog OID]` runs on a store folder that no process holds: it takes the lock of the store and refuses a held one. It reads the current delta of each key of each catalog, and the settings of `zoe.text`, writes a new tantivy directory `text/<oid>.new`, then renames it over `text/<oid>` and syncs `text/`. It writes nothing in the tables. It prints the documents and the time of each catalog. The naming table lists the subcommand `catalog` with `rebuild-text`, and `migrate`.

### 16. The cards

The split of the plan stands: ZOE-293 holds the key spaces, the format name, the migration tool, the op of the log, verify and the bit-flip harness; ZOE-294 holds pack, undo, the dumps and `zoe convert`; ZOE-295 holds the write path, the index of each catalog, its directory, the lag metric and the text clause; ZOE-296 holds `ZoeTextIndex`, the translator, the overlay of a `DemoStorage` and the profile steps; ZOE-297 holds the compatibility mode and its transliteration table; ZOE-298 holds `zoe catalog rebuild-text`. The settings namespace `zoe.text` goes with ZOE-296, and its read in `rebuild-text` with ZOE-298.

## Open points

1. The name of the migration tool: `zoe migrate`.
2. The name of the protocol extension and of its calls: `text`, `text_set`, `text_unindex`.
3. An instance without the write path: `index_object` raises `Unsupported`.
4. A query while the index is rebuilt: it waits up to its timeout, then fails with `Unavailable`.
5. The transliteration table of the compatibility mode: generated from Unidecode 1.4.0, the release that Plone 6.1 pins, and changed only with a release of Plone that changes the pin.
6. The settings of `ZoeTextIndex` in the durable namespace `zoe.text`.
7. tantivy behind a cargo feature `text` of `zoe-catalog`, on in `zoe serve` and in the wheel: it adds 9.4 MB to a stripped release binary.

## Consequences

- A save of a document writes three deltas in place of the buckets of three ZCTextIndex indexes; the text indexes leave the conflicts of saves.
- A store of format 3 needs the migration tool. The fixtures move with `prepare`.
- The tantivy directory is a cache: losing it costs a rebuild, about 4 minutes for `catalog-1m`.
- A site on FileStorage or on a stock ZEO client cannot use `ZoeTextIndex`.
- Snippets, the `@search` extension of plone.restapi and the labelled query set stay out of phase 6.

## Alternatives

- Text in the slots of the catalog records, or in a durable namespace: no history per tid, so no exact snapshot; Q8 chose a record type.
- The text index answers outside the window from ZCTextIndex: `ZoeTextIndex` replaces ZCTextIndex, so no Python index is left to answer; Q9 chose the delta history.
- The scorer of tantivy for the compatibility mode: its `idf`, its rounded norms and its `f32` scores give other scores and other ties.
- A Rust port of the parser of ZCTextIndex: the parser in Python is the reference, and the plan keeps it.
