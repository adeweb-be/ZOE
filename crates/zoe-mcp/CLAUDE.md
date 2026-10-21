# zoe-mcp

The MCP server for ops agents: read-only ops tools and prompts on a store folder. The content tools, the roles and the read-write tools of the specification are not built. Specification: "AI-native: MCP server and retrieval". ADR 0028 records `zoe mcp`.

## Layout

- `src/lib.rs`: `serve_stdio`, which opens a store folder with the blob directory of the store and serves MCP on stdin and stdout.
- `src/ops.rs`: the read-only ops tools `object_get`, `history`, `diff`, `references`, `space`, `verify` and `explain_conflict`.
- `src/txn.rs`: the transactions as `explain_conflict` and the prompts give them.
- `src/prompts.rs`: the prompts `explain_transaction`, `why_conflict` and `summarise_changes`.

## Invariants

- Every tool is read-only. `zoe mcp` offers no tool that writes, and it does not offer `pack` or `backup`.
- The store opens with Loupe's `Folder::open_with_blob_dir`, so with `Store::open_read_only_repairing` and `Options::blob_dir`. The store does not record its blob directory, so `serve_stdio` takes it: `--blob-dir` of `zoe mcp`, `blobs` in the folder by default. On a store that was closed cleanly, the open creates nothing and changes no byte. A store that a writer left open gets its repair at the open (ADR 0032), and `serve_stdio` writes one line to stderr that holds `repaired the store`, never to stdout. A failed repair is an `Open` error, and the command exits with 1. The server holds the lock of the store while it runs.
- The ops tools are Loupe's functions with an agent face. `object_get`, `history`, `diff`, `references` and `space` call `zoe-loupe`, and the JSON is Loupe's: `object_get` gives Loupe's `Object`. `history`, `diff` and `references` give an object with the oid and Loupe's list, because the structured content of a result is a JSON object. `space` gives Loupe's `Space`. The other reads go through the read-only store of the folder, `Folder::store`: `verify` calls `zoe_convert::verify`, and `explain_conflict` and the prompts read the transactions with the iterator of the core and the neighbours with Loupe's `history`. A class comes from the class pickle alone (`decode_class_name`), as in Loupe's space screen. No tool decodes a state itself. Oids and tids are ZODB-form strings, as Loupe writes them.
- Each list of a result has a limit, and the result gives the full count next to it. `space` gives the newest 100 transactions of the growth and `transactions`, and at most 1000 largest records (20 by default). `verify` gives at most 100 findings in each list, with `totals` and `findings`. `explain_conflict` gives at most 100 objects, with `records`. Only `object_get` and `diff` give a decoded state. No result gives pickle bytes.
- `verify` gives the keys of `zoe verify --format json` (`checksums`, `blobs`, `references`, `pickles`, `classes`, `unknown_classes`), with oids and tids in ZODB form, and adds `findings`, `totals` and `walked`. It takes no list of classes, so `unknown_classes` is empty. A store with blob records whose blob directory has no layout file is a tool error, as for `zoe verify`, and its text names the blob directory and `--blob-dir`.
- `explain_conflict(tid)` reads the history only, because the store keeps no record of conflicts. It gives each oid that the transaction wrote, with its class and size (a deletion has size 0 and no class), and the revisions of that oid just before and just after the transaction, from Loupe's `history`. Loupe's history holds the newest `HISTORY_LIMIT` revisions only. When it does not hold the tid, or when the tid is the last revision of a full history, the object has `history_cut`, and a neighbour that is null is not known. A null `before` without `history_cut` means that the transaction created the object. A tid without a transaction is a tool error.
- Each prompt gives one user message: a request and the JSON that its tools give. `explain_transaction(tid)` carries the result of `explain_conflict`. `why_conflict(oid)` carries the class and the newest 32 revisions of the object, from `history`. `summarise_changes(since)` carries at most 50 transactions after `since`, oldest first, each with the count of its records by class (`<deleted>` for a deletion) and at most 16 of its objects, and `more` when the store has more. The JSON is in a fenced block whose fence is longer than each run of backticks in it, so a text of the store cannot close it. A bad argument of a prompt (an id that is not in ZODB form, an oid without a record, a tid without a transaction) is the JSON-RPC error -32602; an error of the store is -32603.
- `verify` takes no argument. An argument, such as a list of classes, is a tool error.
- An error of a tool is a result with `isError` and a message, and the session goes on. This covers a bad id, an oid without a record, bad arguments (rmcp gives them as tool results) and a panic in a tool. A message gives oids and tids in ZODB form.
- rmcp answers the rest of the protocol, and the session goes on after each: an unknown tool gets the JSON-RPC error -32602, a `tools/call` without params gets -32601, a message of the wrong shape gets -32600, and a line that is not JSON gets no reply.
- The session ends with success when the client closes stdin, also before the `initialize` request. A request before `initialize` gets a reply, and the session waits for `initialize`. A notification as the first message fails the session.
- `references` gives Loupe's `Object.references`: every strong reference of the record, as ZODB's `referencesf` and pack read them, the ones inside opaque nodes too. A record whose state or references do not decode is an error, not an empty list, because its references are not known. `object_get` shows the refusal in `error`, as Loupe does.
- The session runs on a tokio runtime with one thread. Each tool runs on the blocking pool, because the store is synchronous.
- stdout carries only MCP messages. Nothing else in the process writes to it.
- This crate uses tokio and rmcp. Only the feature `serve` of `zoe-server` depends on it, so the extension module never links it: `cargo tree -p zoe-py -e normal` lists no rmcp. It lists tokio, which comes through `zoe-client` for the native client (ADR 0035).

## Tests

- `crates/zoe-server/tests/mcp.rs` is the acceptance test of `zoe mcp`, over stdio.
- `crates/zoe-server/tests/mcp_ops.rs` is the acceptance test of `space`, `verify`, `explain_conflict` and the prompts, over stdio.
