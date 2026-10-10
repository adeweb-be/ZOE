# 0006. CPython 3.12 to 3.14 and 3.14t, on Linux x86-64

Status: accepted

## Context

The wheel, the server binary and Loupe must be built and tested on every target. Free-threaded CPython is officially supported from 3.14; 3.13t was experimental. The first adopting sites run Linux servers.

## Decision

The targets are CPython 3.12, 3.13 and 3.14, plus the free-threaded 3.14t build, on Linux x86-64. The extension module declares that it does not use the GIL. 3.13t is not a target; 3.15 joins when it ships; macOS and Windows come in a later phase.

## Consequences

- The CI matrix has four interpreters and one platform.
- Free-threaded parallel commits in one process also need `persistent`, `BTrees` and `zodbpickle` to declare free-threading support upstream.
