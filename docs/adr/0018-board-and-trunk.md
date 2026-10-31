# 0018. The process: the board, trunk-based development, interactive sessions, narrow red zones and the licence

Status: accepted.

## Context

The human and the agent sessions need one place that holds the state of every card. Agents work in parallel and must not step on each other's changes.

The owner works with the agents in interactive sessions and wants nothing to run unattended.

A red zone needs the approval of the human. A wide list of red zones makes the human the bottleneck that the working methods warn about. With a list that holds two-phase commit, the lock manager, the on-disk format, the pickle encoder, the protocol layer, security code, the environment and CI, thirteen of the twenty-five phase 1 cards touched a red zone, and the human approved most merges. Most of these paths have strong checks without the human: acceptance tests that the builder cannot edit, the crash simulation and TSan.

ZOE is Rust and Python, with a Plone extra. The Rust ecosystem's norm is Apache-2.0 or MIT. The project wants to stay open to the Plone Foundation and to any adopter.

## Decision

### The board and trunk-based development

Cards are GitHub issues on a Projects board with the columns Backlog, Ready, Building, Review, Verifying, Human and Done. The board holds the state of every card. Work is trunk-based: one short-lived branch `card/<id>` per card, one agent per worktree, squash merges, no force pushes.

### Interactive sessions

There are no automated loops and no conductor. A session builds cards, each on its branch. A pull request merges by squash when CI is green; the human approves the red zones and answers the questions on cards. The role briefs are in `loops/prompts/`. No card builds the loops.

### Red zones

The red zones are the changes that cannot be undone and the files that govern the agents:

- the on-disk format, `crates/zoe-core/src/format/`, and any change to the data model wherever its code lives;
- pack, `crates/zoe-core/src/pack/`, because it deletes data;
- `.github/` (the workflows and `CODEOWNERS`) and `loops/` (the role briefs), which only the human changes.

Two-phase commit, the lock manager, the pickle encoder, the protocol layer, security code, `ci/` and `devenv.nix` merge when CI is green.

### The licence

Everything in the repository is licensed Apache-2.0, the Plone extra included.

## Consequences

- Every merged line traces back to one card and one pull request.
- No lease, kill switch, digest or cost cap exists, because nothing runs without the human.
- The human is in a merge only for a red zone, so the throughput is the throughput of the sessions and of CI.
- Unattended loops can come back later as a new decision, with its own cards.
- Of the phase 1 cards, four are red zones: blobs, compression, pack and sequences.
- The stop-and-ask rules stay: a builder still stops before it changes the on-disk format, the wire protocol, a public Python API or a trait that another crate uses.
- No one checks commit and lock changes against `crates/zoe-core/CLAUDE.md` but the builder and the tests, so that file must hold every invariant and the tests must cover it.
- A path can come back into the red zone through a new ADR, for example the protocol layer once the server serves real clients.
- One licence file at the root covers every crate and package.
- Contributions arrive as pull requests under that licence.

## Alternatives

- **A conductor**: a GitHub App and a service that start agent runs on board events (build, review, verify, merge, document), with leases, cost and concurrency caps, a digest and a kill switch. The owner wants nothing to run unattended.
- **A fresh reviewer agent and a verifier on each pull request**, with a claims-and-evidence table and a merge train. They added a session and a wait to every merge.
- **The wide red-zone list** (two-phase commit, the lock manager, the on-disk format, the pickle encoder, the protocol layer, security code, the environment and CI). The human then approves most merges and becomes the bottleneck.

## Merged ADRs

- ADR 0011, Apache-2.0 for everything: Decision, "The licence".
- ADR 0021, interactive sessions instead of automated loops: Decision, "Interactive sessions"; the conductor is in Alternatives.
- ADR 0022, red zones: the on-disk format, pack and the process files: Decision, "Red zones"; the wide list is in Alternatives.
