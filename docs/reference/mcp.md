# MCP tools

`zoe mcp --path STORE` serves the read-only ops tools and prompts of one store to an agent, over MCP on stdin and stdout (see [The zoe command](cli.md#zoe-mcp)). The server is named `zoe`. ADR 0028 records the command.

- Every tool is read-only. No tool writes to the store, and the server offers no pack and no backup.
- The command opens the store read-only and holds its lock while it runs. It repairs a store that a writer left open, and writes one line to stderr that holds `repaired the store`.
- Oids and tids are strings in ZODB form, such as `0x00` or `0x03f0b0c0d0e0f000`.
- A path starts at `root["Application"]`, such as `/plone/folder/doc`.
- Each tool gives its JSON as the structured content of the result, and as text.
- Each list of a result has a limit, and the result gives the full count next to it. No result holds pickle bytes. Only `object_get` and `diff` give a decoded state.
- An error of a tool is a result with `isError` and a message, and the session goes on.

## Tools

| Tool | Arguments | Result |
| --- | --- | --- |
| `object_get` | `oid` or `path`; `as_of`, a tid, optional | One object as decoded JSON: its class, oid, serial, size, state and outbound references, and the refusal of the decoder when the record does not decode. With `as_of`, the object as it was after the transaction of that tid. |
| `history` | `oid` | The revisions of the object, newest first, at most 4,096: the tid, the user and the description of each transaction, and the size of the record. A deletion has size 0. |
| `diff` | `oid`, `tid_a`, `tid_b` | The changes of the decoded state between the state after `tid_a` and the state after `tid_b`: for each change, a JSON pointer into the state, the value before and the value after. |
| `references` | `oid` | The strong outbound persistent references of the object as of the last transaction, as ZODB's `referencesf` gives them: for each one, the oid that it leads to and a JSON pointer into the state. |
| `space` | `largest`, optional: 20 by default, at most 1000 | The measure of the store as of its last transaction: for each class the current records and their bytes, the blob files, the largest current records, and the bytes that each of the newest 100 transactions wrote. The call reads every current record. |
| `verify` | none | The check of `zoe verify`: the keys of `zoe verify --format json` (`checksums`, `blobs`, `references`, `pickles`, `classes`, `unknown_classes`), with oids and tids in ZODB form, and `findings`, `totals` and `walked`. Each list holds at most 100 findings. The call reads the whole store. |
| `explain_conflict` | `tid` | The transaction of `tid`: its user and description, and each object that it wrote, at most 100, with its class and size. For each object, `before` and `after` are the revisions just before and just after the transaction. The store keeps no record of conflicts: this is the history only. |

## Prompts

Each prompt gives one user message: a request, and the JSON of its tools in a fenced block.

| Prompt | Arguments | What it carries |
| --- | --- | --- |
| `explain_transaction` | `tid` | the result of `explain_conflict` for `tid` |
| `why_conflict` | `oid` | the class and the newest 32 revisions of the object |
| `summarise_changes` | `since`, a tid | at most 50 transactions after `since`, oldest first, each with the count of its records by class and at most 16 of its objects |

A bad argument of a prompt is the JSON-RPC error -32602. An error of the store is -32603.

## The user and the description of a transaction

`history`, `explain_conflict` and the prompts give the user and the description of each transaction as text, with the same rule as Loupe:

- The text holds at most 4 KiB (4,096 bytes) of the record.
- A longer text is cut before a character, never inside a UTF-8 character, and ends with `…`.
- Bytes that are not UTF-8 become U+FFFD.
