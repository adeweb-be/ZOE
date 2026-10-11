# zoe-pickle

Decode and encode ZODB records (the class pickle and the state pickle) as a generic tree. Specification: "Loupe", section "Pickle layer".

## Invariants

- Decode then encode gives the same bytes, for every record of the corpus and for every record that the decoder accepts.
- A construct that the tree does not model becomes an opaque node that re-emits its original bytes. An edit inside an opaque node is refused.
- Class names come from reading the class pickle, never from unpickling state.
- The encoder writes the opcode form that each node keeps, also the protocol-0 and protocol-1 forms of Python 2 stores. It never picks another form. Nodes for new values use protocol 3 with ZODB's `persistent_id` convention.
- Decode, encode, drop, clone, comparison and debug never call themselves once per nesting level. They use explicit stacks in the heap. Any new operation on a tree does the same.
- The decoder copies the bytes of each opaque span once: it records spans while it runs and copies the spans that remain at the end.
- A record has a node budget. `decode_record` uses `NODE_BUDGET` (2²⁰ nodes), for online readers such as Loupe. `decode_record_with` takes the budget as a parameter, in `Limits`. The decoder refuses a record that needs more nodes with `Error::Budget`, which is not `Error::Decode`: the bytes can be valid.
- Offline tools that must read every record, such as pack and the converters, give a budget equal to the length of the record. The node count never gets above that length, so this budget refuses nothing that the input can hold. For these tools, any refusal is a hard error. Pack never reads a refused record as a record with no references.
- At a budget equal to the record length, the memory of the tree grows with the record: up to about 150 bytes per byte of record. Pack and the converters read one record at a time for this reason.
- `persistent_ids` is equivalent to ZODB's `referencesf`: it gives the persistent id of every BINPERSID and PERSID of both pickles, in pickle order, with each repeat, the parts inside opaque nodes included. It decodes the record first with a budget equal to its length. A refusal of the decoder, a persistent id that is not a string, a tuple that starts with a string, or a list, and a text oid that is not ASCII are errors. It never guesses: a caller must not read such a record as a record without references. Its tests compare it with `referencesf` on the corpus and on hand-built records.
- `persistent_ids` does not read the tree. A small stack machine runs over the opcodes, as the `noload` of `referencesf` does, and keeps tuples and lists in arenas, so a deep value cannot overflow the call stack.
- `decode_class_name` decodes the class pickle alone and stops at its end, so a large state costs nothing.
- A memo get resolves to the latest put of its index before it, across the class pickle and the state pickle. A put inside an opaque node hides the value of its index.
- The decoder refuses the bytes that Python's unpickler refuses as malformed, for example an unknown protocol or a BINSTRING or LONG4 length of 2³¹ or more. It can refuse more, for example a pickle that leaves values on the stack at STOP. An error that Python finds only when it calls a class or a function, for example REDUCE with arguments that are not a tuple, stays inside an opaque node.

## Known differences from Python

- A dict key that Python cannot hash, such as a list, stays modelled. Python refuses such a record when it loads it, so no store holds one.
