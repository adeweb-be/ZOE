# 0004. No Python on the server; three conflict tiers

Status: accepted

## Context

A ZEO server resolves conflicts by unpickling application classes, so it must carry every Plone egg and pays three unpickles and one pickle per conflicting object per retry. Some classes define their own `_p_resolveConflict`, which only Python can run.

## Decision

The server is a single static binary with no Python inside. Conflicts go through three tiers: structural merges in Rust for BTree buckets, sets and `Length`; field-level merges in Rust where the merge policy allows them; and the long tail resolved on the client through the `client_resolve` extension, where the class already lives. What no tier resolves raises `ConflictError` as today.

## Consequences

- The storage host needs no Python and no Plone eggs.
- Native merges must equal the Python resolvers byte for byte; differential tests enforce it.
- A stock client without `client_resolve` gets tiers 1 and 2 only.
