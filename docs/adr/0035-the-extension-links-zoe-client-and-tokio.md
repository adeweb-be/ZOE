# 0035. The extension links zoe-client and tokio for the native client

Status: accepted. The human decided it on the card of the walking skeleton of the native client (#301). It supersedes the rule of ADR 0001 and of ADR 0028 that the extension module `zoe._zoe` never links tokio. The rule for rmcp stays.

## Context

F28 asks for `zoe.zeo.ClientStorage` as a drop-in for `ZEO.ClientStorage`, built on `zoe-client`. `zoe-client` is async: it runs on tokio, through its trait `Runtime`, so that the simulator of N18 can run it. ADR 0001 and ADR 0028 kept tokio out of the extension: `cargo tree -p zoe-py -e normal` listed neither tokio nor rmcp. The wheel builds one extension from `zoe-py`, and the names of the crates and of the PyPI package are fixed.

Three places could hold the binding:

- a second extension module in the `zoedb` wheel, built from `zoe-client` behind a cargo feature, with a second build step;
- `zoe-py` links `zoe-client`, and tokio comes into `zoe._zoe`;
- a part of `zoe-client` without tokio that `zoe-py` drives with threads of the standard library, while the other users keep the tokio driver.

## Decision

**`zoe-py` links `zoe-client`.** The native client is the class `zoe._zoe.Client`, in the one extension of the wheel. So tokio is in `zoe._zoe`. `zoe-py` uses the features `rt-multi-thread` and `time` of tokio, beside the features that `zoe-client` takes.

**The threads start at the first open.** The import of `zoe._zoe` starts no thread. The first client that opens builds one tokio runtime for the process, with 2 worker threads named `zoe-client`. Every client of the process shares it. Each call of a client waits for the server inside `py.detach`, so it holds no GIL.

**Fork.** A child of `os.fork()` has no thread of the runtime of its parent. A client that the parent opened raises `RuntimeError` at its next call in the child, and its drop there frees nothing of the connection. A client that the child opens builds a runtime of the child.

**rmcp stays out.** `zoe-py` depends on `zoe-server` without the feature `serve`, as before. So `cargo tree -p zoe-py -e normal` lists tokio, and does not list rmcp. The console script still has no server and no MCP server.

## Consequences

- The wheel keeps one extension and one build step.
- The native client and the in-process storage share the error mapping and the GIL rules of `zoe-py`.
- The extension is larger by tokio and `zoe-client`.
- A process that never opens a native client runs no thread of tokio.
- `zoe-client` keeps its trait `Runtime`: the extension gives it `Tokio`, and the simulator its own.

## Alternatives

- **A second extension module.** The wheel would hold two compiled modules, and maturin would need a second build step for the second one. The proof of concept of the server performance card used one, outside the wheel.
- **A driver of `zoe-client` without tokio.** It splits the client in two drivers that must stay in step, and the simulator would test only one of them.
