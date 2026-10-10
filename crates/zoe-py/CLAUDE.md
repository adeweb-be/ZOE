# zoe-py

The Python extension module `zoe._zoe`, built by maturin into the `zoedb` wheel. Specification: "In-process mode".

## Invariants

- The module declares that it does not use the GIL, so that the GIL stays off when CPython 3.14t loads it.
- Every call into the core releases the GIL (on free-threaded builds, it holds nothing).
- `zoe.Storage` is thread-safe and all Connections of a process share it.
- The wheel never imports Plone. Only `zoe.plone`, through the `plone` extra, does.
- The version of the wheel equals the version of the crates.
- A change to a public Python API is a stop-and-ask.
