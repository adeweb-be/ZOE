# zoe-pickle

Decode and encode ZODB records (the class pickle and the state pickle) as a generic tree. Specification: "Loupe", section "Pickle layer".

Red zone in this crate: `src/encode/`.

## Invariants

- Decode then encode gives the same bytes, for every record of the corpus.
- A construct that the tree does not model becomes an opaque node that re-emits its original bytes. An edit inside an opaque node is refused.
- Class names come from reading the class pickle, never from unpickling state.
- The encoder writes protocol-3 pickles with ZODB's `persistent_id` convention.
