# Spike: byte-identical pickle round trip in zoe-pickle

Card ZOE-011, requirements F16 and F10.

**Verdict.** Yes. A decoder and an encoder in pure Rust, with no Python, reproduce all 80 records of a synthetic ZODB 6 corpus byte for byte. To do this, the tree must keep how each value was written as well as the value itself: the opcode of each int, byte string and memo index, the form of each tuple, the batch layout of each list and dict, and every memo put and get with its index. Nine nodes in six records are opaque: a custom `__reduce__`, a `datetime`, a plain inline object (NEWOBJ and BUILD), and a str that holds a lone surrogate. The build card can keep this design, with one change: the encoder, and the drop, clone and comparison of a node, recurse once per nesting level. A deeply nested value, which ZODB can write, overflows the stack and aborts the process, so the tree needs a nesting limit or iterative code. Before the decoder runs on a production copy, it must add the protocol-1 opcodes that Python 2 stores still hold (INT, LONG, OBJ and INST). It should also model inline objects: a Plone store probably holds many of them, and the production-copy run must show how many.

## Question

Can `zoe-pickle` decode a ZODB record (the class pickle, then the state pickle) into a generic tree and encode the tree back to the same bytes, without unpickling? Which constructs must stay opaque nodes?

## Method

1. Build a synthetic corpus. `crates/zoe-pickle/tests/corpus/generate.py` commits objects with ZODB 6.3, BTrees 6.5, persistent 6.8 and zodbpickle 4.5 to a FileStorage in a temporary directory. Then it reads every stored record back and writes its exact bytes to `<construct>-<n>.record`. The hash seed is fixed, so the order of set items is stable. CPython 3.12, 3.13 and 3.14 give the same 80 files.
2. Decode with the pickle stack machine, but build nodes instead of objects. Each stack entry keeps the offset where its bytes start. So the bytes of a construct that the tree does not model are one known span, and an opaque node keeps that span.
3. Encode each node with the opcode that it keeps. The encoder never picks an opcode itself. A value that its opcode cannot hold, for example a fourth item in a TUPLE3 tuple, is an error.
4. Run the acceptance test `tests/round_trip.rs`. Run `tests/corpus.rs`, which checks which records hold opaque nodes, reads class names, follows persistent ids, and resolves memo gets across the two pickles. Run unit tests on hand-written pickles for the opcodes that the corpus does not contain.
5. Count the opcodes with `pickletools`, independently of the decoder.

## Results

| Construct | Records | What the records hold |
| --- | --- | --- |
| persistent-reference | 4 | `(oid, class)` to an object of the same class; five references to four objects of four classes, one target twice; the oid alone (a class with `__getnewargs__`); a weak reference `['w', (oid,)]` |
| class-with-newargs | 2 | the class pickle `(class, ())` that `__getnewargs__` gives |
| holder | 1 | the target of the references |
| oobucket | 7 | a standalone bucket; the six buckets of a 100-item OOBTree, linked by `next` references |
| oobtree | 2 | a tree with one inline bucket; the tree of the six buckets |
| iitreeset | 2 | a set with one inline bucket; a set of 1,100 ints |
| iiset | 18 | the buckets of the big IITreeSet |
| length | 5 | `BTrees.Length.Length` of 0, 1, 42, 70,000 and -3 |
| persistent-mapping | 6 | small, empty, 1,001 items, the root of the store in two versions, a reference target |
| persistent-list | 5 | small, empty, 1,001 and 2,001 items, a reference target |
| zodbpickle-binary | 3 | `binary` of 7, 300 and 0 bytes |
| bytes | 1 | `bytes` of 3, 300 and 0 bytes |
| tuple | 4 | empty; one, two and three items; five items and nested tuples; one str twice and in a list |
| set | 3 | a set of ints, a frozenset of str, an empty set and a set of tuples |
| unicode | 4 | ASCII; two-, three- and four-byte UTF-8 characters; 300 non-ASCII characters; a lone surrogate |
| small-int | 1 | 0, 1, 255, 256, 65,535, 65,536 and 2³¹−1 |
| negative-int | 1 | −1, −129, −2³¹ and −2³¹−1 |
| large-int | 2 | 2³¹, 2⁶³−1, 2⁶⁴, 2⁷⁰ and −2¹⁰⁰; ±2²¹⁰⁰ |
| float | 2 | 0.0, −0.0, 1.5, −2.25, 1e300 and 5e-324; inf, −inf and NaN |
| none-and-bool | 1 | None, True and False, as values and in a list |
| shared-value | 1 | one list under two keys; a list that holds itself |
| custom-reduce | 3 | a factory function; a class, arguments and a state; four such objects in one list |
| inline-object | 1 | a plain object that pickles through copyreg |
| datetime | 1 | a `datetime.datetime` |
| **Total** | **80** | 40,417 bytes, from 32 to 15,964 bytes per record |

**Round trip: 80 of 80 records give the same bytes.** Tests: 3 acceptance tests, 5 corpus checks, 19 unit tests (13 decoder, 4 encoder, 2 tree), all green.

**The 34 opcodes in the corpus**, both pickles of every record:

| Role | Opcodes (count) |
| --- | --- |
| Frame | PROTO 3 (160), STOP (160) |
| Memo | BINPUT (1,091), LONG_BINPUT (749), BINGET (103) |
| Integers | BININT2 (4,166), BININT1 (904), BININT (110), LONG1 (6), LONG4 (2) |
| Other atoms | BINUNICODE (1,351), SHORT_BINBYTES (107), BINBYTES (2), BINFLOAT (10), NONE (4), NEWTRUE (2), NEWFALSE (2) |
| Tuples | TUPLE2 (141), TUPLE (30), TUPLE1 (17), EMPTY_TUPLE (4), TUPLE3 (2) |
| Lists and dicts | MARK (64), EMPTY_DICT (56), SETITEM (39), SETITEMS (13), EMPTY_LIST (22), APPENDS (21), APPEND (1) |
| References and calls | BINPERSID (105), GLOBAL (100), REDUCE (11), BUILD (4), NEWOBJ (1) |

No SHORT_BINSTRING, BINSTRING, EXT1, EXT2 or EXT4 is in the corpus. Unit tests cover them on hand-written pickles.

### Opaque nodes

| Record | Opaque nodes | First unmodeled opcode | Why |
| --- | --- | --- | --- |
| custom-reduce-1 to -3 | 6 | REDUCE | The callable is a function or a class that the tree does not know. Without an import, the decoder cannot tell what the call returns. A `__reduce__` that also gives a state adds a BUILD. The call, the state and the BUILD are one opaque node. |
| datetime-1 | 1 | REDUCE | `datetime.datetime` called with 10 bytes. The same rule applies: the decoder models REDUCE only for `builtins.set` and `builtins.frozenset`. |
| inline-object-1 | 1 | NEWOBJ | The copyreg path of a plain object: `cls.__new__(cls)`, then BUILD with its `__dict__`. |
| unicode-4 | 1 | BINUNICODE | Python writes a lone surrogate with `surrogatepass`. The bytes are not UTF-8, so a Rust `String` cannot hold them. |

The other 74 records hold no opaque node.

### What the corpus shows

1. **One memo for two pickles.** ZODB's `ObjectWriter` clears the memo, then writes the class pickle and the state pickle with one pickler. The state pickle continues the memo numbers of the class pickle and refers back to them. In 25 records, 28 BINGETs in the state name the GLOBAL of the class pickle. Each persistent reference to an object of the record's own class does this: the class in its persistent id is a get of memo index 0. A decoder that reads the two pickles as separate pickles fails on these records. `Tree::resolve` follows a get to its put in either pickle.
2. **Memo indices go past 255.** The corpus has 749 LONG_BINPUTs, from big buckets and lists. Each put and get keeps its width.
3. **The same value has two encodings.** zodbpickle's C pickler writes a list or dict of exactly one item with APPEND or SETITEM. It writes a longer one in batches of 1,000 with MARK and APPENDS or SETITEMS, and the last batch is MARK, item, APPENDS also when it holds only one item. The pure-Python pickler of the standard library writes that last batch as item, APPEND. So the tree keeps the layout of each batch.
4. **`zodbpickle.binary` writes the opcodes of `bytes` in protocol 3**: SHORT_BINBYTES or BINBYTES. The oid in each persistent id is a `binary`, so it is SHORT_BINBYTES too. ZODB 6 writes no Python 2 string. The tree has a `ByteString` node for SHORT_BINSTRING and BINSTRING, which only the unit tests exercise.
5. **Persistent ids come in three forms**: `(oid, class)`, where the class is a GLOBAL or a get; the oid alone, for a class with `__getnewargs__`; `['w', (oid,)]` for `persistent.wref.WeakRef`. The cross-database forms `['m', …]` and `['n', …]` need a multi-database and are not in the corpus.
6. **Class names come from the class pickle.** In this corpus, the class pickle is a GLOBAL, or `(GLOBAL, newargs)` for a class with `__getnewargs__`. `Tree::class_name` reads both and unpickles nothing. ZODB documents more class formats, which this corpus does not hold (see the risks below).
7. **A set is a REDUCE.** Protocol 3 has no set opcode: a set is `builtins.set` called with a list. The tree models it as a `Reduce` node.
8. **Opaque bytes hold memo indices.** In custom-reduce-3, the second token gets its factory from a put inside the first opaque node, and the second pair reuses the strings of the first pair. The round trip is exact, because the opaque bytes do not change. But an editor that renumbers the memo must keep every index that an opaque node puts or gets. `Tree::resolve` gives `None` for a get whose put is inside opaque bytes.

## The tree

A record decodes to a `Tree` with a class root and a state root. A `Node` is one of: `None`, `Bool`, `Int` (one variant per opcode; LONG1 and LONG4 keep their two's-complement bytes), `Float`, `Bytes`, `ByteString`, `Unicode`, `Tuple` (items and a mark flag), `List` and `Dict` (batches with a mark flag), `Global`, `Persistent` (the persistent id), `Reduce` (known constructors only), `Put` (memo index, width and value), `Get` (memo index and width), and `Opaque` (bytes and the first unmodeled opcode). A put wraps its value. The encoder writes the value, then the put, then the batches of a list or dict, which is the order that Python's pickler uses.

## Verdict for the build card

Keep the stack machine with byte offsets, puts and gets as nodes, the opcode form on every node, the batch layout, one memo across the two pickles, and opaque spans. Without tests, the crate has about 1,000 lines of Rust with their docs (decoder 430, encoder 220, tree 280) and no dependency other than `thiserror`.

The build card should add:

- **A nesting limit.** The encoder recurses once per nesting level (`encode/mod.rs`), and so do the drop, clone, debug and comparison of a `Node` (`tree.rs`). A stack overflow aborts the process: it is not a panic and not an error. The review reproduced it on a 2 MB thread stack, the default for a spawned Rust thread and for `cargo test`, with records that ZODB 6.3 writes: a `PersistentMapping` that holds an empty tuple wrapped in 800 one-item tuples (4,125 bytes) aborts `encode_record` in a debug build, and 4,000 levels abort a release build. CPython's pickler writes tuples 5,000 levels deep, and zodbpickle on 3.14 writes lists 50,000 levels deep. The tree must refuse a value past a fixed depth, or keep it opaque, or use iterative code for encode and drop.
- **Inline objects as modeled nodes.** NEWOBJ then BUILD is how every plain object with a `__dict__` pickles, and REDUCE of a class with simple arguments is how `datetime` pickles. A Plone store probably holds many of them, `DateTime` values first; the production-copy run must confirm how many. If they stay opaque, Loupe cannot edit them. The decoder cannot tell a class from a function without an import, so the model needs a list of known constructors: a fixed list, or the classes seen in the store.
- **A safety net.** After decode, encode the record and compare. If the bytes differ, keep the whole pickle as one opaque node and report it. Loupe's integrity screen can then list these records, and the editor refuses them. The safety net cannot catch a stack overflow, so it does not replace the nesting limit.
- **POP and POP_MARK.** Python writes them for a tuple that holds itself, and Python 3's `save_reduce` writes POP, then a memo get, for a reduce that its own arguments reach again, in every protocol. The spike decoder refuses them.
- **Smaller fixes from the review.** Copy each opaque span once, not again at each wrap, because nested wraps take quadratic time on crafted input. Resolve a memo get to the latest put before it, not the first put with the index. Refuse what Python refuses, such as an unknown PROTO value or a BINSTRING or LONG4 length of 2³¹ or more, which Python reads as a negative count. Set a budget on the node count, because a `Node` is 48 bytes and a run of one-byte opcodes gives about 48 times the input size.

## Risks for a production copy

- **Python 2 stores.** ZODB on Python 2 wrote protocol 1 before ZODB 5.3, protocol 2 in ZODB 5.3, and protocol 3 from ZODB 5.4 (ZODB's own changelog). A store keeps each record as it was written. Unless a migration tool rewrote every record, one store can hold records of protocols 1, 2 and 3. Protocol 1 has no PROTO, no TUPLE1 to TUPLE3, no NEWTRUE or NEWFALSE (booleans are INT with text), and no LONG1 (long ints are LONG with text). It uses OBJ for instances of old-style classes and REDUCE of `copy_reg._reconstructor` for new-style ones. The spike decoder supports a pickle without PROTO, but it refuses INT, LONG, OBJ and INST. Protocol 2 from Python 2 writes NEWOBJ for objects and LONG1 for every `long`, also a small one. The spike decoder reads both, and NEWOBJ is opaque.
- **Python 2 names and strings.** Python 2 pickles name `__builtin__` and `copy_reg`, not `builtins` and `copyreg`, so a Python 2 set is opaque under the current list of known constructors. A Python 2 `str` is SHORT_BINSTRING or BINSTRING, also in protocol 3. ZODB 6 loads an ASCII one as `str` and raises `UnicodeDecodeError` on a non-ASCII one, which is why such a store needs a migration before it runs on Python 3. The tree has no such limit: a `ByteString` keeps the bytes, so the round trip is exact, and Loupe can show a record that ZODB 6 cannot load.
- **`zodbpickle.binary`.** From ZODB 5.4 on Python 2, a `binary` and the oid of each persistent reference are SHORT_BINBYTES. Records from before ZODB 5.4 hold each oid as a Python 2 string, SHORT_BINSTRING. Both forms round-trip, but code that reads an oid from a persistent id must accept both.
- **Other class formats.** ZODB's `serialize.py` lists more class descriptions than this corpus holds. The current formats are `type`, `(type, newargs)`, `((module, name), None)` (format 3) and `((module, name), newargs)` (format 7). ZODB 6 still writes formats 3 and 7 for instances of a persistent class. The legacy formats are `((module, name), initargs)`, `(class, None)` and `(class, initargs)`. A persistent class can also be given as a persistent reference. `Tree::class_name` returns `None` for format 3, so the build card must read every format.
- **Extension codes.** EXT1, EXT2 and EXT4 name objects in the copyreg extension registry. ZODB itself registers none, so a store holds them only if a site registered codes. They stay opaque, which is safe because their meaning is outside the record.
- **Wrapped records.** A record that a storage wrapper wrote, such as a compressed record from `zc.zlibstorage`, is not a pickle until the wrapper is removed. The production copy must show whether the site uses a wrapper.
- **Scope of this corpus.** It holds no Plone content: no `DateTime`, no OFS or Acquisition classes, no `BTreeFolder2`, no catalog, no blobs. A synthetic corpus cannot show how often each opaque construct appears in a real store. The F10 run on a production copy (on the adopting site's private runner) must report, per class, how many records round-trip, how many hold opaque nodes, and how many the decoder refuses.
