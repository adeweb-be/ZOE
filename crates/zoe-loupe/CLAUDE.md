# zoe-loupe

The Rust side of Loupe, the desktop inspector and editor. Specification: "Loupe: desktop inspector and editor".

## Invariants

- Loupe never opens a `Data.fs`. It converts one.
- Folder mode opens a store with `Store::open_read_only`. It holds the store lock, so a running instance and Loupe never share a store. It never writes to the folder: the acceptance test compares the bytes of every file before and after browsing.
- Every decode goes through zoe-pickle, with the default node budget (`decode_record`) and one `Tree::resolver()` per record. A record that zoe-pickle refuses gives an `Object` with the refusal in `error`, not an error of the call and not a crash.
- The state of an object is JSON. The module documentation of `src/state.rs` gives the form of each node. A memo get shows the value that it stands for. A get inside its own value is a cycle marker. A dict with a key that starts with `$` is a list of pairs, so no key of the store looks like a tag. So is a dict with a key longer than 256 bytes, because each key is a segment of the pointers below it.
- A crafted record cannot exhaust the stack, the memory or the time of a call. The JSON has a depth limit and a value budget. Each text, bytes, opaque or large-int value has a cap, and the state has a text budget. Both count the bytes of the JSON, escapes and hex digits included. A value that is cut shows its start and its size in the record. The decoders and the int formatter read no more of a value than the cap needs.
- Each get of one value at one depth converts the value once. The other gets reuse its JSON and pay for it from the budgets. The reused JSON holds at most 2^18 values in all, so a chain of gets cannot multiply the memory. A JSON with a cycle marker for a value outside it is not reused, because the marker is wrong in another place.
- `references` lists the strong persistent references of the state, each with a JSON pointer into `state`. A memo get adds no reference: the place of the value has it. A weak reference shows in the state only. The references inside an opaque node are not known. A pointer has a cap, and the pointers of a state have a budget: past them, a reference points to a value that holds it, at most the root. No reference is left out.
- Ids cross the IPC boundary as ZODB-form strings, as `ZODB.utils.oid_repr` writes them: `0x00`, `0x0100`.
- A path starts at `root["Application"]`. Each segment goes through the key of a BTree or a mapping, the `_tree` item of a folder, or an attribute that holds a reference. A BTree lookup reads the separating keys to pick one child when they have the type of the segment. A BTree with int keys, such as an `IOBTree`, holds only a segment that is an int.
- Every Loupe write is one ZODB transaction: user `loupe:<operating-system user>`, a description that the operator types, and extension data with the tool name and version.
- Writes need expert mode for the session.
- Opaque pickle nodes are not editable.
- A delete is refused while inbound references exist, unless the operator chooses cascade.
- Every commit shows the pickle diff first.
- Credentials live in the operating-system keychain, never in a configuration file.
