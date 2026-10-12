# 0004. No Python on the server; three conflict tiers

Status: accepted

## Context

A ZEO server resolves conflicts by unpickling application classes, so it must carry every Plone egg and pays three unpickles and one pickle per conflicting object per retry. Some classes define their own `_p_resolveConflict`, which only Python can run.

ZEO 6 already lets the client resolve. With the `client-conflict-resolution` option, a ZEO server lists each conflict in the reply of `vote`, and every stock `ZEO.ClientStorage` resolves it and votes again, with no negotiation (`docs/spikes/zeo-wire.md`).

## Decision

The server is a single static binary with no Python inside. Conflicts go through three tiers: structural merges in Rust for BTree buckets, sets and `Length`; field-level merges in Rust where the merge policy allows them; and the long tail resolved on the client, where the class already lives. For the long tail, `zoe serve` lists the conflict in the reply of `vote` as ZEO 6 does, for every client. It keeps the per-OID locks between the two votes. The locks stay held until `tpc_finish`, `tpc_abort`, a failed vote or the end of the connection. What no tier resolves raises `ConflictError` as today.

## Consequences

- The storage host needs no Python and no Plone eggs.
- Native merges must equal the Python resolvers byte for byte; differential tests enforce it.
- A stock client gets all three tiers.
- A conflict resolved on the client costs more round trips: two `loadSerial` calls, the store of the resolved state and a second vote. The locks of the transaction stay held during them, so another transaction on these oids waits, at most `lock-timeout`. A client that stops between the two votes and keeps its connection keeps its locks until the connection ends.
