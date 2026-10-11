# zoe-mcp

The MCP server for ops and content agents. Specification: "AI-native: MCP server and retrieval". ADR 0028 records `zoe mcp`.

## Layout

- `src/lib.rs`: `serve_stdio`, which opens a store folder and serves MCP on stdin and stdout.
- `src/ops.rs`: the read-only ops tools `object_get`, `history`, `diff` and `references`.

## Invariants

- The server runs as a principal. Every content result goes through the roles bitmap before ranking.
- Objects outside the catalog are visible to the ops role only.
- Ops tools are read-only. `pack` and `backup` need the read-write role.
- An anonymous principal never receives a restricted passage.
- `zoe mcp` runs as the ops principal of the local operator, with the read-only role. It offers no tool that writes, and it does not offer `pack` or `backup`.
- The store opens with Loupe's `Folder::open`, so with `Store::open_read_only`. The server holds the lock of the store while it runs.
- The ops tools are Loupe's functions with an agent face. Each tool calls `zoe-loupe` and never reads the store or decodes a record itself. The JSON is Loupe's: `object_get` gives Loupe's `Object`. `history`, `diff` and `references` give an object with the oid and Loupe's list, because the structured content of a result is a JSON object. Oids and tids are ZODB-form strings, as Loupe writes them.
- An error of a tool is a result with `isError` and a message, and the session goes on. This covers a bad id, an oid without a record, bad arguments (rmcp gives them as tool results) and a panic in a tool. A message gives oids and tids in ZODB form.
- rmcp answers the rest of the protocol, and the session goes on after each: an unknown tool gets the JSON-RPC error -32602, a `tools/call` without params gets -32601, a message of the wrong shape gets -32600, and a line that is not JSON gets no reply.
- The session ends with success when the client closes stdin, also before the `initialize` request. A first message that is not `initialize`, such as a notification, fails the session.
- `references` gives Loupe's `Object.references`: every strong reference of the record, as ZODB's `referencesf` and pack read them, the ones inside opaque nodes too. A record whose state or references do not decode is an error, not an empty list, because its references are not known. `object_get` shows the refusal in `error`, as Loupe does.
- The session runs on a tokio runtime with one thread. Each tool runs on the blocking pool, because the store is synchronous.
- stdout carries only MCP messages. Nothing else in the process writes to it.
- This crate uses tokio and rmcp. Only the feature `serve` of `zoe-server` depends on it, so the extension module never links it: `cargo tree -p zoe-py -e normal` lists no tokio and no rmcp.

## Tests

- `crates/zoe-server/tests/mcp.rs` is the acceptance test of `zoe mcp`, over stdio.
