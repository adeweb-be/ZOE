# 0032. A tool that reads a store repairs it when a writer left it open

Status: accepted. The human decided it. It supersedes the part of ADR 0028 that says that a read-only open changes no byte, and that a store that a crash left open does not open read-only.

## Context

redb sets a recovery flag in the header of its file when a writer opens the database. Only a clean close of the `Database` clears the flag, and it writes the allocator state first. A writer that dies by a signal, by SIGKILL or by a power loss leaves the flag and no allocator state. Every commit that redb acknowledged is in the file, but the read-only database of redb needs the allocator state, and it fails with `RepairAborted` without it. The core gives `NotClosedCleanly` for it. Only a writer open repairs the file: redb walks every page and rebuilds the allocator state.

Each tool that reads a store opens it read-only: Loupe folder mode, `zoe mcp`, `zoe verify` (with `--backup` too), `zoe convert --to-data-fs`, `zoe pack --dry-run`, `zoe backup` and the read-only stores of `zoe serve`. ADR 0028 said that such an open changes no byte and creates nothing, and that a store that a crash left open fails with `NotClosedCleanly` until a writer opens it once. So after a crash, when the operator most needs to inspect a store, every tool refused it. A run of Plone, Loupe and `zoe mcp` on a converted store showed it.

The tests of a clean stop of `zoe serve` use the strict read-only open: when it accepts a store, the stop closed the store cleanly. A read-only open that repairs would make these tests pass on a store that a stop left open.

## Decision

**A tool that reads a store repairs it.** When a writer left the store open, the tool repairs it, then opens it read-only. It writes to the store only then. On a store that was closed cleanly, it changes no byte and creates nothing, as before.

**A new open of the core: `Store::open_read_only_repairing(dir, options)`.** It takes the lock of the store, as the read-only open does, and holds it from its start to its end. So no other open comes between the repair and the read-only open. On a clean store, it is `Store::open_read_only_with`. When the read-only open of redb fails with `RepairAborted`, and only then, it opens `zoe.redb` for writes with `redb::Builder::open`, which creates no file. redb repairs the file at that open. The call drops that database, which writes the allocator state and clears the recovery flag. Then it opens the database read-only and makes the store as `open_read_only_with` does. The repair uses `Options::cache_size` as its page cache. `Store::repaired` tells the caller that the open repaired the store.

**`Store::open_read_only` stays strict.** It changes no byte, and it refuses a store that a writer left open with `NotClosedCleanly`. The tests of a clean stop keep it.

**The repair writes only `zoe.redb`.** It is the repair that a writer open of redb runs, and nothing more. It runs no recovery of the store: it removes no `zoe.redb.new` and no file of `blobs/tmp/` or `staging/`, replays no intent file, syncs no directory, and writes nothing in the blob directory or in its parent. The next writer open does these, as before. It keeps every commit that was durable before the writer died. It changes no key space, `meta` key or frame field.

**A failed repair is `RepairFailed`.** A failure of the open for writes or of the repair fails the call with `ZoeError::RepairFailed`. Its text says that the store was not closed cleanly and that its repair failed, and gives the error of the system: `Read-only file system (os error 30)` on a read-only file system, `Permission denied (os error 13)` for a file without write permission. The open for writes fails before redb writes a byte, so such a failure changes no byte. Another holder of the lock fails the call with `Locked` before any write. zoe-py maps `RepairFailed` to `StorageError`.

**Each tool says that it repaired the store.** The core logs nothing for a repair: it tells the caller. A repair writes to the files of the operator, so each tool that repaired a store writes one line to stderr. The line says that the store was not closed cleanly and that the tool repaired it, and holds the stable phrase `repaired the store`, for example `zoe verify: /srv/store: the store was not closed cleanly: repaired the store`. The commands write it as a message of the command. `zoe serve` writes it as a WARN log line that names the storage. `zoe mcp` writes it to stderr, never to stdout, which carries only MCP messages. Loupe shows the same text in its window. A tool that opened a clean store, or that failed to repair it, writes no such line.

**Each tool gives a failed repair.** The commands write the text of `RepairFailed` as their message on stderr and exit with 1. `zoe serve` fails its start with 1, and `zoe mcp` exits with 1. Loupe gives the failure `notRepaired` with the text, and its UI string says that Loupe must write the database file once to repair the store.

## Consequences

- After a crash, every tool opens the store. The first one repairs it, and the next ones find a clean store.
- A repair reads every page of the database file. So it takes as long as the first writer open after a kill (the known limit of zoe-core). On a large store, the first tool waits for it. It shows no progress and cannot stop.
- A repair needs write access to the database file. A store on a read-only file system, or a store whose files the user cannot write, fails with `RepairFailed`. Copy it to a place where the tool can write, or open it once with a writer.
- Loupe writes to the folder of a store that a writer left open: the database file, once, at the open. It writes nothing to a clean store.
- The kill loop of the crash harness runs `zoe verify` after each stop of the server. `zoe verify` now repairs a store that a stop left open, so the kill loop no longer proves a clean stop. The tests that use `Store::open_read_only` after a stop still prove it.
- `zoe.Storage(read_only=True)` does not change: it opens the store for writes already, so it repairs as a writer does.
- The scratch copy of `zoe verify --backup` is a store that the command restores and closes itself, so the strict open stays there.

## Alternatives

- **A repairing `Store::open_read_only`.** It would make the tests of a clean stop pass on a store that a stop left open.
- **A writer open, a close, then a read-only open, in each tool.** Only the core holds the lock of the store across the two opens, and a writer open of the core runs the whole recovery of the store: it removes files in `blobs/tmp/` and `staging/` and replays the intent files. A tool that reads should write as little as it can.
- **A save of the allocator state on each commit** (the quick repair of redb). Each commit would write more. The human declined it.
- **A read-only open that rebuilds the allocator state in memory.** It writes nothing, but it needs a change of redb.
