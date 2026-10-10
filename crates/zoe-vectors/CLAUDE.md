# zoe-vectors

Native retrieval: passages, embeddings, the `VectorIndex` trait and hybrid search. Specification: "AI-native: MCP server and retrieval".

## Invariants

- Security filtering by the roles bitmap happens before ranking, in every mode and with every backend.
- A result never contains a passage of an object that does not exist at the tid of the query.
- The default embedding provider is local. A remote provider is opt-in per site, logged, and counted in `zoe_vectors_remote_bytes_total`.
- Passages are versioned by tid, so a rebuild needs no Plone.
