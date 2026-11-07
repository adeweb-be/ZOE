# Finding: the native text index on tantivy, its delta record type, and the compatibility with ZCTextIndex

Card ZOE-284; requirement F18, "Native BM25 text index", the risk row "Text index lags the tid or diverges from ZCTextIndex ranking", "Data model", "Record framing", "Pack", "The write-ahead log" and the naming table of the specification, N13 and N15; the decisions Q8, Q9, Q15 and N1 of the plan of phase 6. The prototype and the scripts are on the branch `spike/ZOE-284` at [`1b3d7a42`](https://github.com/adeweb-be/ZOE/tree/1b3d7a42b96cd15814c51b922b6a4c55c075ae2d), not for merge. Sections 1 to 7 ran at [`a11a75e7`](https://github.com/adeweb-be/ZOE/tree/a11a75e7622fd18ffc2cc87345bd2ea6f276aa81); section 8 adds the filter and the sort inside the search. ADR 0060 (accepted) gives the design that follows from this finding.

**The numbers, in short.**

- **G4, a text query under 20 ms at p99 on `catalog-1m`: met in both modes, with the filter and the sort inside the search (section 8).** On the 12 terms of the plan, with the filter of a listing (495,943 rids), a selective filter (1,207 rids), a sort, or the filter and the sort, the worst p99 is 6.7 ms in the enhanced mode, 6.1 ms in the compatibility mode and 9.7 ms with the boosts over three fields; alone, 4.8, 13.2 and 6.4 ms. A check of each hit after the scores (sections 4 and 6) gave 34 to 72 ms on the 3 words of the top band, which match 77 % of the entries. That was the first round, with another spike on the machine. In the round of section 8, the same check gave 19.0 to 26.4 ms with a filter. The limits that stay: a query with no `sort_on` and no `b_size` orders every hit, and a phrase of two frequent words takes up to 30 ms in the compatibility mode, alone as with a filter.
- **Build and size.** One million entries, three text fields: 196 s (compatibility) and 243 s (enhanced) with 4 threads, merge included; 1.40 GB of index; 3.0 to 3.3 GB peak memory. A field of pairs of words makes a phrase of two words one posting list; it holds 60 % of the size.
- **The deltas.** A save writes three rows of 3,105 bytes in all, against 458 records and about 420 KB for ZCTextIndex on the same fixture. Every two saves of different documents share 74 records of ZCTextIndex on average; they share no delta.
- **Compatibility.** On the 1,472 queries of 1,640 that the parser of ZCTextIndex accepts, over the texts of `catalog-mini` and 3,000 synthetic documents of many scripts, the rids, the order, the counts and the integer scores equal those of ZCTextIndex, with a transliteration table generated from Unidecode 1.4.0; without it, 35 queries differ. On `catalog-1m`, all 111 queries are identical with the index of section 8; the index of the first measures gave 6 queries that differ by 1 in a few scores, from its corpus or its build, not from the score. tantivy gives the postings and the dictionary; ZOE keeps the exact length of each document, the counts, and the score of `OkapiIndex`.
- **The two mechanisms of Q9.** Both give the exact matched set at the snapshot, also for a document deleted after it. The overlay of a transaction adds 1.5 to 3.2 ms for 1 to 10 staged documents with a direct overlay, against 10.3 to 19.2 ms in all with a tantivy segment in memory (its build alone: 6.4 to 15.0 ms). At an old snapshot, the cost is about 0.06 ms for each document changed after the snapshot: 88 ms for 1,000, 0.6 s for 10,000.
- **Threads and size.** A reader starts no thread; a writer with one indexing thread starts 6. tantivy adds 9.4 MB to a stripped binary.

**The design** is in ADR 0060 (accepted); its section 17 gives its names, its errors and its settings.

## Question

The seven questions of the card: the delta record type (Q8); the write path, also on a `DemoStorage` (N1); whether a compatibility switch gives the rids, the order, the counts and the scores of ZCTextIndex, and what ZOE keeps beside tantivy for it; the enhanced mode; the two mechanisms of consistency of Q9; the costs; and the place of the code, of the directory and of `zoe catalog rebuild-text` (Q15).

## Method

**The prototype.** On the branch:

- `crates/zoe-core/src/format/text.rs` and `crates/zoe-core/src/text.rs`: a scratch key space `text_deltas` with checksum tag 9 and its index `text_txn`, written with the frame and the checksum of the core into a redb file of their own. It measures the bytes of a delta, the read of a text at a snapshot and the scan of a tid range. It does not go through the staging, the commit batch or the log.
- `crates/zoe-catalog/src/text/`: one tantivy 0.27 index of the three text indexes of a catalog, `SearchableText`, `Title` and `Description`, in either mode (`index.rs`); the analyzer of `plone_lexicon` and the enhanced analyzer (`analysis.rs`); the executor of the compatibility mode on the postings of tantivy (`compat.rs`); the translation of a plan into a tantivy query (`enhanced.rs`); and the overlay of both mechanisms of Q9 (`overlay.rs`). A filter of the mirror is a roaring bitmap of rids, applied on the fast field `rid`.
- `python/zoedb/src/zoe/plone/textquery.py`: the translator: the parse tree of ZCTextIndex's `QueryParser` as a plan, with the words of the lexicon.
- `crates/zoe-catalog/examples/textspike.rs` and the scripts of `crates/zoe-catalog/spike/` run the measures; `spike/README.md` lists them.

**The texts.** `corpus.py` writes the texts of the plan of `catalog-1m` (ZOE-282, `bench/plone/src/plonebench/catalog/plan.py`), so the texts are those of the fixture: the generated words of `corpus.make_words`, a vocabulary of 2,000 words with a Zipf spread. 1,000,000 entries, 2.98 GB of JSON; a Document or a News Item has 3 to 8 paragraphs of 40 to 120 words in `SearchableText`.

**The queries of the measures.** `queries.py` writes 111 queries on `catalog-1m`, each as the plan that the translator gives: the 12 text terms of the plan of the fixture (3 in each of the bands low, mid, high and top, matching 2.3 %, 10 %, 35 % and 77 % of the entries), 20 queries of two words, 10 `OR`, 5 `AND NOT`, 10 phrases of two words taken from the texts, 10 prefixes of three letters with `*`, 24 queries of the search form of Plone (`munge_search_term` joins 1, 2 or 3 words with `AND` and adds `*` to the last), and 10 words on each of `Title` and `Description`. Each query runs in five variants: alone (the 20 first by relevance and the count), with the filter of the listing of the plan (`portal_type` Document and `review_state` published: 495,943 rids), with a selective filter (`Creator` editor-000: 1,207 rids), with a sort on `effective` (descending, the 20 first), and with the filter and the sort. One warm pass, then 5 rounds; the p99 is over all the runs of a variant.

**The compatibility.** `zctext.py` builds, in Python, an `OkapiIndex` with the lexicon of `plone_lexicon` of a Plone 6.1.5 site (the "Unicode Whitespace splitter" and the "Unicode Ignoring Accents Case Normalizer" of Products.CMFPlone 6.1.5, and `baseNormalize` of plone.i18n 5.1.1 with Unidecode 1.4.0, the releases that Plone 6.1.5 pins), for each of the three indexes, and indexes the documents as `ZCTextIndex.index_object` does: an attribute that is `None` gives no entry, an empty text gives an entry of no word. Products.ZCatalog 7.2.1, CPython 3.13. The documents: the 10,000 entries of `catalog-mini`, and 3,000 synthetic documents of generated character sequences of many scripts (Latin with accents, Latin extended, Greek, Cyrillic, CJK, kana, Hangul, Thai, Arabic, Devanagari, numbers of other forms, compatibility characters, combining marks), mixed with the words of the vocabulary in upper, lower and title case and with accents, and joined by spaces, hyphens, apostrophes, dots, slashes, underscores and no-break spaces. No natural language. The query set: for each index, 25 words of the vocabulary, 25 words of the texts, 10 words in another case, 10 with accents, 20 of each of two words, `AND`, `OR`, `NOT`, `AND NOT` and a group in parentheses, 30 phrases of two or three words of the texts, 40 globs with `*` and `?`, 10 mixed, and 9 queries of bad syntax; the same families on the synthetic documents. 1,640 queries. For each query, the script runs `QueryParser(lexicon).parseQuery(q).executeQuery(index)` and writes the plan of the translator and the result in the order of `Catalog.search`: `IIBucket.byValue(0)`. The prototype builds its index of the same documents and compares the rids, the order, the count and the scores.

`zcquery.py` runs the 111 queries of the measures through the real ZCTextIndex of the fixture (its lexicon and its `OkapiIndex`, read-only), and `compare1m.py` compares the count, the sum of the scores and the 200 first hits with the prototype in the compatibility mode, the rids of the fixture mapped to the entries of the plan by their paths.

The documents that the Plone suites create did not run: their texts are natural language, which the invariants of the prototype exclude, and they need the probe of ZOE-292. The synthetic set covers the character classes of the splitter instead.

**The records of ZCTextIndex.** `zcsave.py` opens the packed `Data.fs` of `catalog-1m` read-only under a `DemoStorage`, finds the three ZCTextIndex indexes of `portal_catalog` and reindexes 60 documents, one transaction each, with the texts of other entries of the plan, as `index_object` does. It counts the records and the bytes of each transaction in the changes of the `DemoStorage`, and the records that two saves both write. The Plone classes are absent: the site and the tool load as broken objects, and the script reads their state.

**Machine and builds.** The workstation of `ghost-to-cache.md`: Intel Core Ultra 7 155H (16 cores, 22 threads), 30 GB, Linux 7.0. `main` at `0abe0ca5`, rustc 1.99, tantivy 0.27.0, roaring 0.12, release builds. Another spike (ZOE-285) ran on the machine at the same time; the heavy steps of both took a shared lock, so they did not overlap, but the light steps did. The build used 4 indexing threads. The query measures ran in one thread. The stores, the indexes and the raw results are not in the repository; the scripts on the branch give them again.

## 1. The delta record type

ADR 0060, section 1, gives the key and the frame. The prototype writes them with the frame code of the core.

**The bytes of a save.** The deltas of the 1,000,000 entries of `catalog-1m`, three for each, `SearchableText`, `Title` and `Description`, language `en`, codec 0, in 1,000 commits of 1,000 entries:

| | Value |
| --- | ---: |
| rows of `text_deltas` | 3,000,000 |
| bytes of their keys and frames | 3,011,811,206 |
| bytes of the keys of `text_txn` | 93,000,000 |
| bytes for one save of one entry, both key spaces | 3,105 |
| of which the texts | about 2,815 |
| overhead of one delta: key (26 to 35 bytes), frame header (32), language (3), key of `text_txn` (26 to 35) | about 97 |
| payload with zstd at level 3, against codec 0 | 0.45 |
| redb file after the load | 6.44 GB |
| load | 105.8 s |

A save of one entry writes three rows of 3.1 KB in all, with codec 0. With the setting `zstd`, the texts of the fixture shrink to 0.45 of their size; generated words compress better than natural text.

ZCTextIndex on the same fixture (`zcsave.py`, 60 saves, one entry each, the three indexes reindexed with the texts of other entries): a save writes 458 records (mean; median 462) and 390 KB of pickles, about 420 KB in a ZOE store with the frame, the key and the key of `txn_oids` of each record. By class, over the 60 saves: 21,879 `IIBucket` (the postings of the words in `_wordinfo`, the lengths in `_docweight`), 5,205 `IOBucket` (`_wordinfo` and `_docwords`), 217 `IIBTree` and 180 `Length` (`document_count` and the total length of each index). So the deltas write 135 times fewer bytes, and 3 rows in place of 458 records.

**The conflicts of two saves.** Every pair of the 1,770 pairs of these saves of different documents writes records in common: 74 on average, the `Length` counters and the buckets of the frequent words. Tier 1 merges most of them, and the ones it refuses are the first cause of failed saves (`posting-dicts.md`). Two saves of different documents write no delta in common: the key holds the rid. Two saves of one document write the same keys. They already conflict on the content object, and ADR 0060 (section 8) checks the deltas of one key against each other.

**Reads.** A read of the text of one document at a snapshot, one reverse range scan: 1.6 µs at p50 and 3.4 µs at p99 on the 30,000 deltas of `catalog-mini`, warm; 140 µs and 571 µs on the 6.44 GB store of `catalog-1m` read at random right after the load, mostly from the disk. A scan of `text_txn` over 1,000 commits (3,000 deltas): 0.84 ms on `catalog-mini`, 11.4 ms on `catalog-1m`. A read of every frame of `text_deltas` with its checksum, as `zoe verify` and `zoe catalog rebuild-text` read them: 22.1 s for the 3,000,000 rows of `catalog-1m`.

**The rest of Q1** (the frame and its tag, the format name and its migration tool, the log and the unfolded table, pack, `zoe verify`, `zoe scrub`, the bit-flip harness, the dumps, `zoe convert`, undo and the iterator) is design, not measure: ADR 0060, sections 1 to 7.

## 2. The write path

The prototype writes the deltas into its own redb file, not through `Txn`. The design of ADR 0060, sections 8 and 9, follows the namespace writes of ADR 0043, for these reasons:

- A delta, as a namespace write, has no oid and takes no lock. So it stages as `Txn::kv_set` stages, and goes over the wire as `kv_set` goes: no reply, in the lane of the transaction. ADR 0060 adds a check of the deltas of one key in the vote and the finish.
- `ZoeTextIndex.index_object` runs before the storage transaction begins: in the processing of the indexing queue, at a search or at `before_commit`. So it keeps the deltas in a data manager until the commit, as `zoe.kv` keeps its writes, and these same deltas are the staged texts that a search in the transaction sends with its query (Q9, ADR 0060 section 13).
- On a `DemoStorage` the deltas of a finish go to the memory of the `DemoStorage`. Each query sends them to the base as staged texts, and the direct overlay of section 5 answers for them (ADR 0060, section 9).

## 3. Compatibility with ZCTextIndex

**Yes, with two things beside tantivy and one table.** On the 1,640 queries of the comparison, 168 fail in `QueryParser` with `ParseError` or `QueryError` (for example `a NOT b`, `(a OR b) c`, a glob that starts with `*`): the translator runs the same parser, so they fail the same way. Of the other 1,472, all are identical to ZCTextIndex: the same rids, in the same order, with the same count and the same integer scores. 1,012 of them have ties of score, so the order of the ties is checked too. 215 have no hit. The families: words, words in another case and with accents, `AND`, `OR`, `AND NOT`, groups, phrases, globs with `*` and `?`, and mixed queries, on the three indexes, on the texts of `catalog-mini` and on the synthetic documents of many scripts. On `catalog-1m`, all 111 queries of the measures give the same count, the same sum of all the scores and the same 200 first hits as the ZCTextIndex of the fixture, with the corpus written again from the plan and the index built again for section 8. The index of the first measures gave 6 queries with the same rids and counts and some scores that differ by 1 (sums that differ by 1 to 18 over 1,523 to 247,968 hits). Its output, compared with the same run of ZCTextIndex, still gives these 6. The code of the score is the same in both, so the cause is in the corpus or the index of the first measures; that corpus is no longer on disk. ZCTextIndex itself takes 233 ms at the median for these queries, and 7.7 s for the slowest, a phrase; the prototype takes at most 95 ms to rank all the hits of a query.

What tantivy gives: the term dictionary, so the expansion of a glob over the words of the lexicon in code-point order, as `Lexicon.globToWordIds`; the postings with the frequency of each word in each document, and its positions, so a phrase matches the words in a row, as the `WidCode` search of `search_phrase`; and the deletes.

What ZOE keeps beside it, and what the prototype computes:

- The number of words of each document in each index, exactly, in a fast field. The norm of tantivy is a byte that rounds a length above 40 words, and `OkapiIndex` divides by the exact length.
- The number of documents and of words of each index: `N` and the mean length of `OkapiIndex`. A document with an empty text counts, with no word, as `index_object` counts it.
- The document frequency of a word without the deleted documents: tantivy keeps a deleted document in the frequency of the term dictionary until a merge. The prototype takes the frequency of the dictionary when a segment has no delete, and counts the live postings otherwise.
- The score: `int(tf * idf * 1024 + 0.5)` with `idf = ln(1 + N / df)`, `tf = f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / mean))`, in `f64` and in the order of operations of `OkapiIndex._search_wids`. The scores of the words add up in `AND`, `OR`, phrases and globs, a `NOT` keeps the scores of its left side, and an atom with no word is left out of an `AND`, as `ParseTree` does. The order is the score descending, then the rid descending: `IIBucket.byValue(0)` sorts the pairs and reverses them.

The BM25 of tantivy cannot serve here: its `idf` is `ln(1 + (N - df + 0.5) / (df + 0.5))`, its lengths are the rounded norms, and its scores are `f32`.

**The mismatches, by kind.**

| Kind | Seen | Cause | What removes it |
| --- | --- | --- | --- |
| Transliteration | 35 of the 1,472 queries differ with the `unidecode` crate (0.3.0); 306 of the 3,000 synthetic documents get other words | The normalizer of Plone runs Unidecode on each word that starts with a character from U+0041 to U+024F. The tables of the crate differ from Unidecode 1.4.0 for some characters: the crate gives `[?]` where Python gives nothing, and the reverse for the apostrophe of some letters. | A table generated from Unidecode 1.4.0, the release that Plone 6.1 pins: one string for each character of the basic plane (`ZOE_UNIDECODE_TABLE` in the prototype). With it, 0 of 3,000 documents and 0 of 1,472 queries differ. |
| The Unicode version | not seen | Python's `re` and `unicodedata` follow the Unicode version of the interpreter (15.0 on 3.12, 15.1 on 3.13, 16.0 on 3.14). A character assigned in a later version can be a letter in one and not in the other, for `\w` and for NFKC. | The tables of the Rust crates follow one version. A site whose texts hold characters of a newer version than its interpreter knows can differ on those characters. The finding names it; the switch documents it. |
| The query parser | none | The parser, the stop words of a lexicon and the errors stay in Python. | Nothing to remove. |
| An empty text, a missing attribute | none, once the prototype followed `index_object` | `index_object` indexes an empty text as an entry of no word, and leaves the entry as it is when the attribute gives `None`. | `ZoeTextIndex.index_object` stages an empty text, and stages nothing for `None`. |
| The score in a combined query | none in the comparison: it runs one index | `Catalog.search` intersects the result of each index with `weightedIntersection`, which adds 1 to the score for each other index whose result is a set. The order does not change; `data_record_score_` and the normalized score do. | The mirror adds 1 for each clause that gives a set (ADR 0060, section 12). |
| The statistics at an old snapshot | not measured | The prototype scores the replaced documents with the statistics of the current index, as Q9 allows. | ADR 0060 (section 13) corrects N, the total length and the document frequencies by the replaced documents, so the scores are those at the snapshot. The prototype did not build it. |
| An overflow of a sum | not seen | The values of an `IIBucket` are 32-bit. A glob that matches thousands of words in one document can pass 2^31 in ZCTextIndex. | The prototype adds in `i64`. Such a query is not a real query. |

The documents of the Plone suites did not run (see "Method"): their natural-language texts would add no character class that the synthetic set does not hold, but the comparison on them is the gate G1 of ZOE-297.

## 4. The enhanced mode

What tantivy 0.27 gives, with its default features:

- **Stemmers.** Snowball stemmers for 18 languages: Arabic, Danish, Dutch, English, Finnish, French, German, Greek, Hungarian, Italian, Norwegian, Portuguese, Romanian, Russian, Spanish, Swedish, Tamil and Turkish. No stemmer for the other languages of a site: those fields fold and lower the case only.
- **Folding and case.** `AsciiFoldingFilter` and `LowerCaser`.
- **Stop words.** Built-in lists for 13 of those languages (not Arabic, Greek, Romanian, Tamil, Turkish); a site can give its own list.
- **Phrases.** `PhraseQuery` on the positions. The prototype adds the field of pairs (section 6).
- **Prefixes.** `RegexQuery` on the term dictionary, an automaton over the FST: a glob of the translator is a prefix of the folded word, not stemmed, so a prefix of a stem still matches.
- **Fuzzy matching.** `FuzzyTermQuery`, a Levenshtein automaton over the term dictionary, at distance 1 or 2, with transpositions.
- **Boosts.** `BoostQuery` around the query of each field: a query on `SearchableText` becomes `Title^3 OR Description^1.5 OR SearchableText^1`, each word a disjunction over the three fields.
- **Statistics of another index.** `Searcher::search_with_statistics_provider`, which the overlay of section 5 uses, so a document of the overlay scores with the statistics of the main index.
- **One field for each language.** A field is a name and an analyzer: with plone.app.multilingual, `SearchableText` becomes one field for each language of the site, and a document goes to the field of the language of its delta. The prototype did not build it; a query in one language reads one field, so the cost of a query stays that of one field, and the index holds each document once.

The prototype runs the enhanced mode with the English stemmer and stop words; the generated words of the fixture are not English, so the stemmer cuts some of their endings, as it would for a word of another language.

The costs on `catalog-1m`, p99 in ms, the 12 terms of the plan and the slowest families of the synthetic set, with the check of each hit after the scores (the full table is in section 6; section 8 gives the filter and the sort inside the search):

| Enhanced | Alone | Filter | Selective | Sort | Filter and sort |
| --- | ---: | ---: | ---: | ---: | ---: |
| `SearchableText` only, band terms | 7.7 | 34.0 | 25.3 | 20.8 | 41.1 |
| boosts over the three fields, band terms | 13.7 | 57.5 | 38.3 | 32.8 | 50.3 |
| boosts and fuzzy distance 1, band terms | 25.3 | 65.2 | 42.0 | 39.8 | 54.0 |
| boosts, the search form of Plone (1 word `*`) | 24.3 | 55.1 | 37.7 | 28.7 | 39.3 |
| boosts and fuzzy, two words | 63.7 | 34.4 | 26.3 | 28.5 | 26.2 |

The boosts read three fields in place of one: about 1.8 times the cost of a query alone. Fuzzy matching at distance 1 doubles it again, and more for a query of several words, each of which expands to its neighbours in the dictionary: with 2,000 words of 4 to 8 letters, a word of the fixture has many neighbours at distance 1. A natural vocabulary has fewer.

## 5. Consistency: the two mechanisms of Q9

Both mechanisms run the query on the main index without the replaced documents, then on an overlay of their new versions with the statistics of the main index, and merge the two results. The measure: the query `nalu` (a word of the mid band, 100,000 hits) on `catalog-1m`, after 10,000 documents (one entry in 100) changed, each at a tid of its own: 9 in 10 got the text of another entry, 1 in 10 an unindex. A snapshot at k tids before the last tid has k documents changed after it. Best of 5 runs, ms.

| Changed or staged documents | 1 | 10 | 100 | 1,000 | 10,000 |
| --- | ---: | ---: | ---: | ---: | ---: |
| scan of `text_txn` after the snapshot | 0.02 | 0.03 | 0.19 | 1.8 | 15.4 |
| read of their texts at the snapshot (3 indexes) | 0.04 | 0.14 | 1.0 | 11.1 | 79.8 |
| compatibility, overlay as a tantivy segment: build | 6.4 | 15.0 | 98 | 911 | 6,677 |
| compatibility, query with that overlay | 3.9 | 4.2 | 5.8 | 6.6 | 13.2 |
| compatibility, direct overlay, at an old snapshot: build and query | 3.7 | 5.0 | 13.4 | 75.7 | 540 |
| compatibility, direct overlay, in the transaction: build and query | 3.9 | 5.6 | 13.5 | 74.4 | 604 |
| enhanced, overlay as a tantivy segment: build | 3.7 | 9.2 | 60 | 513 | 5,511 |
| enhanced, query with that overlay | 2.2 | 2.3 | 2.5 | 2.5 | 2.6 |
| the same query with no overlay: compatibility, enhanced | 2.4, 1.4 | | | | |

- **The matched set is exact.** For every row the count of the overlay equals the count at the snapshot computed from the texts: the count of the index now, minus the changed documents that match now, plus those that match at the snapshot. The documents with an unindex after the snapshot, that the index no longer holds, are back in the result. So the mechanism of an old snapshot also removes, at any snapshot behind the index, the miss of a document deleted after the snapshot: the documents changed between the tid of the index and the snapshot are replaced in both directions.
- **The scores.** At an old snapshot the overlay of the prototype scores with the statistics of the current index, as Q9 allows; ADR 0060 corrects them in the compatibility mode (section 13), which the prototype did not build. In the transaction, the compatibility mode counts the index with the overlay in place of the replaced documents, and the scores are those of ZCTextIndex in the transaction: the direct overlay gives the same hits and scores as the segment, on every row.
- **A tantivy segment in memory is the costly part.** A segment of no document costs 1.1 ms to build, one document 2.2 to 2.6 ms (`overlaycost`), and then 0.5 to 0.7 ms per document of about 450 words with the field of pairs. The direct overlay of the prototype, for the compatibility mode, splits the texts of the queried index, scores the words of the query in each document, and needs no segment: 0.05 ms per document, mostly the analysis. The enhanced mode can take the same path, scoring with the BM25 of tantivy and the statistics of the main index; the prototype did not build it.
- **The cost as a function of the documents.** In a transaction that indexes, a save stages 1 to 10 documents: the direct overlay adds 1.5 to 3.2 ms to the query. At an old snapshot, the cost grows with the documents changed after the snapshot: about 0.06 ms per document with the direct overlay and the reads, so 1,000 changed documents cost 88 ms and 10,000 about 0.6 s. The text index has no window: only pack bounds the age of a snapshot, so nothing bounds this cost, and no timeout stops the call.

## 6. Costs

**Queries, p99 in ms on `catalog-1m`, one thread, warm, with the check of each hit after the scores.** The 12 terms of the plan are the query set of G4; "all 111" is the whole synthetic set. Section 8 gives the same queries with the filter and the sort inside the search.

| Mode | Set | Alone | Filter (495,943 rids) | Selective (1,207 rids) | Sort on `effective` | Filter and sort |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| compatibility | 12 terms of the plan | 19.1 | 35.0 | 19.5 | 38.9 | 71.6 |
| compatibility | the 9 terms of the low, mid and high bands | 5.9 | 10.0 | 6.0 | 7.2 | 10.8 |
| compatibility | all 111 | 25.3 | 38.3 | 24.0 | 27.7 | 40.7 |
| enhanced, `SearchableText` | 12 terms of the plan | 7.7 | 34.0 | 25.3 | 20.8 | 41.1 |
| enhanced, `SearchableText` | the 9 terms of the low, mid and high bands | 4.8 | 16.5 | 9.7 | 7.2 | 13.4 |
| enhanced, `SearchableText` | all 111 | 6.8 | 30.6 | 19.9 | 15.2 | 28.7 |
| enhanced, boosts | 12 terms of the plan | 13.7 | 57.5 | 38.3 | 32.8 | 50.3 |
| enhanced, boosts | all 111 | 17.8 | 44.7 | 28.4 | 26.3 | 39.6 |

The p50 of every row is 1 to 4 ms. The p99 comes from the 3 terms of the top band, which match 77 % of the entries, and from the broad globs of the search form of Plone (`plone-1`, a word with `*`: 16.6 ms alone in the compatibility mode, 24.3 ms with boosts) and the 3-letter prefixes. A phrase of two words reads one pair: 27 ms in the compatibility mode, which reads the frequency and the length of both words for each of its hits, and 6 to 9 ms in the enhanced mode. Without the field of pairs, the phrases of the fixture took 10 times longer on a corpus of 100,000 entries (22 and 46 ms there, against 2.2 and 0.3 ms with it).

**Against the target of 20 ms.** A text query alone meets it in the enhanced mode, on `SearchableText` (p99 7.7 ms on the terms of the plan, 6.8 ms on the whole set) and with the boosts (13.7 and 17.8 ms). The compatibility mode meets it alone on the terms of the plan (19.1 ms), not on the whole set (25.3 ms). With a filter or a sort, both modes miss it on the words of the top band. The cause is the same in every such row: the prototype scores each hit, then reads the rid of each hit from a fast field and looks it up in the bitmap of the filter, and reads the date of each hit for the sort. For a word in 770,000 entries that is 770,000 lookups after the scores, and the filter disables the pruning of tantivy (block-max WAND), which the query alone uses for its first hits. Section 8 moves the filter and the sort inside the search, and meets the target.

**The build.** `build` reads the 2.98 GB of JSON, analyses each text in 4 threads and writes the index, then merges it into one segment:

| Mode | Analysis and writing | Merge | In all | Size | Peak memory |
| --- | ---: | ---: | ---: | ---: | ---: |
| compatibility | 119.5 s | 76.7 s | 196 s | 1.40 GB | 2.98 GB |
| enhanced | 161.6 s | 81.3 s | 243 s | 1.40 GB | 3.34 GB |

Each mode holds the three fields, with positions, the three fields of pairs, the length of each field, the rid and the date. The fields of pairs hold about 60 % of the size: on 100,000 entries the index takes 68 MB without them and 166 MB with them. The peak memory is the arena of the 4 writer threads (256 MB each) and the documents in flight; one writer thread takes less. `zoe catalog rebuild-text` reads the deltas (22 s for all of them, section 1) while it builds, so about 4 minutes for `catalog-1m`.

**Memory and threads.** A reader of the 1.40 GB index maps it: the process of the measures held 63 to 92 MB resident after the queries, the page cache holds the rest. A reader with the manual reload policy starts no thread. A writer with one indexing thread starts 6 threads (the indexing thread, the segment updater and the merge pool), and stops them when it drops; with 4 indexing threads, 9 (`threads`). The extension starts them at the open of a store with a registered catalog, never at import.

**Size.** A stripped release binary that links the delta store of the core is 1.3 MB; with the text index of the prototype, tantivy, its stemmers and the analyzers, 10.7 MB: tantivy adds 9.4 MB. The `zoe` binary and the extension module would grow by about as much. The transliteration table of the compatibility mode is 0.42 MB as JSON.

## 7. Place

- **The crate.** The prototype lives in `zoe-catalog`, module `text`. tantivy adds 9.4 MB to a stripped release binary (section 6). ADR 0060 puts tantivy behind a cargo feature `text` of `zoe-catalog`, on in `zoe serve` and in the wheel, so a build without the text index stays small, and adds no crate name to the naming table.
- **The directory.** `text/<oid>/` in the store directory, one tantivy index for each registered catalog, with the tid of its last commit in the payload of the tantivy commit (ADR 0060, section 10). The lock of the store covers it: tantivy's own writer lock is a file inside the index directory and guards against a second writer of the same process tree only. A copy of a closed store opens its copy of the directory; a torn directory or one behind the pack watermark is rebuilt at the open; a dump holds no directory; a fork answers from the index of its base with an overlay.
- **`zoe catalog rebuild-text`.** It reads the current delta of each key, a full scan of `text_deltas` (22 s for `catalog-1m`), and builds the index (196 to 243 s with 4 threads): about 4 minutes for `catalog-1m`, the two overlapped. It writes only `text/`. Its row in the naming table: the subcommand `catalog` with `rebuild-text`, beside `migrate`, the migration tool of format 4.

## 8. The filter and the sort inside the search

**Question.** Section 6 shows the queries that miss G4: a word of the top band with the filter of a listing or with a sort, 34 to 72 ms at p99. Can a filter and a sort inside the search meet 20 ms at p99 in both modes? If not, what would it take?

**What the catalog must give.** `Catalog.search` of Products.ZCatalog 7.2.1:

- With no `sort_on`, the result of a text index has scores (an `IIBucket`). The catalog sorts it with `byValue(0)`: the score descending, then the rid descending. Then it slices the list to `b_start` and `b_size`, and `actual_result_count` is the number of all hits. So the text clause gives the count of all its hits in the filter, and the first `b_start + b_size` hits by score with their scores. Without `b_size`, it gives every hit in that order.
- With `sort_on`, the catalog gives the keys of the result to `sortResults`, and the scores are not used. So the text clause gives its hits as a set, with no score, and the sort of the mirror orders them.
- The search view of Products.CMFPlone 6.1.4 removes `sort_on` when it is `relevance`, and gives `b_start` and `b_size` (10 by default) to the catalog. `@search` of plone.restapi 9.15.6 gives `b_size` and `sort_limit` to the catalog when the request holds them.

The measures keep the first 20, as in section 6. A query with no `b_size` is measured as a part below.

**The design measured** (`src/text/filter.rs` of the prototype):

- The text index keeps in memory the rid of each of its documents and the document of each rid: 12 MB for 1,000,000 documents, built at the open from the fast field `rid`. The rows of the mirror sort as the rids (ADR 0059), so the same map serves the rows.
- The other clauses give a bitmap of rids. The smaller side drives the intersection:
  - A filter that holds fewer than one document in 32 drives. Its documents, sorted, are a document set of the query. In the enhanced mode, a `Must` clause with a score of 0 leads the intersection of tantivy. In the compatibility mode, each word seeks its postings to these documents.
  - A query whose hits are fewer than a quarter of the filter drives: the rid of each hit is looked up in the bitmap of the filter, as in section 6, but from the map in memory. The bound of the hits comes from the document frequencies: a word gives its frequency, a phrase the least of its words, `AND` the least of its items, `OR` the sum. In the enhanced mode a glob and fuzzy matching give no bound.
  - In the other cases, the filter becomes a bitset of the documents of each segment: 1.2 ms for 495,943 rids. In the enhanced mode, the collector checks this bitset in the callback of the pruning of tantivy (block-max WAND): a hit outside the filter does not enter the first 20, so the threshold of the pruning rises with the hits of the filter only. A second pass on the same weight counts the hits in the filter, with no score. A query with no bound has one pass that scores every hit, since its terms do not prune. In the compatibility mode, each hit gets its score of `OkapiIndex`, and the bitset only moves the end of the list, with no branch: a check that keeps one hit in two at random is a branch that the processor cannot predict. The lengths of the documents come from an array in memory (4 bytes for each document of each index), not from the fast field. A heap keeps the first 20 by score, then by rid descending.
- With `sort_on`, the text clause gives its hits as a bitset with no score; in the compatibility mode the parse tree runs on bitsets. The bitset of the filter cuts it, and the count is its population. The sort array of the mirror gives the first 20: for a large result, a walk of the rows in the order of the sort that stops at 20; for a small one, the best 20 of the result. The prototype has the sort array of one index, the key of each rid and the rids in its order; ties go by rid descending.
- Each other clause whose result is a set adds 1 to the score, as before (section 3).

**Method.** The same 111 queries and five variants as section 6: one warm pass, then 5 rounds (3 with fuzzy matching), one thread, the p99 over all the runs of a variant. One binary runs the check after the scores of section 6 ("before") and the path inside the search ("after") on the same index, in turn: before, after, three times. The tables give the median of the three runs. The Alone variant runs the same code in both. The machine is that of the method, `main` at `979273db` and the prototype at `1b3d7a42`; the desktop was in use, with a load average of 1.6 to 3.3. The corpus and the indexes of sections 1 to 7 were no longer on disk: `corpus.py` wrote the corpus again from the plan of `main`, and the same `build` command built each index again (4 threads, merged into one segment): 164 s in the compatibility mode and 168 s in the enhanced mode.

**The results are identical.** The command `check` runs both paths on each query and each variant with a filter or a sort: 444 results in each mode.

- The compatibility mode: 444 of 444 identical, rids, order, counts and integer scores.
- The enhanced mode on `SearchableText`: 263 identical. The other 181 are sorts on `effective`, and they differ only in the order of rows with the same key: the sort of tantivy orders such rows by document, the prototype by rid descending. The rule of the mirror (ADR 0059) gives that order.
- With the boosts: 260 identical, the same 181 sorts, and 3 phrases with the same rids, order and count whose scores differ in the last bit of an `f32` (1 in 10 million): the scores of three fields add in another order. With fuzzy matching: 253 identical and 191 sorts.
- `compat1m` on the index of this section against ZCTextIndex on the fixture (`compare1m.py`): 111 of 111 identical (section 3).

**The numbers.** p99 in ms on the 12 terms of the plan, the median of three runs. "First" is the table of section 6; "before" is the check after the scores in this run.

| Mode | | Alone | Filter (495,943 rids) | Selective (1,207 rids) | Sort on `effective` | Filter and sort |
| --- | --- | ---: | ---: | ---: | ---: | ---: |
| compatibility | first | 19.1 | 35.0 | 19.5 | 38.9 | 71.6 |
| compatibility | before | 16.7 | 25.0 | 12.1 | 18.9 | 26.4 |
| compatibility | after | 13.2 | 6.1 | 0.09 | 1.9 | 3.1 |
| enhanced, `SearchableText` | first | 7.7 | 34.0 | 25.3 | 20.8 | 41.1 |
| enhanced, `SearchableText` | before | 4.4 | 20.5 | 11.3 | 9.8 | 19.0 |
| enhanced, `SearchableText` | after | 4.8 | 6.7 | 0.11 | 1.9 | 3.1 |
| enhanced, boosts | first | 13.7 | 57.5 | 38.3 | 32.8 | 50.3 |
| enhanced, boosts | before | 5.9 | 22.7 | 13.3 | 11.2 | 20.0 |
| enhanced, boosts | after | 6.4 | 9.7 | 0.25 | 3.2 | 4.3 |
| enhanced, boosts and fuzzy distance 1 | first | 25.3 | 65.2 | 42.0 | 39.8 | 54.0 |
| enhanced, boosts and fuzzy distance 1 | before | 11.3 | 25.6 | 16.2 | 13.8 | 23.2 |
| enhanced, boosts and fuzzy distance 1 | after | 11.1 | 14.1 | 1.8 | 5.3 | 6.5 |

The 3 words of the top band give these p99s. Their p50 after: 5.8 ms with the filter, 1.8 ms with the sort and 3.0 ms with both in the compatibility mode; 6.3, 1.7 and 2.8 ms in the enhanced mode. The range of the three runs after is small: the compatibility mode with the filter gives 5.3 to 6.1 ms, the enhanced mode 6.6 to 7.3 ms. Before, the same cells gave 24.5 to 32.2 and 17.6 to 21.7 ms. The Alone variant runs the same code before and after: its spread (16.7 against 13.2 ms in the compatibility mode) is the effect of the state of the machine, and section 6 ran with another spike on the machine.

All 111 queries, p99 in ms, before and after:

| Mode | Alone | Filter | Selective | Sort | Filter and sort |
| --- | ---: | ---: | ---: | ---: | ---: |
| compatibility | 23.6, 27.6 | 35.2, 23.1 | 22.3, 0.17 | 25.2, 1.8 | 36.4, 3.0 |
| enhanced, `SearchableText` | 4.4, 4.7 | 20.2, 7.9 | 11.3, 1.2 | 9.6, 2.7 | 18.9, 3.8 |
| enhanced, boosts | 8.2, 9.6 | 19.2, 13.2 | 12.1, 1.9 | 10.2, 5.0 | 17.7, 11.3 |
| enhanced, boosts and fuzzy | 19.1, 20.7 | 24.5, 14.5 | 16.0, 2.4 | 13.3, 10.7 | 20.7, 11.8 |

The p99 of the compatibility mode over all 111 comes from the phrases of two words: 30 ms at p99 alone, 26 ms with the filter (37 ms before). Section 6 gives the cause: the phrase reads the frequency and the length of both words for each hit of the pair. Its filter and its sort do not add to it.

**Some families got slower, all under 10 ms at p99.** The families whose hits have no bound, or a loose one, pay the bitset of the filter (1.2 ms). At p50 they lose at most 1.9 ms, at p99 at most 3.7 ms, and no such family passes 9.1 ms at p99. They are the globs of the search form of Plone in the enhanced mode (`plone-2`, `plone-3` with a filter: p50 0.6 to 5.3 ms before, 1.8 to 6.6 ms after), the phrases of the compatibility mode (p50 0.7 ms before, 1.8 ms after), and `AND` with the boosts (p99 5.0 ms before, 8.7 ms after).

**The parts,** for the 3 words of the top band with the filter of the listing, median in ms (commands `parts` and `expt`):

| Part | Compatibility | Enhanced |
| --- | ---: | ---: |
| the filter of the listing as a bitset of documents (495,943 rids) | 1.2 | 1.2 |
| the filter of `editor-000` as documents (1,207 rids) | 0.02 | 0.02 |
| the hits in the listing with their scores | 7.1 | |
| the first 20 of them by score: a heap; a selection of all of them | 0.6; 1.5 | |
| the count of the hits in the listing, with no score | | 1.3 |
| the first 20 by score in the listing, pruned | | 3.7 |
| every hit in the listing in the order of the score (no `b_size`) | 16.7 | 35.1 |
| the hits as a bitset, with no score | 1.8 | 1.7 |
| the bitset of the hits and of the filter, and its count | 0.02 | 0.02 |
| the first 20 of the hits in the listing by `effective`: a walk of the order; the best 20 of the set | under 0.01; 1.3 | under 0.01; 0.9 |

In the compatibility mode, the scores of the hits of one word in the filter take 6.7 ms with a branch on the filter and the lengths from the fast field, 5.3 ms with the lengths from memory, and 3.1 ms with no branch and the lengths from memory. The postings alone, with their frequencies, take 0.7 ms.

**Documents in the order of the rids.** If the documents of the index followed the order of the rids, the filter would be a copy of the bitmap into a bitset: 0.9 ms in batches, against 1.2 ms through the map. The cost is the read of the roaring bitmap, not the map. So the order of the rids saves 0.3 ms for a broad filter, and it does not last: an update moves a document to a new segment, and a merge does not sort. No index was built in that order.

**Against G4.** Met in both modes. With the filter and the sort inside the search, the 12 terms of the plan stay under 20 ms at p99 in every variant: 6.7 ms the worst in the enhanced mode, 13.2 ms in the compatibility mode (alone, the code of section 6), 9.7 ms with the boosts and 14.1 ms with fuzzy matching. The limits that stay:

- A query with no `sort_on` and no `b_size` gets every hit in the order of the score: `Catalog.search` uses `sort_limit` only for a sort. For a word of the top band in the listing, about 380,000 hits, the order adds 16.7 ms in the compatibility mode, about 25 ms with the filter and the scores, and the enhanced mode takes 35 ms with a heap. The search view of Plone gives `b_size`.
- A phrase of two frequent words in the compatibility mode: 30 ms at p99 alone, 26 ms with the filter (section 6).
- Fuzzy matching over all 111 queries: 20.7 ms at p99 alone. It is off by default.
- The order of rows with equal keys of a sort follows the rule of the mirror; the prototype uses the rid descending.

## What did not run

- The record type in the commit path of the core: the staging, the commit batch, the log, the fold, pack, undo, verify, the dumps and `zoe convert`. ADR 0060 gives their rules; ZOE-293 and ZOE-294 build them.
- `ZoeTextIndex` in a Plone site, the profile step and the documents that the Plone suites create.
- The overlay of a `DemoStorage`, and a field for each language with plone.app.multilingual.
- A direct matcher for a small overlay, which would remove the fixed cost of a segment in memory (section 5).
- The filter and the sort inside the search with an overlay of section 5, and on an index of many segments with deletes. The code of section 8 handles segments and deletes, but the measures ran on one segment with no delete, and with no overlay.
- The order of every hit with no `b_size` through a plain sort in the enhanced mode: the measure used a heap.
- The change of the maps in memory at each reload of the reader, the wire, the parse of a query in Python, concurrent queries, the correction of the statistics at an old snapshot, the check of the files of the index at the open, and a rebuild with one indexing thread.
