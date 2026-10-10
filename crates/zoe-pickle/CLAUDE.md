# zoe-pickle

Decode and encode ZODB records (the class pickle and the state pickle) as a generic tree. Specification: "Loupe", section "Pickle layer".

## Invariants

- Decode then encode gives the same bytes, for every record of the corpus and for every record that the decoder accepts.
- A construct that the tree does not model becomes an opaque node that re-emits its original bytes. An edit inside an opaque node is refused.
- Class names come from reading the class pickle, never from unpickling state.
- The encoder writes the opcode form that each node keeps, also the protocol-0 and protocol-1 forms of Python 2 stores. It never picks another form. Nodes for new values use protocol 3 with ZODB's `persistent_id` convention.
- Decode, encode, drop, clone, comparison and debug never call themselves once per nesting level. They use explicit stacks in the heap. Any new operation on a tree does the same.
- The decoder copies the bytes of each opaque span once: it records spans while it runs and copies the spans that remain at the end.
- A record has a node budget, `NODE_BUDGET`. The decoder refuses a record that needs more nodes.
- A memo get resolves to the latest put of its index before it, across the class pickle and the state pickle. A put inside an opaque node hides the value of its index.
- The decoder refuses the bytes that Python's unpickler refuses as malformed, for example an unknown protocol or a BINSTRING or LONG4 length of 2³¹ or more. It can refuse more, for example a pickle that leaves values on the stack at STOP. An error that Python finds only when it calls a class or a function, for example REDUCE with arguments that are not a tuple, stays inside an opaque node.
