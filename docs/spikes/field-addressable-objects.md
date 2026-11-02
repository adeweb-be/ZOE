# Finding: field-addressable objects, before the design of phase 6

Card ZOE-281; requirement N19, the triage row "Field-addressable objects", the roadmap row of phase 6 ("Needs: … the field-store spike verdict"), the open question of `docs/spec/working-methods.md` on the weighting of the three criteria, F17 and F19, and the triage row "SQL façade". The prototype and the scripts are on the branch `spike/ZOE-281` at [`0cc51329`](https://github.com/adeweb-be/ZOE/tree/0cc513298f38180fd1007fab958d690b8c3e8d99), not for merge.

This finding gives the numbers of each shape by criterion, with no weighting. The weighting is the human's (the open question of working-methods), and so is the verdict of the triage row. The verdicts below are proposals.

**The numbers, in short.**

- **The inventory.** Of the 56 indexes of the catalog of `xlarge`, 42 index a stored field of the object (17 of the 31 stock indexes, and the 25 `bench_*` indexes), 5 compute a value from the fields of the object, and 9 read other objects: the parents, the type information, the workflow, the relations, `portal_transforms`. Of the 53 metadata columns, 40 hold a stored field (19 of the 32 stock columns, and the 21 `bench_*` columns), 9 a value that the catalog computes from the object, and 4 a value that reads other objects. For the stock types, the merge policy of `zoe.plone` names `modification_date` (`max`) and the coupled group `effective`, `effective_date`, `expiration_date`, `expires` (an Event also couples `start`, `end`, `whole_day`, `open_end` and `sync_uid`); every other field is `strict`.
- **Conflicts.** In the 8 steps, 390 `ConflictError`s reached Zope, all on the trees of the version storage of CMFEditions (ZopeVersionControl); none on the catalog (queued indexing) and none on a content object. Behind them, the vote that failed hid 355 conflicts on a Document and 3 on a Folder. Tier 2 merges the 3 Folder conflicts and would merge 247 of the Document conflicts (`contention-fields`: title against description); it refuses 108 (`contention-same`: two titles). In each Document conflict both sides changed `version_id`. (A) removes no conflict beyond tier 2, by construction; (B) removes none: with the declared fields both sides write a pickle and tier 2 decides, and with every field a slot (B) merges what tier 2 merges and refuses what it refuses. Each of these votes also failed on the version storage, so no shape and no tier 2 mode changes a `ConflictError` of these runs.
- **Write amplification.** A content record is 2.0 to 4.0 % of the record bytes of an operation. (B) would not write 0.9 to 1.2 KB per operation (0.45 to 1.75 % of the record bytes), and with the 13 declared fields only in `create-spread`, where the update of the folder changes `modification_date` alone: every edit changes `text` or `version_id`, which no slot holds. (B) writes the same number of records, and redb rewrites about 2 pages for each record whatever its size, so the bytes at the system call stay the same. (A) adds 0.73 to 1.17 KB of slots per operation (0.5 to 1.1 %); about 850 bytes per content record on `xlarge` (an estimate).
- **Throughput.** Extracting the 13 declared fields of a Document at a commit costs 29.3 µs from its record with zoe-pickle (in the server or the client), 7.8 µs from the object in Python on 3.13 and 8.7 µs on 3.14t: 0.003 to 0.5 % of the Zope CPU of an operation. One value from slots costs 15 to 51 ns; the same value in a bucket of the `_unindex` of its index 145 to 415 ns (5.7 to 12.9 µs a bucket), and 29 µs through a decode of the content record. A load of shape (B) costs 1.6 µs more than a decode and pickle of the record in Rust (37.1 against 35.5 µs), and 26 to 27 µs more in Python. The decode of the records that the indexer writes, as the mirror will decode them, costs 2.1 to 3.7 ms per operation.
- **Ergonomics.** (A) changes no Plone code and nothing in the contract of ZODB; it needs a declaration table, a slot codec, and either a wire extension (the client writes the slots) or a derivation in the server. (B) breaks the rule that a store writes a whole pickle: the load, `loadSerial`, `history`, undo, the iterator, `zodbconvert`, `zoe convert`, the dumps and pack must compose a pickle, and pack must write records, which it never does today. (C) changes nothing.
- **The mirror.** Under every shape the mirror takes its values from the buckets of the catalog, as the specification says: 14 of the 31 stock indexes are not a stored field, the catalog lags the object by the delay of queued indexing (p50 0.11 to 0.43 s, p95 0.22 to 0.79 s in these runs), an add-on can register its own indexer for any name, and the map from a rid to an object is in the catalog. G1 asks for identical results.

**Proposed verdicts.** (C) go: phase 6 as the specification gives it. (A) later: its readers (Loupe, `zoe export --parquet` of phase 7, the SQL façade) are not in phase 6, and the mirror cannot use it; a later (A) can reuse the typed value codec of the delta record type. (B) no-go: no conflict beyond tier 2, no fewer records, and a change of the contract of ZODB in every reader of records. What each verdict changes in ZOE-283 to ZOE-303 is in "Verdicts".

## Question

Should the store keep chosen fields of an object addressable, in the typed-field slots of its record frame, and in which shape? What does each shape give on the three criteria of the triage (conflicts and write amplification, throughput, ergonomics), with numbers, and what does it change in the design of phase 6?

The shapes of the card:

- **(A) Derived slots.** Beside the pickle of an object of a declared class, the record holds the values of the declared fields in its slots. The pickle stays the authority. The store, the mirror, an export and Loupe read a field without the pickle.
- **(B) Field writes.** A slot is the authority of its field. A save that changes only slotted fields writes slots, not a new pickle, and a load makes the state from the pickle and the slots.
- **(C) No slots.** Phase 6 as the specification gives it.

This finding adds no shape. It measures one variant of (A): the server derives the slots from the pickle with zoe-pickle, in place of the client (see "Ergonomics").

## Method

**The runs.** B5 on the `small` fixture (`small-ba01ff255313`), stack `zoe-native-qi-kv`, scenarios `edit`, `create-spread`, `contention-fields` and `contention-same`, one repetition, steps of 4 and 16 users of 60 s (`--steps 4:60,16:60`), the default warmup (60 s, 8 users) and trim (5 s), `--keep-store` and `--profile zoe.plone:default`. The profile writes the merge policy of the site, as an owner does: the entries of the stock Dexterity classes in shadow mode, the two container classes on with `split_merges` and `folder_orders`, and the classes of `zoe.plone`. So tier 2 runs on each conflict of a content class and writes what it would do to the resolution log, and the conflict goes on as without it. `ZOE_BINARY` named a script that adds `--resolution-log` to `zoe serve`. Every run passed its verification.

**The conflicts.** The server writes one line of the resolution log for each stale written oid of a class of a tier whose mode is not off, every line of one vote at one time. `bench/fieldspike/votes.py` groups the lines by vote and names the owner of each oid: the first object that is not of BTrees on a path of references to it, from the current records of the kept store. For each line of a class that is not of BTrees, it takes the read record and the committed record that the line names. `fieldspike fields` (the prototype, below) decodes the two records into one heap of zoe-pickle and gives each field the canonical id of its value (`Canon`); the fields that the committed side changed are those whose id differs. The fields that the new side changed are the `keys` of the line of tier 2, which lists them (`merge/fields.rs`, `note`). `analyze_votes.py` counts the `ConflictError`s of the probe records in each step by the owner of their oid, and, for each line of a content class, the fields of both sides, the outcome of tier 2, and the outcome of (B): when one side changes declared fields only, (B) merges disjoint field sets, where `modification_date` merges with `max` as tier 2 does; when both sides change a field that is not declared, both write a pickle and tier 2 decides.

**Write amplification.** `bench/fieldspike/dump.py` reads each transaction of the two step windows of each kept store (the trim left out) and the class of each record, and, for each record of a content class (`plone.app.contenttypes.content.*` and the site), the record before it (`loadBefore`). `fieldspike fields` gives the fields that changed, the bytes of their pickles, and the bytes of the slots of the declared fields. `analyze.py` divides by the successful operations of each step. The bytes of a changed field are the bytes of its value pickled alone, plus its key (BINUNICODE and BINPUT). What (B) would not write is the bytes of an update of a content record minus the slots of its changed fields (7 bytes and the value each): with the declared fields, only for an update that changes declared fields only; with every field a slot, for every update. A new object writes its whole pickle in every shape.

**The declared fields.** From the inventory: the 13 stored fields of a content object that an index or a column of the stock catalog reads as they are, or through an accessor that returns them: `title`, `description`, `subject`, `creators`, `creation_date`, `modification_date`, `effective_date`, `expiration_date`, `id`, `portal_type`, `_plone.uuid`, `exclude_from_nav`, `cmf_uid`. `xlarge` adds its 25 `bench_*` attributes.

**The slots of the prototype.** A `u16` count, then for each field its id (`u16`, its place in the declared list), its type (`u8`), the length of its value (`u32`) and the value: none, bool, int (`i64`), float, str (UTF-8), bytes, `DateTime` (`_micros` as `i64`, the naive flag, the zone name), a tuple of str, or the pickle of any other value. It is a codec for the measure, not a proposal of format.

**The prototype.** `crates/zoe-pickle/examples/fieldspike.rs` on the branch:

- `fields`, above;
- `bench`: the extraction of the declared fields of a record into slots (`decode_record_with`, a load into a `Heap::with_instances`, the encode of each declared value), the read of one value from slots, the decode of each bucket of each tree of the catalog (`Heap::with_values`, `btrees::bucket_state`), and the record of a load of shape (B): the pickle decoded, its declared fields replaced by the values of its slots, and pickled again with `python::Pickler`. For each of the 572 records, the record that (B) makes from the record and its own slots is the record of a round trip through the heap, byte for byte;
- `decode`: the decode of each record that the batches of the indexer wrote in the windows of a run, into a `Heap::with_values`, as the mirror would decode the committed buckets.

`bench/fieldspike/pybench.py` times in Python the extraction from the object (`getattr` of each declared field and the encode into the same slots), the extraction from the state (`__getstate__` and the lookup of each field), and a load of shape (B): the unpickle of the record, the decode of its slots, an update of the state and `__setstate__`, against the unpickle and `__setstate__` alone. It makes the objects from the records with stand-in classes for Plone and the real `DateTime`. `bench/fieldspike/sample.py` (in Zope) wrote the 572 current records of the Documents and News Items of a kept store and every bucket of the trees of its catalog.

**The inventory.** `bench/fieldspike/inventory.py` ran in Zope on a read-only copy of the kept store of `edit`. For each index and column, and for a Document, a News Item, a Folder, a File and a Collection of the site, it records where the catalog takes the value: an indexer adapter (`IIndexer`) and its function, a method of the class, a stored attribute, or an attribute that the object acquires or lacks, and which stored attributes of the object hold an equal value. It also records the fields of the schemas of each type and the merge policy. The catalog of `small` is the stock catalog of Plone 6.1 (31 indexes, 32 columns); `xlarge` adds 25 indexes and 21 columns on the attributes `bench_*` that its build sets (`corpus.catalog_indexes`), so their class comes from the code of the build.

**Machine and builds.** The workstation of `ghost-to-cache.md`: Intel Core Ultra 7 155H, 30 GB, Linux 7.0. `main` at `2c57a5cb`, `zoe serve` in release, rustc 1.99, the zoeclient Plone instance (Plone 6.1.5, CPython 3.13.15) built from this tree. The B5 processes ran pinned as B5 pins them (CPUs 0 to 3). Another job built a large fixture on the machine during the runs, so the rates of these runs do not compare with other runs; the counts and the bytes do not depend on them. The micro-benchmarks ran in one process pinned to CPU 8, a performance core, after the B5 runs, with no other heavy job; each number is the median of 51 rounds over the 572 records (5 rounds for the decode of the records of the indexer). CPython 3.13.15 and 3.14.7t with the forks of persistent (6.8+zoe.1) and zodbpickle (4.5+zoe.1) of `python/uv.lock`, and DateTime 6.0; the GIL stayed off on 3.14t. The kept stores and the raw results are not in the repository; the scripts on the branch give them again.

## The inventory (step 1)

S is a stored field of the object, read as it is or through an accessor that returns it, or a constant when the object has none. C is a value that the catalog computes from the fields of the object alone. O is a value that reads other objects.

### Indexes

| Index | Kind | Class | Source on a Document |
| --- | --- | --- | --- |
| `Creator` | FieldIndex | C | the first item of `creators` |
| `Date` | DateIndex | C | `effective_date`, else `modification_date`, as an ISO str |
| `Description` | ZCTextIndex | S | `description`, str, through `Description()`; a ZCTextIndex keeps word ids |
| `SearchableText` | ZCTextIndex | O | `id`, `title`, `description`, `subject` and the text of `text` through `portal_transforms`, by an indexer |
| `Subject` | KeywordIndex | S | `subject`, tuple of str, through `Subject()` |
| `Title` | ZCTextIndex | S | `title`, str, through `Title()`; a ZCTextIndex keeps word ids |
| `Type` | FieldIndex | O | the title of the type information in `portal_types` |
| `UID` | UUIDIndex | S | `_plone.uuid`, str, through an indexer |
| `allowedRolesAndUsers` | KeywordIndex | O | the roles with View: the permission settings and local roles of the object and of each parent, and the groups |
| `created` | DateIndex | S | `creation_date`, DateTime, through `created()` |
| `effective` | DateIndex | S | `effective_date`, DateTime, through `effective()`, a constant when absent |
| `effectiveRange` | DateRangeIndex | C | `effective_date` and `expiration_date`, a range |
| `expires` | DateIndex | S | `expiration_date`, DateTime, through `expires()`, a constant when absent |
| `getId` | FieldIndex | S | `id`, str, through `getId()` |
| `getObjPositionInParent` | GopipIndex | O | the position of the object in the order of its folder |
| `getRawRelatedItems` | KeywordIndex | O | the intids of the relations of `relatedItems` |
| `id` | FieldIndex | S | `id`, str |
| `in_reply_to` | FieldIndex | S | `in_reply_to` of a comment of plone.app.discussion, int |
| `is_default_page` | BooleanIndex | O | the default page of the parent |
| `is_folderish` | BooleanIndex | C | the class of the object, by an indexer |
| `exclude_from_nav` | BooleanIndex | S | `exclude_from_nav`, bool, the default of the schema when absent |
| `modified` | DateIndex | S | `modification_date`, DateTime, through `modified()` |
| `object_provides` | KeywordIndex | O | the interfaces of the class, of `__provides__` and of the behaviors that the type information names |
| `path` | ExtendedPathIndex | O | the physical path: the ids of the parents |
| `portal_type` | FieldIndex | S | `portal_type`, str |
| `review_state` | FieldIndex | O | the last status of `workflow_history`, through the workflow of the type in `portal_workflow` |
| `sortable_title` | FieldIndex | C | `title`, normalized and padded by an indexer |
| `cmf_uid` | FieldIndex | S | `cmf_uid`, an object of CMFUid that gives an int |
| `start` | DateRecurringIndex | S | `start` of an Event, datetime; the recurrence of a recurring Event is computed |
| `end` | DateRecurringIndex | S | `end` of an Event, datetime |
| `sync_uid` | FieldIndex | S | `sync_uid` of an Event, str |

Stock: 17 S, 5 C, 9 O. The 25 indexes of `xlarge` on `bench_*` are S: 11 `FieldIndex` (6 on a str, 5 on an int), 6 `KeywordIndex` (a tuple of 1 to 3 str), 5 `DateIndex` (a `DateTime` in UTC) and 3 `BooleanIndex`. So on `xlarge`: 42 S, 5 C, 9 O.

### Metadata columns

| Column | Class | Source |
| --- | --- | --- |
| `CreationDate` | C | `creation_date` as an ISO str |
| `Creator` | C | the first item of `creators` |
| `Date` | C | `effective_date`, else `modification_date` |
| `Description` | S | `description` |
| `EffectiveDate` | C | `effective_date` as an ISO str, or 'None' |
| `ExpirationDate` | C | `expiration_date` as an ISO str, or 'None' |
| `ModificationDate` | C | `modification_date` as an ISO str |
| `Subject` | S | `subject` |
| `Title` | S | `title` |
| `Type` | O | the title of the type information |
| `UID` | S | `_plone.uuid` |
| `created` | S | `creation_date` |
| `effective` | S | `effective_date` |
| `end` | S | `end` of an Event |
| `exclude_from_nav` | S | `exclude_from_nav` |
| `expires` | S | `expiration_date` |
| `getIcon` | O | whether the object has a lead image |
| `getId` | S | `id` |
| `getObjSize` | C | the size of `text` or `file`, as text |
| `getRemoteUrl` | S | `remoteUrl` of a Link |
| `id` | S | `id` |
| `image_scales` | O | the scales of the image fields, kept in annotations |
| `is_folderish` | C | the class |
| `listCreators` | S | `creators` |
| `location` | S | `location` of an Event |
| `modified` | S | `modification_date` |
| `portal_type` | S | `portal_type` |
| `review_state` | O | the workflow status |
| `start` | S | `start` of an Event |
| `mime_type` | C | the MIME type of `text` or `file` |
| `cmf_uid` | S | `cmf_uid` |
| `sync_uid` | S | `sync_uid` of an Event |

Stock: 19 S, 9 C, 4 O. The 21 columns of `xlarge` on `bench_*` are S. So on `xlarge`: 40 S, 9 C, 4 O.

### The fields of the stock types

The state of a Document of the fixture has 18 keys: `title`, `description`, `subject`, `creators`, `creation_date`, `modification_date`, `id`, `portal_type`, `_plone.uuid`, `cmf_uid`, `text`, `workflow_history`, `version_id`, `location_id`, `__provides__` and the permission settings `_View_Permission`, `_Access_contents_information_Permission` and `_Modify_portal_content_Permission`. The first edit through the form adds `language`, `rights`, `relatedItems` and `table_of_contents`. A field of a behavior that keeps its default is absent from the state: `effective_date`, `expiration_date` and `exclude_from_nav` on most objects of the fixture, which the accessors and the schema give. The schema field `subjects` is stored as `subject`, and `effective` and `expires` as `effective_date` and `expiration_date`. The raw text of `text` is a `RawValueHolder` of its own, so the Document holds a reference to it, not the text. A Folder keeps its children in the trees `_tree` and its order in an annotation, not in its own state.

The merge policy that the profile `zoe.plone:default` writes names these fields for the stock types of Plone 6.1:

| Class | tier 2 | Fields with a strategy | Coupled groups |
| --- | --- | --- | --- |
| `plone.app.contenttypes.content.Document`, `NewsItem`, `File`, `Image`, `Link`, `Collection` | shadow | `modification_date`: `max` | `effective`, `effective_date`, `expiration_date`, `expires` |
| `plone.app.contenttypes.content.Event` | shadow | `modification_date`: `max` | the group above; `end`, `open_end`, `start`, `sync_uid`, `whole_day` |
| `plone.app.contenttypes.content.Folder`, `Products.CMFPlone.Portal.PloneSite` | on | `modification_date`: `max` | the first group |
| `zoe.plone.history.WorkflowHistory` | on | every workflow: `append`, option `state` `review_state` | |
| `zoe.plone.properties.PrincipalProperties` | on | `login_time`, `last_login_time`: `max` | |

Every other field is `strict`, the default of each entry. So the policy already addresses each field of a stock type for the merge: it names the class, and tier 2 decodes the three states and compares them field by field.

## Conflicts (step 2)

`ConflictError`s that reached Zope in each step, by the owner of their oid, and the conflicts of content classes that the same votes hid. "Version storage" is the trees of ZopeVersionControl under `portal_historiesstorage` of CMFEditions: the `IIBTree` and `IOBTree` of a `BranchInfo`, and the `OOBTree` of a `ZopeVersionHistory` with its buckets.

| Scenario | Users | ok ops | `ConflictError`s | On | Content conflicts behind them | Tier 2 | (A) beyond tier 2 | (B) beyond tier 2 |
| --- | --- | ---: | ---: | --- | --- | --- | ---: | ---: |
| `edit` | 4 | 222 | 0 | | 0 | | 0 | 0 |
| `edit` | 16 | 228 | 0 | | 0 | | 0 | 0 |
| `create-spread` | 4 | 219 | 0 | | 0 | | 0 | 0 |
| `create-spread` | 16 | 220 | 2 | version storage: 2 `OOBucket` | 3 Folder, and 3 order lists of plone.folder | Folder: merged (on); order lists: merged by the order merge | 0 | 0 |
| `contention-fields` | 4 | 1,263 | 116 | version storage: 103 `IIBTree`, 9 `OOBucket`, 3 `IOBTree`, 1 `IOBucket` | 106 Document | 106 merged (shadow) | 0 | 0 |
| `contention-fields` | 16 | 1,746 | 154 | version storage: 116 `IIBTree`, 19 `IOBTree`, 9 `OOBucket`, 6 `OOBTree`, 4 `IOBucket` | 141 Document | 141 merged (shadow) | 0 | 0 |
| `contention-same` | 4 | 1,746 | 0 | | 0 | | 0 | 0 |
| `contention-same` | 16 | 2,568 | 118 | version storage: 108 `IIBTree`, 10 `OOBucket` | 108 Document | 108 refused on `title` (shadow) | 0 | 0 |

The fields of the two sides of each content conflict, from the diff of the read and committed records and the `keys` of the line of tier 2:

| Scenario | Conflicts | One side changed | The other side changed | Disjoint |
| --- | ---: | --- | --- | --- |
| `contention-fields` | 140 | `description`, `modification_date`, `version_id` | `title`, `modification_date`, `version_id` | yes: `modification_date` takes `max`, and both sides set `version_id` to the same next number, which tier 2 takes as an equal change |
| `contention-fields` | 107 | `title`, `modification_date`, `version_id` | `description`, `modification_date`, `version_id` | yes, as above |
| `contention-same` | 108 | `title`, `modification_date`, `version_id` | `title`, `modification_date`, `version_id` | no: two different titles |
| `create-spread` | 3 | Folder: `modification_date` | `modification_date` | yes |

- **Where the conflicts are.** With queued indexing, the saves write no object of the catalog, and no `ConflictError` of these runs is on the catalog. Two saves of one Document both add a version to its history in CMFEditions, and both write the same trees of the version storage, which tier 1 refuses. Item 16 of `docs/findings/plone-end-to-end.md` counted 16 to 37 conflicts on the Document on stacks without queued indexing; here the Document conflicts are 247 and 108, behind the version storage.
- **What tier 2 does.** It would merge every disjoint Document conflict (247 of 247) and refuses the 108 conflicts of two titles, a true conflict. Each of the 355 votes also failed on the version storage, so switching the Document entry on would remove no `ConflictError` of these runs: the next class to address is the version storage, which no shape touches.
- **(A)** keeps the pickle as the authority: the vote detects and merges a conflict on the pickle as it does today. It removes no conflict beyond tier 2, by construction.
- **(B)** with the declared fields: in each Document conflict both sides changed `version_id`, which no index reads and no slot holds, so both sides write a pickle and tier 2 decides, as today. With every field a slot, both sides write slots only, and (B) needs the same rules as tier 2 (an equal change, `max` for `modification_date`): it merges the 247 that tier 2 merges and refuses the 108 that it refuses. In `create-spread`, the Folder conflicts change `modification_date` only, which tier 2 already merges. So (B) removes no conflict beyond tier 2 in these runs.

## Write amplification (step 3)

Per successful operation of each step. "Records" is the record bytes that the transactions of the window wrote, the batches of the indexer and the saves; "content" is the records of the content classes; "changed" is the bytes of the fields that changed in an update of a content record (value and key). "(B) decl." is what (B) would not write with the 13 declared fields, "(B) all" with every field a slot; "(A)" is the slots that (A) would add.

| Scenario | Users | ok ops | Records KB/op | indexer | saves | Records/op | Content records/op | Content KB/op | Changed B/update | (B) decl. B/op | (B) all B/op | (B) all % | (A) B/op | (A) % |
| --- | --- | ---: | ---: | ---: | ---: | ---: | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `edit` | 4 | 222 | 148.6 | 134.2 | 14.4 | 107 | 2.04 (1.02 new) | 2.93 | 532 | 0 | 990 | 0.67 | 760 | 0.51 |
| `edit` | 16 | 228 | 145.0 | 131.4 | 13.6 | 111 | 1.97 (0.99 new) | 2.87 | 535 | 0 | 963 | 0.66 | 740 | 0.51 |
| `create-spread` | 4 | 219 | 216.0 | 185.5 | 30.5 | 218 | 3.03 (2.02 new) | 4.68 | 85 | 965 | 965 | 0.45 | 1,172 | 0.54 |
| `create-spread` | 16 | 220 | 203.8 | 173.6 | 30.2 | 210 | 2.88 (1.92 new) | 4.45 | 81 | 917 | 917 | 0.45 | 1,115 | 0.55 |
| `contention-fields` | 4 | 1,263 | 117.0 | 85.1 | 31.9 | 71 | 1.99 (1.00 new) | 2.60 | 187 | 0 | 1,146 | 0.98 | 729 | 0.62 |
| `contention-fields` | 16 | 1,746 | 98.2 | 66.8 | 31.4 | 59 | 1.98 (0.99 new) | 2.59 | 184 | 0 | 1,143 | 1.16 | 728 | 0.74 |
| `contention-same` | 4 | 1,746 | 89.9 | 58.3 | 31.6 | 61 | 1.99 (0.99 new) | 2.63 | 183 | 0 | 1,163 | 1.29 | 764 | 0.85 |
| `contention-same` | 16 | 2,568 | 66.3 | 32.8 | 33.6 | 47 | 1.98 (0.99 new) | 2.62 | 183 | 0 | 1,158 | 1.75 | 758 | 1.14 |

- **The content records are few.** An edit writes 2 content records: the Document, and a new copy of it that CMFEditions keeps as the version of the save. A creation writes 3: the new object, its version copy and its folder. The other records are the batches of the indexer (49 to 90 % of the record bytes), the version storage of CMFEditions, the `RawValueHolder` of the text, the intids and the members.
- **What changes in an update.** In `edit`, every update changes `title`, `description`, `text`, `modification_date` and `version_id`, and the first edit of a Document adds `language`, `rights`, `relatedItems` and `table_of_contents`. In `contention-fields` and `contention-same`, every update changes `modification_date` and `version_id`, and `title` or `description`. In `create-spread`, the update of the folder changes `modification_date` only. `text` is read only by `SearchableText`, through `portal_transforms` (class O), and `version_id` by no index. So (B) with the declared fields applies to the update of the folder in a creation and to nothing else.
- **What (B) would save.** At most the bytes of the one updated content record of an operation, less its changed slots: 0.9 to 1.2 KB per operation, 0.45 to 1.75 % of the record bytes. A new object writes its whole pickle in every shape, and so does the version copy of CMFEditions.
- **The records and the pages.** (B) writes a record for each slot write, as for a pickle: a row `(oid, tid)` in `records` and a key in `txn_oids`. redb rewrites about 2 pages of 4 KB for each record that a commit changes, whatever its size (`server-write-volume.md`: 1.2 to 2.1 pages per record in B5, for records of about 1.1 KB). A content record of 1.3 to 1.5 KB fits in one leaf page, as a slot record of 30 to 300 bytes does, so the pages per operation, and the bytes that `zoe serve` gives to its write calls, stay the same. The records per operation stay 47 to 218.
- **What (A) would add.** 0.73 to 1.17 KB of slots per operation, 0.5 to 1.1 % of the record bytes: 360 to 390 bytes for each content record, the version copies included. On `xlarge`, the 25 `bench_*` fields add about 480 bytes per record (6 str and 5 int `FieldIndex` values of 15 bytes, 6 tuples of about 33 bytes, 5 `DateTime`s of 19, 3 bools of 8; an estimate from the generator of the build, not a measure), so about 850 bytes of slots per content record.

## Throughput (step 4)

A Document of the sample has 1,375 bytes on average and 372 bytes of slots for its declared fields (it holds 10 of the 13; `effective_date`, `expiration_date` and `exclude_from_nav` are absent).

| Measure | Rust (zoe-pickle) | Python 3.13 | Python 3.14t |
| --- | --- | --- | --- |
| Extraction of the declared fields at a commit, per Document | from the record: 29.3 µs (decode 14.2, load into a heap 13.8, encode 1.3) | from the object: 7.8 µs, of which the encode 5.3; from `__getstate__`: 7.2 µs | 8.7 µs, encode 5.9; 8.4 µs |
| Read of one value from slots | 15 to 17 ns for a `DateTime`, 16 to 37 ns for a str, 34 to 51 ns for a tuple of str | the decode of all the slots of a record: 18.1 µs | 19.9 µs |
| Decode of a bucket of the index of that value | `_unindex` of a FieldIndex, DateIndex or BooleanIndex: 5.7 to 12.9 µs a bucket of 31 to 62 entries, 145 to 415 ns a value; of a KeywordIndex: 23 to 29 µs, 750 to 900 ns a value; `Type` (an i18n `Message` per entry): 51 µs, 1.7 µs a value; an `_index` bucket: 16 to 32 µs, 1.0 to 1.4 µs an entry; a bucket of `data`: 312 µs for 31 rows, 10 µs a row | | |
| Load of shape (B) | the record from the pickle and its slots: 37.1 µs, against 35.5 µs for the decode and pickle of the record alone (+1.6 µs) | unpickle, slots, `__setstate__`: 58.9 µs, against 33.1 µs (+25.8 µs) | 62.6 against 35.8 µs (+26.8 µs) |
| Decode of the records that the indexer wrote, into a heap with `DateTime`, `Message` and `Missing.Value` | `edit`: 40,576 records, 964 ms for 450 operations, 2.1 ms per operation (3.3 ms per batch); `create-spread`: 78,992 records, 1,642 ms for 439 operations, 3.7 ms per operation (6.0 ms per batch); 21 to 24 µs a record | | |

- **At a commit.** An operation writes 2 to 3 records of a declared class: 16 to 26 µs in Python, 60 to 90 µs with zoe-pickle, against a Zope CPU of 420 to 464 ms per operation in the form scenarios and 19 to 22 ms in the REST scenarios of these runs: 0.003 to 0.5 %. The generic decoder of zoe-pickle builds a tree that keeps every opcode for the round trip; the reader of ZOE-276 decodes a whole state of 1 KB in 0.39 µs (`ghost-to-cache.md`), so an extractor of that kind would cost about 1 µs per record, in the client or in the server.
- **The read of one value.** A slot read is about 3 to 30 times cheaper than the share of one value in the decode of its bucket, and 500 to 2,000 times cheaper than a decode of the content record. Both reads first load their record from the store. The gain counts where a reader scans the fields of many objects: Loupe, an export, the SQL façade. The mirror decodes a bucket when a commit changes it, not for each query.
- **The load of (B).** In Rust, the composition costs little beyond the decode and pickle of the record (+1.6 µs), but a load that composes is a decode and a pickle, 35 to 37 µs with zoe-pickle, where a load today gives the stored bytes as they are. In Python, the decode of the slots costs 18 to 20 µs, 11 µs of which are the two `DateTime.__setstate__` (5.6 µs each), as the unpickle pays for each `DateTime` that it reads.
- **A by-product for ZOE-283.** The mirror decodes the committed buckets of each index at its maintenance point: 2.1 to 3.7 ms per operation of B5 with the generic decoder of zoe-pickle on one core, 3.3 to 6.0 ms per batch of the indexer (item 9 of the plan of phase 6). A decoder for buckets alone would cost less.

## Ergonomics (step 5)

| | (A) derived slots | (B) field writes | (C) no slots |
| --- | --- | --- | --- |
| A Plone site and its add-ons | No code. A declaration table, derived from the Dexterity schemas and the indexers of the catalog as the merge policy is derived (an import step of `zoe.plone`), names the fields of each class; a class without an entry writes no slots | The same declarations, and each save diffs the state of each object against its last record to tell a slot write from a pickle write. No Plone code changes, but every reader of a raw record outside ZOE (a stock ZEO client, `zodbconvert`, ZODB's tools) reads a pickle whose slotted fields are stale, unless the server composes each record that it gives out | Nothing |
| Who writes the slots | The client at `store`, with the declarations and a wire extension: a stock ZEO client writes no slots, so a mixed cluster has records without them and every reader falls back to the pickle. Or the server, which derives them from the pickle with zoe-pickle at the vote: no wire change, 29 µs per declared record on the commit path with the generic decoder | The client, through a new call of the protocol for a field write: `store` carries a whole pickle | — |
| A store writes a whole pickle | Kept | Broken: a field write is a record without a pickle | Kept |
| `_p_resolveConflict`, tier 1, tier 2 | Unchanged. A merged record gets its slots derived again from the merged state, which the core holds in its heap | Tier 2 must merge slot records and pickle records; `_p_resolveConflict` and tier 1 need composed states | Unchanged |
| Undo | The undo record copies the slots of the record that it restores: they derive from its pickle | An undo of a pickle record after later field writes must compose, and an undo of a field write writes the old slot values: two paths | Unchanged |
| `history`, `loadSerial`, `loadBefore` | Unchanged: they give the pickle | Each gives a composed pickle: the last pickle record at or before the tid, with every later field write up to the tid. A load walks back to the last pickle record, or each field write repeats every declared field so that a load reads two records | Unchanged |
| The iterator, `zodbconvert`, `zoe convert --to-data-fs`, the dumps (ADR 0030) | Unchanged: they carry the pickle and drop the slots, which a convert into ZOE or a restore derives again | Each record becomes a composed pickle: the iterator, `zodbconvert`, `zoe convert` and `zoe backup` compose, and a restore writes whole pickles or needs a dump entry of its own | Unchanged |
| Pack | Unchanged: it removes rows, and the slots go with their record | Pack cannot remove a pickle record that a later field write needs: it must write a composed record in its place, and pack writes no record today (red zone) | Unchanged |
| Verify, scrub, the write-ahead log | The checksum already covers the slots, and the log copies each frame byte for byte. Verify can also check that the slots are what the pickle gives | Verify and scrub check slot records against their declarations and the chain of each oid; the log is unchanged | Unchanged |
| Loupe, `zoe export --parquet` (phase 7), the SQL façade (Could) | Read a declared field without a decode of the pickle: 15 to 51 ns from the slots against 29 µs for a decode of the record; the other fields still need the pickle | As (A) for the slotted fields; they compose for the rest | Decode the pickle with zoe-pickle, as Loupe does today |
| The mirror (F17) | The buckets of the catalog (see "The mirror and the slots") | The same | The buckets, as specified |
| Extent of the format change | A writer of slots (in the client or the server), a slot codec, a declaration table with versions (a record keeps the slots of the version that wrote it), the derivation in convert and restore, a check in verify. The reader of each build already opens a frame with slots and leaves them opaque, so an older build reads a store with slots; `replace_txn` refuses a transaction row with slots, not a record | A new kind of record (a frame of slots without a pickle), composition in each reader of records, a pack that writes, a wire extension, a dump entry or composition in backup, and a migration of the stores with field writes before an older build can read them | None |

## The mirror and the slots

Under each shape, the mirror of F17 takes its values from the committed buckets of each index, as the specification says, for four reasons that the inventory and the runs give:

1. 14 of the 31 stock indexes, and 13 of the 32 stock columns, are not a stored field: `allowedRolesAndUsers`, `path`, `getObjPositionInParent`, `review_state`, `object_provides`, `SearchableText`, `sortable_title` and the other indexes of classes C and O. No slot of the object holds their value.
2. The catalog holds the values of the time of indexing. With queued indexing (ADR 0047) the indexer applies a save later: in these runs its delay was p50 0.11 to 0.43 s and p95 0.22 to 0.79 s. A partial reindex (`reindexObject(idxs=...)`), and code that changes a field without a reindex, leave indexes behind too. A mirror fed from slots would answer the new value while ZCatalog answers the old one, and G1 asks for identical rids, order and counts.
3. A site or an add-on can register an indexer under the name of any index for its types; then the value of an S index is not the field. The derivation of the declarations could leave those names out, but only for the indexers that it sees.
4. A query answers rids, and the map from a rid to an object is in the catalog (`paths`, `uids`), not in the object.

So slots can serve a reader that wants the fields of the objects as they are stored, such as the SQL façade or an export, but not the mirror.

## The numbers by criterion and by shape

No weighting. "op" is a successful operation of B5 on `small`, 4 and 16 users.

| Criterion | (A) derived slots | (B) field writes | (C) no slots |
| --- | --- | --- | --- |
| `ConflictError`s that reach Zope (390, all on the version storage of CMFEditions) | unchanged | unchanged | — |
| Conflicts on content objects beyond tier 2 (358 behind those votes) | 0, by construction | 0: 247 merge and 108 refuse in (B) as in tier 2; 3 Folder conflicts that tier 2 already merges | — |
| Record bytes per op | +0.73 to +1.17 KB (+0.5 to +1.1 %); about 850 bytes per content record on `xlarge` (estimate) | −0.92 to −0.97 KB with the declared fields, in `create-spread` only; −0.9 to −1.2 KB (−0.45 to −1.75 %) with every field a slot | — |
| Records per op, and so pages of redb per op | unchanged | unchanged | — |
| Extraction at a commit, per declared record | 29.3 µs with zoe-pickle; 7.8 µs (3.13) and 8.7 µs (3.14t) from the object in Python; about 1 µs with a dedicated decoder (estimate) | the same, and a diff of each state against its last record | — |
| Read of one field | 15 to 51 ns from the slots | the same | 145 to 415 ns a value in a bucket of `_unindex`; 29 µs through a decode of the record |
| Load | unchanged | +1.6 µs over a decode and pickle of 35.5 µs in Rust, where a load today copies bytes; +25.8 µs (3.13) and +26.8 µs (3.14t) in Python | — |
| Ergonomics | declarations and a slot codec; derived in the server: no wire change; no change of the contract of ZODB | the contract of ZODB changes in store, load, history, undo, the iterator, convert, the dumps and pack; a wire extension; pack writes records | nothing |

## Verdicts

Proposed, for the human to weigh and decide. The four points that the card names are the source of the values of the mirror, the order index, the security index and the record type of the text deltas.

**(C) go.** Phase 6 as the specification gives it. In the cards:

- **The source of the values of the mirror** (ZOE-283, ZOE-286 to ZOE-289): the committed buckets of each index and of `data`, decoded with zoe-pickle, as written. For ZOE-283: in these runs the indexer writes 29 to 183 records per operation (88 to 183 in `edit` and `create-spread`), and their decode with the generic decoder of zoe-pickle costs 2.1 to 3.7 ms per operation at the maintenance point (item 9 of the plan).
- **The order index** (ZOE-302): the position tree of the one container (Q11), as written. `getObjPositionInParent` is class O: the position lives in the folder, and a reorder writes no child, so no slot of a child can hold it, under any shape.
- **The security index** (ZOE-285, ZOE-301): as written. `allowedRolesAndUsers` is class O. Each object stores its own part (`__ac_local_roles__`, `__ac_local_roles_block__`, `_View_Permission`), and the inherited part reads the parents. ZOE-285 decodes the own part from the records that a commit writes, with zoe-pickle, or takes `allowedRolesAndUsers` from the buckets. A cold build from the objects of a site of a million objects is a million decodes, about 29 µs each with the generic decoder, against about 32,000 buckets of `allowedRolesAndUsers._unindex` at about 23 µs each.
- **The record type of the text deltas** (ZOE-284, ZOE-293, ZOE-294): as Q8 decided. `SearchableText` is class O, and `Title` and `Description` are text indexes over stored text that keep word ids; a slot does not replace a delta.
- No card and no ADR follow.

**(A) later.** Not in phase 6. It removes no conflict and no record, its readers (Loupe, `zoe export --parquet` of phase 7, the SQL façade) are not in phase 6, and the mirror cannot use it. The triage row "SQL façade" then waits for a consumer that asks for (A). When one does: an ADR (the declarations and their versions, the slot codec, who derives: the server from the pickle with no wire change, or the client), and cards for the writer, convert and restore, verify, and the reader of Loupe or of the export. In phase 6 it changes nothing in ZOE-283 to ZOE-303.

**(A) go**, if the human weighs the throughput of field reads above the other criteria: a format change, with a card and an ADR of its own outside the plan of phase 6, as the card says. It changes no card of phase 6: the mirror keeps the buckets, the order and security indexes read other objects, and the deltas stay as Q8 decided. The cards that follow: the ADR above, the declaration table and its import step in `zoe.plone`, the writer of slots in the server (or in the client with a wire extension), the derivation in `zoe convert` and `zoe restore`, the check in `zoe verify`, and the readers.

**(B) no-go.** It removes no conflict beyond tier 2 in these runs and no record, and writes 0.45 to 1.75 % fewer record bytes, with the same pages. Its cost is the contract of ZODB in every reader of records, a pack that writes records, and a wire extension. A go would need an ADR and cards for the field-write record, the composition in load, `history`, undo, the iterator, convert, backup and pack (red zone), the wire extension and the diff in the client; it would change no card of phase 6 either, for the reasons of (A).

**What the slots and the delta record type share** (Q8, not reopened). They share the frame and its plumbing, which exist: the header, the checksum, the entry of the write-ahead log, and the reading of a frame by pack, verify and scrub. They share no key space: the deltas are a key space of their own with a history per tid, and the slots live in the frames of `records`. The deltas need no slots. One part could be shared: a delta carries index values (str, int, `DateTime`, tuples of str, bool) as its old and new values, and a later slot set would encode the same kinds of values; a typed value codec defined in ADR 0060 is the codec that a later (A) would reuse.

## Limits

- One repetition of each scenario on `small`, as the card asks; another repetition can move the counts by a few.
- The bytes of a field are the bytes of its value pickled alone; inside the record, a memo get can make a repeated value shorter.
- The owner of an oid is the first object that is not of BTrees on one path of references to it, from the current records; an oid that two objects refer to takes one of them.
- One workstation, not the reference machine; each timing comes from one run of medians.
- The Python micro-benchmarks use stand-in classes for the classes of Plone and a subclass of the unpickler with Python hooks, so the absolute times of the loads are above those of a Plone instance; the differences between the two loads compare. `cmf_uid` is a stand-in object there, which the encode pickles.
- The slot codec, the declared fields and the field ids are those of the prototype, not a proposal.
