# 0001. A synchronous Rust core, and where async lives

Status: accepted.

## Context

ZOE runs in two shapes: in-process inside one Zope instance, and as a server for a cluster. The ZODB object model is synchronous: a ghost loads inside `__getattribute__`, where nothing can await. Correctness (the data model, two-phase commit, locks, conflicts, pack, blobs) must be the same in both shapes.

Other crates talk to a network or to an agent: the Rust client of the ZEO protocol, the benchmark, the MCP server, which is built on rmcp and so runs on tokio, and the server mode of Loupe.

F28 asks for `zoe.zeo.ClientStorage` as a drop-in for `ZEO.ClientStorage`, built on `zoe-client`. `zoe-client` is async: it runs on tokio, through its trait `Runtime`, so that the simulator of N18 can run it. The wheel builds one extension from `zoe-py`, and the names of the crates and of the PyPI package are fixed. Three places can hold the binding of the native client:

- a second extension module in the `zoedb` wheel, built from `zoe-client` behind a cargo feature, with a second build step;
- `zoe-py` links `zoe-client`, and tokio comes into `zoe._zoe`;
- a part of `zoe-client` without tokio that `zoe-py` drives with threads of the standard library, while the other users keep the tokio driver.

## Decision

### The core is synchronous

One crate, `zoe-core`, owns correctness. It is synchronous Rust with no tokio and no Python inside. The in-process storage and the server are thin skins that call it the same way.

### Where async lives

Async lives in the server skin (`zoe-server`), which runs core calls on a blocking pool, and in the crates that talk to a network or to an agent: `zoe-client`, `zoe-bench`, `zoe-mcp` and the server mode of Loupe. tokio stays out of `zoe-core`.

### The extension links zoe-client and tokio, never rmcp

**`zoe-py` links `zoe-client`.** The native client is the class `zoe._zoe.Client`, in the one extension of the wheel. So tokio is in `zoe._zoe`. `zoe-py` uses the features `rt-multi-thread` and `time` of tokio, beside the features that `zoe-client` takes.

**The threads start at the first open.** The import of `zoe._zoe` starts no thread. The first client that opens builds one tokio runtime for the process, with 2 worker threads named `zoe-client`. Every client of the process shares it. Each call of a client waits for the server inside `py.detach`, so it holds no GIL.

**Fork.** A child of `os.fork()` has no thread of the runtime of its parent. A client that the parent opened raises `RuntimeError` at its next call in the child, and its drop there frees nothing of the connection. A client that the child opens builds a runtime of the child.

**rmcp stays out.** `zoe-py` depends on `zoe-server` without the feature `serve` (ADR 0028). So `cargo tree -p zoe-py -e normal` lists tokio, and does not list rmcp. The console script has no server and no MCP server.

## Consequences

- A fix to the core fixes both shapes; there is no second implementation to keep in step.
- The core is testable without a runtime or an interpreter, and deterministic simulation only needs clock and I/O traits.
- The server pays a thread hop per storage call; prefetch and pipelining hide it.
- Async clients get awaitable wrappers around the core, never async code inside it.
- The wheel keeps one extension and one build step.
- The native client and the in-process storage share the error mapping and the GIL rules of `zoe-py`.
- The extension is larger by tokio and `zoe-client`.
- A process that never opens a native client runs no thread of tokio.
- `zoe-client` keeps its trait `Runtime`: the extension gives it `Tokio`, and the simulator its own.

## Alternatives

- **An extension without tokio.** The native client of F28 runs on `zoe-client`, which is async, so it needs one of the two options below.
- **A second extension module.** The wheel would hold two compiled modules, and maturin would need a second build step for the second one. The proof of concept of the server performance card used one, outside the wheel.
- **A driver of `zoe-client` without tokio.** It splits the client in two drivers that must stay in step, and the simulator would test only one of them.

## Merged ADRs

- ADR 0035, the extension links zoe-client and tokio for the native client: Decision, "The extension links zoe-client and tokio, never rmcp"; its alternatives are in Alternatives.
