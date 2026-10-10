# 0005. ZODB 6 and ZEO 6 only

Status: accepted

## Context

Plone 6.0 sites still run on the ZODB 5 and ZEO 5 lines. Supporting them doubles the conformance matrix and the protocol edge cases.

## Decision

ZOE targets ZODB 6.0 or later and ZEO 6.0 or later, which means Plone 6.1 or later.

## Consequences

- One protocol version and one set of conformance suites.
- A site on Plone 6.0 upgrades Plone before it moves to ZOE.
