# 0022. Red zones: the on-disk format, pack and the process files

Status: accepted

## Context

The first red-zone list held two-phase commit, the lock manager, the on-disk format, the pickle encoder, the protocol layer, security code, the environment and CI. Thirteen of the twenty-five phase 1 cards touched one of these paths, so the human approved most merges, and the human became the bottleneck that the working methods warn about. Most of these paths have strong checks without the human: acceptance tests that the builder cannot edit, the crash simulation, TSan and a fresh reviewer.

## Decision

The red zones are the changes that cannot be undone and the files that govern the agents:

- the on-disk format, `crates/zoe-core/src/format/`, and any change to the data model wherever its code lives;
- pack, `crates/zoe-core/src/pack/`, because it deletes data;
- `.github/` (the workflows and `CODEOWNERS`) and `loops/` (the role briefs), which only the human changes.

Two-phase commit, the lock manager, the pickle encoder, the protocol layer, security code, `ci/` and `devenv.nix` merge on the reviewer agent's approval.

## Consequences

- Four phase 1 cards are red zones: blobs, compression, pack and sequences.
- The stop-and-ask rules stay: a builder still stops before it changes the on-disk format, the wire protocol, a public Python API or a trait that another crate uses.
- The reviewer checks commit and lock changes against `crates/zoe-core/CLAUDE.md` with no human behind it, so that file must hold every invariant.
- A path can come back into the red zone through a new ADR, for example the protocol layer once the server serves real clients.
