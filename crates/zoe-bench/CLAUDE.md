# zoe-bench

A load generator that speaks the ZEO protocol. It measures what `zodbshootout` cannot: contention and server saturation. Specification: "Performance and testing harness".

## Invariants

- Public CI runs only on synthetic corpora. Production data never enters this crate, its fixtures or its outputs.
- Results are JSON, one file per run, with the reference machine described in it.
