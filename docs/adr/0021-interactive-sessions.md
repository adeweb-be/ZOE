# 0021. Interactive sessions instead of automated loops

Status: accepted

## Context

The working methods planned a conductor: a GitHub App and a service that started agent runs on board events (build, review, verify, merge, document), with leases, cost and concurrency caps, a digest and a kill switch. The owner works with the agents in interactive sessions and wants nothing to run unattended.

## Decision

There are no automated loops and no conductor. A session takes the roles in turn: it builds one card on its branch, starts a fresh reviewer session for each pull request, and leaves red-zone approval and every merge to the human. The role briefs stay in `loops/prompts/`. The cards that would have built the loops are closed.

## Consequences

- Leases, the kill switch, the digest and the cost cap are gone, because nothing runs without the human.
- The human is in every merge, so the throughput is the throughput of the sessions.
- Unattended loops can come back later as a new decision, with its own cards.
