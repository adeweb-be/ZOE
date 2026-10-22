# MCP

`zoe mcp` gives an AI agent read-only tools on one store, over the Model Context Protocol. The agent can read an object, its history and the difference between two revisions, measure the space of the store, run the check of `zoe verify`, and explain a transaction. No tool writes to the store.

`zoe mcp` is a subcommand of the static binary. The console script of the wheel does not have it (see [Install](install.md)).

## Run it

The MCP client of the agent starts the command and talks to it on stdin and stdout:

```sh
zoe mcp --path /var/lib/zoe
```

Most MCP clients take the command and its arguments in their configuration, for example:

```json
{
  "mcpServers": {
    "zoe": {
      "command": "/usr/local/bin/zoe",
      "args": ["mcp", "--path", "/var/lib/zoe"]
    }
  }
}
```

- `--blob-dir DIR` gives the blob directory when it is not `blobs` in the store directory. The tools `verify` and `space` read the blob files.
- The command opens the store read-only and holds its lock while it runs. So stop the Zope instance or the server first, or give the agent a copy of the store, such as a restore of a backup.
- A store that a writer left open gets a repair at the start, and the command writes one line to stderr that holds `repaired the store`.
- The command ends when the client closes stdin.

## The tools

| Tool | What the agent gets |
| --- | --- |
| `object_get` | one object, by oid or by path from the Zope application, as decoded JSON. With `as_of`, the object as it was after a transaction. |
| `history` | the revisions of an object, newest first |
| `diff` | the changes of the state of an object between two revisions |
| `references` | the objects that an object refers to |
| `space` | the space of the store, by class, and its largest records |
| `verify` | the check of `zoe verify`, with at most 100 findings in each list |
| `explain_conflict` | a transaction: its user, its description and each object that it wrote, with the revisions just before and after it |

The prompts `explain_transaction`, `why_conflict` and `summarise_changes` give the agent a request with the data of these tools.

Oids and tids are strings in ZODB form, such as `0x00` or `0x03f0b0c0d0e0f000`. A result holds no pickle bytes. The user and the description of a transaction show at most 4 KiB of the record. A longer text ends with `…`.

The store keeps no record of conflicts. `explain_conflict` and `why_conflict` read the history of the objects, and the agent infers the conflict from it.

[MCP tools](../reference/mcp.md) gives the arguments and the results of each tool.

## Not built yet

`zoe mcp` runs on a store folder only. It does not run inside `zoe serve` or against a server, and it has no tools that change content.
