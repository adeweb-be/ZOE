# zoe-convert

Converters between `Data.fs`, RelStorage and ZOE stores. Specification: "Loupe", section "Converters", and F14.

## Invariants

- A conversion copies tids, oids and pickles unchanged.
- A conversion can resume from a tid.
- Every conversion verifies the transaction count, the last tid and a sample of records byte for byte, and writes a report.
- The RelStorage reader refuses a schema version that it does not know.
