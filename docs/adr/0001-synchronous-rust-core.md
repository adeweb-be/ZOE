# 0001. A synchronous Rust core

Status: accepted. A decision of the human sets where async lives outside the server skin: tokio is allowed in `zoe-client`, `zoe-bench`, `zoe-mcp` and the server mode of Loupe, and stays out of `zoe-core` and `zoe-py`.

## Context

ZOE runs in two shapes: in-process inside one Zope instance, and as a server for a cluster. The ZODB object model is synchronous: a ghost loads inside `__getattribute__`, where nothing can await. Correctness (the data model, two-phase commit, locks, conflicts, pack, blobs) must be the same in both shapes.

Other crates talk to a network or to an agent: the Rust client of the ZEO protocol, the benchmark, the MCP server, which is built on rmcp and so runs on tokio, and the server mode of Loupe.

## Decision

One crate, `zoe-core`, owns correctness. It is synchronous Rust with no tokio and no Python inside. The in-process storage and the server are thin skins that call it the same way. Async lives in the server skin (`zoe-server`), which runs core calls on a blocking pool, and in the crates that talk to a network or to an agent: `zoe-client`, `zoe-bench`, `zoe-mcp` and the server mode of Loupe. The extension module (`zoe-py`) never links tokio: `cargo tree -p zoe-py -e normal` lists none.

## Consequences

- A fix to the core fixes both shapes; there is no second implementation to keep in step.
- The core is testable without a runtime or an interpreter, and deterministic simulation only needs clock and I/O traits.
- The server pays a thread hop per storage call; prefetch and pipelining hide it.
- Async clients get awaitable wrappers around the core, never async code inside it.
